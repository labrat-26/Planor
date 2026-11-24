use super::*;
use crate::backend::{Win32Backend, WindowBackend};
use std::collections::BTreeMap;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_SHOW,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART,
};
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct Saved {
    pub pid: u32,
    pub cookie: usize,
    pub flags: u32,
    pub show: u32,
    pub min: [i32; 2],
    pub max: [i32; 2],
    pub normal: [i32; 4],
    pub rect: [i32; 4],
    pub topmost: bool,
}
pub(crate) struct Journal {
    pub path: PathBuf,
    pub entries: BTreeMap<isize, Saved>,
    defer_save: bool,
    dirty_save: bool,
}
impl Journal {
    pub(crate) fn open(path: PathBuf) -> Result<Self> {
        let entries = match fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path,
            entries,
            defer_save: false,
            dirty_save: false,
        })
    }
    pub(crate) fn begin_batch(&mut self) {
        self.defer_save = true;
    }
    pub(crate) fn end_batch(&mut self) -> Result<()> {
        self.defer_save = false;
        if std::mem::take(&mut self.dirty_save) {
            self.save()?;
        }
        Ok(())
    }
    pub(crate) fn save(&mut self) -> Result<()> {
        if self.defer_save {
            self.dirty_save = true;
            return Ok(());
        }
        let _span = perf::SpanGuard::new(perf::SpanKind::DaemonJournal);
        if self.entries.is_empty() {
            if self.path.exists() {
                fs::remove_file(&self.path)?
            }
            return Ok(());
        }
        let tmp = self.path.with_extension("tmp");
        let f = File::create(&tmp)?;
        serde_json::to_writer(&f, &self.entries)?;
        // Atomic write+rename protects against daemon crash/kill; window HWNDs do not survive OS
        // reboot, so per-save FlushFileBuffers (10-21 ms) only stalls window-open events.
        drop(f);
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
    pub(crate) fn capture(&mut self, id: isize, backend: &impl WindowBackend) -> Result<()> {
        if self
            .entries
            .get(&id)
            .is_some_and(|s| s.cookie == backend.get_recovery_cookie(id))
        {
            return Ok(());
        }
        let Some(info) = backend.get_window_placement(id) else {
            return Err("GetWindowPlacement / GetWindowRect failed".into());
        };
        let cookie = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos() as usize)
            | 1;
        self.entries.insert(
            id,
            Saved {
                pid: info.pid,
                cookie,
                flags: info.flags,
                show: info.show,
                min: info.min,
                max: info.max,
                normal: info.normal,
                rect: info.rect,
                topmost: info.topmost,
            },
        );
        self.save()?;
        backend.set_recovery_cookie(id, cookie)?;
        Ok(())
    }
    pub(crate) fn restore(&mut self, backend: &impl WindowBackend) -> Result<()> {
        self.defer_save = false;
        self.dirty_save = false;
        self.entries.retain(|&id, s| {
            if backend.get_window_pid(id) != Some(s.pid)
                || backend.get_recovery_cookie(id) != s.cookie
            {
                return false;
            }
            if !backend.restore_window_placement(id, s) {
                return true;
            }
            backend.remove_recovery_cookie(id);
            false
        });
        self.save()?;
        if self.entries.is_empty() {
            Ok(())
        } else {
            Err("window restoration incomplete; retained recovery journal".into())
        }
    }
}
use planor::{Gesture, Rect, Strip, Swipe};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct HotCfg {
    pub gap_px: i32,
    pub first_pair_split: bool,
    pub default_tile_share: f64,
    pub peek_enabled: bool,
    pub peek_width_px: i32,
    pub stack_peek_px: Option<i32>,
    pub stack_zone_percent: i32,
    pub spawn_on_cursor_monitor: bool,
    pub focus_follows_mouse: bool,
    pub warp_cursor_to_focused: bool,
    pub drag_stack_modifier_vk: i32,
    pub border_width_px: f32,
    pub border_radius_px: f32,
    pub focused_color: Option<u32>,
    pub stacked_color: Option<u32>,
    pub pinned_color: Option<u32>,
    pub drop_target_color: Option<u32>,
    pub group_hover_color: Option<u32>,
    pub anim_enabled: bool,
    pub anim_duration_ms: u64,
    pub anim_spring: bool,
    pub anim_placeholder: bool,
    pub anim_scroll: bool,
    pub anim_open_close: bool,
    pub anim_resize: bool,
    pub anim_drag_reflow: bool,
    pub anim_stack_switch: bool,
    pub three_finger_enabled: bool,
    pub axis_lock_deadzone: f64,
    pub swipe_step_px: f64,
    pub swipe_natural_direction: bool,
    pub swipe_scroll_gain: f64,
    pub swipe_inertia: bool,
    pub swipe_drag_gain: f64,
    pub swipe_drag_accel: f64,
    pub swipe_drag_cross_monitors: bool,
    pub swipe_fling_enabled: bool,
    pub swipe_fling_speed: f64,
    pub win_wheel_step_px: f64,
    pub max_log_bytes: u64,
    pub max_log_files: usize,
}

fn parse_hex_color(v: &toml::Value) -> Option<u32> {
    v.as_str()
        .and_then(|s| u32::from_str_radix(s.trim_start_matches('#'), 16).ok())
}

impl HotCfg {
    pub(crate) fn from_toml(cfg: &toml::Value) -> Self {
        let layout = &cfg["layout"];
        let borders = &cfg["borders"];
        let animation = &cfg["animation"];
        let touchpad = &cfg["touchpad"];
        let mouse = &cfg["mouse"];
        let diagnostics = &cfg["diagnostics"];
        let borders_enabled = borders["enabled"].as_bool() == Some(true);
        let color = |k: &str| {
            if borders_enabled {
                parse_hex_color(&borders[k])
            } else {
                None
            }
        };
        Self {
            gap_px: layout["gap_px"].as_integer().unwrap_or(6) as i32,
            first_pair_split: layout["first_pair_split"].as_bool().unwrap_or(true),
            default_tile_share: layout["default_tile_share"].as_float().unwrap_or(0.5),
            peek_enabled: layout["peek_enabled"].as_bool() == Some(true),
            peek_width_px: layout["peek_width_px"].as_integer().unwrap_or(48) as i32,
            stack_peek_px: layout["stack_peek_px"].as_integer().map(|v| v as i32),
            stack_zone_percent: layout["stack_zone_percent"].as_integer().unwrap_or(50) as i32,
            spawn_on_cursor_monitor: layout["spawn_on_cursor_monitor"].as_bool() == Some(true),
            focus_follows_mouse: layout["focus_follows_mouse"].as_bool() == Some(true),
            warp_cursor_to_focused: layout["warp_cursor_to_focused"].as_bool() == Some(true),
            drag_stack_modifier_vk: match layout["drag_stack_modifier"].as_str() {
                Some("Ctrl") => 17,
                Some("Alt") => 18,
                _ => 16,
            },
            border_width_px: borders["width_px"].as_integer().unwrap_or(2) as f32,
            border_radius_px: borders["corner_radius_px"].as_integer().unwrap_or(8) as f32,
            focused_color: color("focused_color"),
            stacked_color: color("stacked_color"),
            pinned_color: color("pinned_color"),
            drop_target_color: color("drop_target_color"),
            group_hover_color: color("group_hover_color"),
            anim_enabled: animation["enabled"].as_bool() == Some(true),
            anim_duration_ms: animation["duration_ms"].as_integer().unwrap_or(180) as u64,
            anim_spring: animation["curve"].as_str() == Some("spring"),
            anim_placeholder: animation["placeholder"].as_bool() != Some(false),
            anim_scroll: animation["scroll"].as_bool() == Some(true),
            anim_open_close: animation["open_close"].as_bool() == Some(true),
            anim_resize: animation["resize"].as_bool() == Some(true),
            anim_drag_reflow: animation["drag_reflow"].as_bool() == Some(true),
            anim_stack_switch: animation["stack_switch"].as_bool() == Some(true),
            three_finger_enabled: touchpad["three_finger_enabled"].as_bool() == Some(true),
            axis_lock_deadzone: touchpad["axis_lock_deadzone"].as_integer().unwrap_or(12) as f64,
            swipe_step_px: touchpad["swipe_step_px"].as_integer().unwrap_or(60) as f64,
            swipe_natural_direction: touchpad["swipe_natural_direction"].as_bool() == Some(true),
            swipe_scroll_gain: touchpad["swipe_scroll_gain"].as_float().unwrap_or(1.),
            swipe_inertia: touchpad["swipe_inertia"].as_bool() == Some(true),
            swipe_drag_gain: touchpad["swipe_drag_gain"].as_float().unwrap_or(1.),
            swipe_drag_accel: touchpad["swipe_drag_accel"].as_float().unwrap_or(0.),
            swipe_drag_cross_monitors: touchpad["swipe_drag_cross_monitors"].as_bool() == Some(true),
            swipe_fling_enabled: touchpad["swipe_fling_enabled"].as_bool() == Some(true),
            swipe_fling_speed: touchpad["swipe_fling_speed"].as_integer().unwrap_or(900) as f64,
            win_wheel_step_px: mouse["win_wheel_step_px"].as_integer().unwrap_or(240) as f64,
            max_log_bytes: (diagnostics["max_log_mb"].as_integer().unwrap_or(25) as u64) * 1024 * 1024,
            max_log_files: diagnostics["max_log_files"].as_integer().unwrap_or(3) as usize,
        }
    }

    #[inline]
    pub(crate) fn reveal_peek_px(self) -> i32 {
        if self.peek_enabled {
            self.peek_width_px + self.gap_px
        } else {
            0
        }
    }

    #[inline]
    pub(crate) fn anim_action_enabled(self, action: &str) -> bool {
        match action {
            "scroll" => self.anim_scroll,
            "open_close" => self.anim_open_close,
            "resize" => self.anim_resize,
            "drag_reflow" => self.anim_drag_reflow,
            "stack_switch" => self.anim_stack_switch,
            _ => false,
        }
    }
}
use win32::Event;
type Key = (u128, String);
pub(crate) struct GestureRec {
    pub until: Instant,
    pub out: PathBuf,
    pub started_us: i64,
    pub meta: serde_json::Value,
    /// Recorded lines (contacts, buttons, step marks), each with `t` in microseconds from the start.
    pub events: Vec<serde_json::Value>,
    /// Record only: Planor does not act on the gestures (a guided session must not move windows).
    pub passive: bool,
}
impl GestureRec {
    fn push(&mut self, mut v: serde_json::Value) {
        v["t"] = serde_json::json!(perf::qpc_us() - self.started_us);
        self.events.push(v);
    }
}
pub(crate) struct DragSession {
    pub id: isize,
    pub key: Key,
    pub original: Rect,
    pub point: (i32, i32),
    pub sampled: Instant,
    pub hover: Instant,
    pub target: Option<isize>,
    pub mode: planor::Drag,
    pub destination: Option<Key>,
    pub resizing: bool,
    pub moved: bool,
    pub pad_drag: bool,
    pub edge_scroll_at: Option<Instant>,
    pub edge_scroll_frac: f64,
    pub saw_lbutton_down: bool,
    pub lbutton_up_since: Option<Instant>,
    pub visuals_cleared: bool,
    pub movesize_exit_since: Option<Instant>,
}
/// A window being carried by a four-finger touchpad swipe. Planor moves it itself (there is no OS move
/// loop without a mouse button) and moves the cursor with it, so the normal drag logic sees an
/// ordinary drag.
pub(crate) struct PadDrag {
    pub id: isize,
    /// Cursor position minus the window's top-left corner, fixed for the whole drag.
    pub grab: (f64, f64),
    pub cursor: (f64, f64),
    /// Union of all monitors (left, top, right, bottom): the cursor, and so the window, never leaves it.
    pub limits: (f64, f64, f64, f64),
    pub seen: Instant,
    /// Smoothed finger speed (units/s) for the acceleration; a raw per-report speed is noisy, which made
    /// the same swipe move the window by different amounts.
    pub speed: f64,
    /// When the real window was last moved: moves are rate-limited so they cannot queue up behind a busy app.
    pub moved: Instant,
    /// Recent raw finger deltas, newest last, for the lift-off velocity (fling).
    pub samples: std::collections::VecDeque<(Instant, f64, f64)>,
}
pub(crate) struct Engine<B: WindowBackend> {
    pub backend: B,
    pub pad: Option<PadDrag>,
    pub pad_scrolling: bool,
    /// When set, a finished touchpad scroll settles (reveal + focus) at this time unless scrolling resumes.
    pub settle: Option<Instant>,
    pub scroll_frac: f64,
    pub renderer: crossbeam_channel::Sender<render::Message>,
    pub action: &'static str,
    pub cfg: toml::Value,
    pub hot: HotCfg,
    pub journal: Journal,
    pub strips: BTreeMap<Key, Strip>,
    pub displays: Vec<(String, Rect, Rect)>,
    pub displays_arc: Arc<[(String, Rect, Rect)]>,
    pub desktop: Option<u128>,
    pub active: String,
    pub pinned: HashSet<isize>,
    pub orphans: HashSet<isize>,
    /// Monitor each orphan lived on before its monitor was detached. `pull-orphans` may move the window onto
    /// another monitor; when its own monitor is attached again it goes back there.
    pub homes: HashMap<isize, String>,
    pub skipped: HashSet<isize>,
    pub paused: bool,
    pub locked: bool,
    pub deadline: Option<Instant>,
    pub dirty: bool,
    pub instant: bool,
    pub force_place: bool,
    pub flung: bool,
    pub spawn_hint: Option<String>,
    pub quick: bool,
    pub last_holes: Vec<Rect>,
    pub last_holes_arc: Arc<[Rect]>,
    pub last_tile_geometry: Vec<(isize, Rect, String, bool, bool, bool, bool, Rect)>,
    pub last_had_zones: bool,
    pub overlay_seq: u64,
    pub last_foreground: isize,
    pub null_fg_ticks: u8,
    pub holes_checked: Instant,
    pub monitors_checked: Instant,
    pub desktop_checked: Instant,
    pub scroll_samples: std::collections::VecDeque<(Instant, f64, bool)>,
    pub scroll_start_focus: Option<(Key, isize)>,
    pub scroll_dpi: Option<f64>,
    pub wheel_mode: Option<(Key, Instant, bool, bool)>,
    pub inertia: Option<(bool, f64, Instant, Option<f64>)>,
    pub v_travel: f64,
    /// A cross-axis swipe already hopped to another monitor: the rest of that gesture is ignored.
    pub hopped: bool,
    /// Newly managed windows and how many times they were put back after moving themselves (apps restore
    /// their own saved position just after they appear).
    pub born: HashMap<isize, (Instant, u8)>,
    /// `planor gestures record`: every touchpad frame until the deadline, written to the file afterwards.
    pub gesture_rec: Option<GestureRec>,
    pub gesture: Gesture,
    pub seq: u64,
    pub input: Option<Instant>,
    pub input_qpc_us: Option<i64>,
    pub started: Instant,
    pub config_version: u64,
    pub zoomed: HashSet<isize>,
    pub drag: Option<DragSession>,
    pub last_scan: Instant,
    pub placed: HashMap<isize, (u64, Rect)>,
    pub covering: HashMap<isize, Instant>,
    pub max_refresh_hz: f64,
    pub diag: diag::Telemetry,
}
impl<B: WindowBackend> Engine<B> {
    #[cfg(test)]
    pub(crate) fn new_for_test(
        backend: B,
        cfg: toml::Value,
        journal: Journal,
        renderer: crossbeam_channel::Sender<render::Message>,
    ) -> Self {
        let diag_enabled = cfg["diagnostics"]["enabled"].as_bool() == Some(true);
        let desktop = backend.current_desktop();
        let max_refresh_hz = backend.max_refresh_hz();
        let hot = HotCfg::from_toml(&cfg);
        Self {
            backend,
            pad: None,
            pad_scrolling: false,
            settle: None,
            scroll_frac: 0.,
            renderer,
            action: "open_close",
            cfg,
            hot,
            journal,
            diag: diag::Telemetry::new(diag_enabled),
            strips: Default::default(),
            displays: vec![],
            displays_arc: Arc::from([]),
            desktop,
            active: String::new(),
            pinned: Default::default(),
            orphans: Default::default(),
            homes: Default::default(),
            skipped: Default::default(),
            paused: false,
            locked: false,
            deadline: None,
            dirty: false,
            instant: false,
            force_place: false,
            flung: false,
            spawn_hint: None,
            quick: false,
            last_holes: vec![],
            last_holes_arc: Arc::from([]),
            last_tile_geometry: vec![],
            last_had_zones: false,
            overlay_seq: 0,
            last_foreground: 0,
            null_fg_ticks: 0,
            holes_checked: Instant::now(),
            monitors_checked: Instant::now(),
            desktop_checked: Instant::now(),
            scroll_samples: Default::default(),
            scroll_start_focus: None,
            scroll_dpi: None,
            wheel_mode: None,
            inertia: None,
            v_travel: 0.,
        hopped: false,
        born: HashMap::new(),
        gesture_rec: None,
            gesture: Default::default(),
            seq: 0,
            input: None,
            input_qpc_us: None,
            started: Instant::now(),
            config_version: 1,
            zoomed: Default::default(),
            drag: None,
            last_scan: Instant::now(),
            placed: Default::default(),
            covering: HashMap::new(),
            max_refresh_hz,
        }
    }
    #[cfg(test)]
    pub(crate) fn expire_timers_for_test(&mut self) {
        let past = Instant::now() - Duration::from_secs(5);
        if self.deadline.is_some() {
            self.deadline = Some(past);
        }
        self.holes_checked = past;
        self.monitors_checked = past;
        self.desktop_checked = past;
        self.last_scan = past;
    }
    fn key(&self) -> Option<Key> {
        Some((self.desktop?, self.active.clone()))
    }
    fn owning(&self, id: isize) -> Option<Key> {
        self.strips
            .iter()
            .find(|(_, s)| s.locate(id).is_some())
            .map(|(k, _)| k.clone())
    }
    /// Returns the managed tile HWND corresponding to `fg` (either `fg` itself or its `GA_ROOTOWNER`
    /// when `fg` is a dialog/popup owned by a managed tile), or `None` for unmanaged foregrounds.
    fn managed_foreground_id(&self, fg: isize) -> Option<isize> {
        if fg == 0 {
            return None;
        }
        if self.owning(fg).is_some() {
            return Some(fg);
        }
        let root = self.backend.root_owner(fg);
        if root != 0 && root != fg && self.owning(root).is_some() {
            Some(root)
        } else {
            None
        }
    }
    fn blocked(&self) -> bool {
        self.paused || self.locked || self.deadline.is_some() || self.desktop.is_none()
    }
    fn reveal(&mut self) {
        let width = self.hot.reveal_peek_px();
        if let Some(s) = self.key().and_then(|k| self.strips.get_mut(&k)) {
            s.reveal(width)
        }
        self.dirty = true
    }
    fn mark_cover(&mut self, id: isize) {
        if self.hot.anim_placeholder {
            self.covering.insert(id, Instant::now());
        }
    }
    fn add(&mut self, id: isize) {
        self.add_with_frame(id, None, true)
    }
    fn add_with_frame(&mut self, id: isize, precomputed_rect: Option<Rect>, cover_newborn: bool) {
        if self.blocked()
            || self.owning(id).is_some()
            || self.skipped.contains(&id)
            || !self.backend.eligible(id, &self.cfg)
        {
            return;
        }
        if !self.backend.on_current_desktop(id) {
            return;
        }
        let hint = self.spawn_hint.take();
        let Some(rect) = precomputed_rect.or_else(|| self.backend.frame(id)) else {
            return;
        };
        // A new window goes to the monitor under the cursor (setting `spawn_on_cursor_monitor`),
        // otherwise to the monitor it mostly overlaps.
        let Some((name, area, _)) = hint
            .and_then(|n| self.displays.iter().find(|m| m.0 == n).cloned())
            .or_else(|| {
                self.displays
                    .iter()
                    .rev()
                    .max_by_key(|(_, _, b)| {
                        ((rect.x + rect.w).min(b.x + b.w) - rect.x.max(b.x)).max(0) as i64
                            * ((rect.y + rect.h).min(b.y + b.h) - rect.y.max(b.y)).max(0) as i64
                    })
                    .cloned()
            })
        else {
            return;
        };
        if self.journal.capture(id, &self.backend).is_err() {
            self.skipped.insert(id);
            return;
        }
        self.active = name.clone();
        self.action = "open_close";
        let s = self
            .strips
            .entry((self.desktop.unwrap(), name))
            .or_insert_with(|| Strip::new(area));
        s.gap = self.hot.gap_px;
        s.split = self.hot.first_pair_split;
        s.share = self.hot.default_tile_share;
        s.insert(id, self.backend.minimum(id, area.horizontal()));
        self.born.retain(|_, (t, _)| t.elapsed() < Duration::from_secs(3));
        self.born.insert(id, (Instant::now(), 0));
        if cover_newborn {
            self.mark_cover(id);
        }
        self.reveal()
    }
    /// A window that just appeared and then moved itself (apps restore their saved position after showing,
    /// "new window struggling to keep the position"): put it back in its slot at once. Only for the first
    /// 2.5 s and at most 6 times, so an app that insists cannot start a tug of war.
    fn hold_newborn(&mut self, id: isize, r: Rect) {
        let Some((t, n)) = self.born.get_mut(&id) else { return };
        if t.elapsed() > Duration::from_millis(2500) || *n >= 6 {
            self.born.remove(&id);
            return;
        }
        if self.drag.is_some() || self.pad.is_some() || self.pad_scrolling {
            return;
        }
        let Some(&(_, want)) = self.placed.get(&id) else { return };
        let park = self.displays.iter().map(|m| m.2.x + m.2.w).max().unwrap_or(0) + 1024;
        let off = |a: i32, b: i32| (a - b).abs() > 2;
        if r.x != park && (off(r.x, want.x) || off(r.y, want.y) || off(r.w, want.w) || off(r.h, want.h)) {
            *n += 1;
            if self.diag.enabled {
                diag::trace(format!("NEWBORN {id} moved itself to {r:?}, back to {want:?} (#{n})"));
            }
            self.instant = true;
            self.force_place = true;
            self.dirty = true;
        }
    }
    pub(crate) fn command(
        &mut self,
        cmd: Command,
        reload: &crossbeam_channel::Sender<toml::Value>,
    ) -> Result<String> {
        match cmd {
            Command::Gestures { action: crate::GestureAction::Mark { text } } => {
                return Ok(match self.gesture_rec.as_mut() {
                    Some(r) => {
                        r.push(serde_json::json!({ "kind": "mark", "text": text }));
                        "marked".into()
                    }
                    None => "not recording".into(),
                });
            }
            Command::Gestures { action: crate::GestureAction::Replay { file, step, gap_ms } } => {
                use planor::replay::{fits, identify, parse_events_jsonl, segments};
                let text = std::fs::read_to_string(&file)?;
                let segs: Vec<_> = segments(&parse_events_jsonl(&text))
                    .into_iter()
                    .filter(|s| step.as_ref().is_none_or(|w| s.label.to_lowercase().contains(&w.to_lowercase())))
                    .filter(|s| {
                        let g = identify(s);
                        s.label.is_empty() || fits(&s.label, &g) || (!s.previous.is_empty() && fits(&s.previous, &g))
                    })
                    .collect();
                let n = segs.len();
                // On its own thread, through the same event bus as the real touchpad, at the recorded pace.
                std::thread::spawn(move || {
                    for s in segs {
                        let t0 = s.frames.first().map(|f| f.0).unwrap_or(0);
                        let start = Instant::now();
                        for (t, p) in s.frames {
                            let due = start + Duration::from_micros((t - t0).max(0) as u64);
                            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                                std::thread::sleep(wait);
                            }
                            crate::win32::emit(Event::Contacts(if p.len() >= 3 { p } else { vec![] }));
                        }
                        crate::win32::emit(Event::Contacts(vec![]));
                        std::thread::sleep(Duration::from_millis(gap_ms));
                    }
                });
                return Ok(format!("replaying {n} gestures"));
            }
            Command::Gestures { action: crate::GestureAction::Stop } => {
                let n = self.gesture_rec.as_ref().map(|r| r.events.len());
                self.write_gesture_recording();
                return Ok(match n {
                    Some(n) => format!("stopped, {n} events written"),
                    None => "not recording".into(),
                });
            }
            Command::Gestures { action: crate::GestureAction::Guide { .. } | crate::GestureAction::Analyze { .. } } => return Ok("runs in the client".into()),
            Command::Gestures { action: crate::GestureAction::Record { name, seconds, out, passive } } => {
                let Some(out) = out else { return Ok("no output path".into()) };
                let cursor = self.backend.cursor_pos();
                let monitor = self.displays.iter().find(|m| contains(m.2, cursor)).or(self.displays.first());
                let t = &self.cfg["touchpad"];
                self.gesture_rec = Some(GestureRec {
                    until: Instant::now() + Duration::from_secs(seconds.clamp(1, 600)),
                    out: out.clone(),
                    started_us: perf::qpc_us(),
                    meta: serde_json::json!({
                        "kind": "meta",
                        "schema": planor::replay::REPLAY_SCHEMA_VERSION,
                        "name": name,
                        "recorded": diag::stamp(),
                        "monitor": monitor.map(|m| serde_json::json!({
                            "name": m.0, "horizontal": m.2.horizontal(),
                            "bounds": [m.2.x, m.2.y, m.2.w, m.2.h],
                        })),
                        "axis_lock_deadzone": t["axis_lock_deadzone"].as_integer(),
                        "swipe_scroll_gain": t["swipe_scroll_gain"].as_float(),
                        "swipe_drag_gain": t["swipe_drag_gain"].as_float(),
                        "swipe_natural_direction": t["swipe_natural_direction"].as_bool(),
                    }),
                    events: vec![],
                    passive,
                });
                crate::input::RECORD_ALL.store(true, std::sync::atomic::Ordering::Relaxed);
                return Ok(format!("recording touchpad frames to {}", out.display()));
            }
            Command::Stop => {
                self.clear_scroll_state();
                self.paused = true;
                self.clear()?;
                self.journal.restore(&self.backend)?
            }
            Command::Reload => {
                self.cfg = self.backend.load_config()?;
                self.hot = HotCfg::from_toml(&self.cfg);
                self.config_version += 1;
                perf::set_diag_enabled(self.cfg["diagnostics"]["enabled"].as_bool() == Some(true));
                let _ = reload.send(self.cfg.clone());
                for s in self.strips.values_mut() {
                    s.gap = self.hot.gap_px;
                }
                self.force_place = true;
                self.reveal()
            }
            Command::Pause => {
                self.clear_scroll_state();
                self.paused = true;
                self.clear()?
            }
            Command::Resume => {
                self.paused = false;
                self.scan()
            }
            Command::Pin if !self.blocked() => self.pin(),
            Command::PullOrphans if !self.blocked() => {
                let Some(dest) = self.key() else {
                    return Err("unknown desktop".into());
                };
                let Some((_, area, _)) = self.displays.iter().find(|m| m.0 == dest.1).cloned()
                else {
                    return Err("unknown monitor".into());
                };
                let ids: Vec<_> = self
                    .orphans
                    .iter()
                    .copied()
                    .filter(|id| self.owning(*id).is_some_and(|k| k.0 == dest.0))
                    .collect();
                for id in ids {
                    if let Some(k) = self.owning(id) {
                        self.strips.get_mut(&k).unwrap().remove(id)
                    }
                    self.orphans.remove(&id);
                    self.backend.show_no_activate(id);
                    // Stays in `homes`: it goes back to its own monitor once that one is attached again.
                    let minimum = self.backend.minimum(id, area.horizontal());
                    self.home_strip(dest.clone(), area).insert(id, minimum);
                }
                self.reveal()
            }
            Command::Key { name } if name == "panic_exit" => {
                self.paused = true;
                self.clear()?;
                self.journal.restore(&self.backend)?;
            }
            Command::Key { name } if !self.blocked() => {
                let valid = matches!(
                    name.as_str(),
                    "focus_left"
                        | "focus_right"
                        | "focus_up"
                        | "focus_down"
                        | "move_left"
                        | "move_right"
                        | "stack_toggle"
                        | "toggle_pin"
                        | "focus_monitor_next"
                        | "focus_monitor_prev"
                );
                if !valid {
                    return Err(format!("unknown key action: {name}").into());
                }
                self.input = Some(Instant::now());
                self.input_qpc_us = Some(perf::qpc_us());
                self.keypress(&name);
                if !self.dirty {
                    self.input = None;
                    self.input_qpc_us = None;
                }
            }
            Command::State { json } => return self.state_snapshot(json),
            Command::Perf {
                reset,
                trace,
                seconds,
                json,
                ..
            } => {
                if reset {
                    perf::reset();
                    return Ok("ok".into());
                }
                if trace.is_some() {
                    return Ok(serde_json::to_string(&perf::export_chrome_trace(seconds))?);
                }
                let frame_telemetry = self
                    .diag
                    .command(Diagnostic::Live)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
                let report = perf::build_report(frame_telemetry);
                return if json {
                    Ok(serde_json::to_string_pretty(&report)?)
                } else {
                    Ok(perf::format_summary(&report))
                };
            }
            Command::Diag { action } => return self.diag.command(action),
            _ => return Err("command unavailable".into()),
        }
        Ok("ok".into())
    }
    /// Rectangles of unmanaged app windows on a monitor. Test daemons share the desktop with the developer's
    /// own windows, which they treat as unmanaged; carving those out would hide every border they draw, so an
    /// isolated instance carves out only windows of the class named in PLANOR_HOLES (none if unset).
    pub(crate) fn state_snapshot(&self, json: bool) -> Result<String> {
        let park_x = self.displays.iter().map(|m| m.2.x + m.2.w).max().unwrap_or(0) + 1024;
        let stack_peek = self.cfg["layout"]["stack_peek_px"].as_integer().unwrap_or(28) as i32;
        let monitors: Vec<serde_json::Value> = self
            .displays
            .iter()
            .map(|(name, work, bounds)| {
                serde_json::json!({
                    "name": name,
                    "work": work,
                    "bounds": bounds,
                    "horizontal": bounds.horizontal(),
                })
            })
            .collect();

        let mut strips_json = Vec::new();
        let mut tiles_json = Vec::new();
        let mut parked_json = Vec::new();

        for ((desk, name), strip) in &self.strips {
            if Some(*desk) != self.desktop {
                continue;
            }
            let cols: Vec<serde_json::Value> = strip
                .columns
                .iter()
                .enumerate()
                .map(|(idx, col)| {
                    serde_json::json!({
                        "index": idx,
                        "members": col.members,
                        "active_index": col.active,
                        "active_id": col.members.get(col.active).copied().unwrap_or(0),
                        "length": col.length,
                        "minimum": col.minimum(),
                        "frac": col.frac,
                    })
                })
                .collect();
            strips_json.push(serde_json::json!({
                "desktop": format!("{desk:032x}"),
                "monitor": name,
                "area": strip.area,
                "gap": strip.gap,
                "share": strip.share,
                "split": strip.split,
                "focus": strip.focus,
                "offset": strip.offset,
                "fullscreen_id": strip.fullscreen_id(),
                "columns": cols,
            }));

            let mon_bounds = self
                .displays
                .iter()
                .find(|m| &m.0 == name)
                .map(|m| m.2)
                .unwrap_or(strip.area);
            for (id, mut rect) in strip.rects(stack_peek) {
                let Some((ci, member_idx)) = strip.locate(id) else {
                    continue;
                };
                let fullscreen = strip.fullscreen_id() == Some(id);
                if fullscreen {
                    if strip.area.horizontal() {
                        rect.y = mon_bounds.y;
                        rect.h = mon_bounds.h;
                    } else {
                        rect.x = mon_bounds.x;
                        rect.w = mon_bounds.w;
                    }
                }
                let bound = if fullscreen { mon_bounds } else { strip.area };
                let preview = strip.columns[ci].members[strip.columns[ci].active] != id;
                let hidden = !strip.columns[ci].shown(member_idx);
                let clipped = rect.x < bound.x
                    || rect.y < bound.y
                    || rect.x + rect.w > bound.x + bound.w
                    || rect.y + rect.h > bound.y + bound.h;
                let parked = hidden || preview || clipped;
                let os_rect = self.backend.frame(id);
                let foreground = self.backend.foreground();
                let focused_id = self.managed_foreground_id(foreground);
                let color = if fullscreen {
                    None
                } else if strip.columns[ci].members.len() > 1 && !preview {
                    self.cfg["borders"]["stacked_color"].as_str()
                } else if focused_id == Some(id) && name == &self.active && strip.active() == Some(id) {
                    self.cfg["borders"]["focused_color"].as_str()
                } else {
                    None
                };
                tiles_json.push(serde_json::json!({
                    "id": id,
                    "monitor": name,
                    "column_index": ci,
                    "expected_rect": rect,
                    "bound": bound,
                    "os_rect": os_rect,
                    "color": color,
                    "fullscreen": fullscreen,
                    "preview": preview,
                    "hidden": hidden,
                    "pinned": false,
                    "parked": parked,
                }));
                if parked {
                    let reason = if hidden {
                        "hidden"
                    } else if preview {
                        "preview"
                    } else {
                        "clipped"
                    };
                    parked_json.push(serde_json::json!({
                        "id": id,
                        "monitor": name,
                        "expected_rect": rect,
                        "os_rect": os_rect,
                        "reason": reason,
                    }));
                }
            }
        }

        for &id in &self.pinned {
            let os_rect = self.backend.frame(id);
            let monitor = os_rect
                .and_then(|r| {
                    self.displays
                        .iter()
                        .find(|(_, _, b)| {
                            r.x >= b.x && r.x < b.x + b.w && r.y >= b.y && r.y < b.y + b.h
                        })
                        .map(|m| m.0.clone())
                })
                .unwrap_or_default();
            let rect = os_rect.unwrap_or_default();
            tiles_json.push(serde_json::json!({
                "id": id,
                "monitor": monitor,
                "column_index": serde_json::Value::Null,
                "expected_rect": rect,
                "bound": rect,
                "os_rect": os_rect,
                "color": self.cfg["borders"]["pinned_color"].as_str(),
                "fullscreen": false,
                "preview": false,
                "hidden": false,
                "pinned": true,
                "parked": false,
            }));
        }

        let mut pinned: Vec<isize> = self.pinned.iter().copied().collect();
        pinned.sort_unstable();
        let mut orphans: Vec<isize> = self.orphans.iter().copied().collect();
        orphans.sort_unstable();
        let homes: BTreeMap<String, String> = self
            .homes
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();

        if !json {
            let mut lines = Vec::new();
            lines.push(format!(
                "Planor state: uptime={}ms cfg_v={} seq={} paused={} locked={} test_mode={} active={} park_x={}",
                self.started.elapsed().as_millis(),
                self.config_version,
                self.seq,
                self.paused,
                self.locked,
                crate::test_mode(),
                self.active,
                park_x
            ));
            lines.push(format!(
                "monitors={} strips={} tiles={} parked={} pinned={} orphans={}",
                monitors.len(),
                strips_json.len(),
                tiles_json.len(),
                parked_json.len(),
                pinned.len(),
                orphans.len()
            ));
            for ((desk, name), s) in &self.strips {
                if Some(*desk) != self.desktop {
                    continue;
                }
                lines.push(format!(
                    "  strip {} cols={} focus={} offset={} area={:?}",
                    name,
                    s.columns.len(),
                    s.focus,
                    s.offset,
                    s.area
                ));
            }
            return Ok(lines.join("\n"));
        }

        let payload = serde_json::json!({
            "schema_version": 1,
            "uptime_ms": self.started.elapsed().as_millis() as u64,
            "config_version": self.config_version,
            "pid": std::process::id(),
            "instance": crate::instance(),
            "test_mode": crate::test_mode(),
            "paused": self.paused,
            "locked": self.locked,
            "seq": self.seq,
            "active_desktop": self.desktop.map(|d| format!("{d:032x}")),
            "active_monitor": self.active,
            "park_x": park_x,
            "stalls": perf::stall_count(),
            "monitors": monitors,
            "strips": strips_json,
            "tiles": tiles_json,
            "parked": parked_json,
            "pinned": pinned,
            "orphans": orphans,
            "homes": homes,
        });
        Ok(serde_json::to_string_pretty(&payload)?)
    }
    fn holes(&self) -> Vec<Rect> {
        let _span = perf::SpanGuard::new(perf::SpanKind::DaemonHoles);
        let Some(only) = self.backend.holes_class_filter() else {
            return vec![];
        };
        let park_x = self.displays.iter().map(|m| m.2.x + m.2.w).max().unwrap_or(0) + 1024;
        let mut managed_above: Vec<Rect> = Vec::new();
        let mut out: Vec<Rect> = Vec::new();
        // `windows()` yields top-to-bottom Z-order. Only the portion of an unmanaged window / system
        // flyout NOT occluded by managed tiles above it should punch a hole in the overlay; an
        // unmanaged app sitting behind managed tiles must not erase their focus borders.
        for w in self.backend.windows() {
            let is_managed = self.owning(w).is_some_and(|k| Some(k.0) == self.desktop)
                || (self.pinned.contains(&w) && self.backend.on_current_desktop(w));
            if is_managed {
                if self.backend.is_visible(w)
                    && !self.backend.is_iconic(w)
                    && !self.backend.is_cloaked(w)
                    && let Some(r) = self.backend.frame(w)
                    && r.x < park_x - 2
                {
                    managed_above.push(r);
                }
                continue;
            }
            if self.owning(w).is_some() || self.pinned.contains(&w) {
                continue;
            }
            if !self.backend.overlay_occluder(w) {
                continue;
            }
            if !only.as_ref().is_none_or(|c| self.backend.class_of(w) == *c) {
                continue;
            }
            let Some(r) = self.backend.frame(w) else {
                continue;
            };
            for piece in subtract_rects(r, &managed_above) {
                if self.displays.iter().any(|m| {
                    piece.x < m.2.x + m.2.w
                        && piece.x + piece.w > m.2.x
                        && piece.y < m.2.y + m.2.h
                        && piece.y + piece.h > m.2.y
                }) {
                    out.push(piece);
                }
            }
        }
        out
    }
    pub(crate) fn tick(&mut self) -> Result<()> {
        if self.locked {
            return Ok(());
        }
        // Own-process windows (Win+W note box, Settings) are skipped by WINEVENT_SKIPOWNPROCESS,
        // so poll foreground changes here to remove/restore the focus highlight promptly.
        // Debounce transient NULL foregrounds (common during window activation) for 2 ticks so
        // the focus border does not blink off and back on.
        if !self.pad_scrolling && self.settle.is_none() && self.pad.is_none() {
            let fg = self.backend.foreground();
            if fg == 0 {
                if self.last_foreground != 0 {
                    self.null_fg_ticks = self.null_fg_ticks.saturating_add(1);
                    if self.null_fg_ticks >= 2 {
                        self.last_foreground = 0;
                        self.dirty = true;
                    }
                }
            } else {
                self.null_fg_ticks = 0;
                if fg != self.last_foreground {
                    self.last_foreground = fg;
                    self.dirty = true;
                }
            }
        }
        // An unmanaged window appearing, moving or closing changes what the overlay must stay clear of.
        if self.holes_checked.elapsed() >= Duration::from_millis(200) {
            self.holes_checked = Instant::now();
            if self.holes() != self.last_holes {
                self.dirty = true
            }
        }
        // Explorer reserves a monitor's taskbar space after the display change that brings the monitor back,
        // so the work area read by `topology` can still be the whole monitor. Nothing else re-reads it, and the
        // strip would keep tiles running under the taskbar. Re-run topology when the work areas differ.
        if self.deadline.is_none() && self.monitors_checked.elapsed() >= Duration::from_millis(500) {
            self.monitors_checked = Instant::now();
            if self.displays != self.backend.monitors() {
                self.deadline = Some(Instant::now() + Duration::from_millis(1500));
            }
        }
        if self.deadline.is_some_and(|t| Instant::now() >= t) {
            self.deadline = None;
            self.topology();
            self.scan()
        }
        if self.desktop_checked.elapsed() >= Duration::from_millis(150) {
            self.desktop_checked = Instant::now();
            let d = self.backend.current_desktop();
            if d != self.desktop {
                self.clear()?;
                self.desktop = d;
                self.placed.clear();
                self.instant = true;
                self.scan()
            }
        }
        self.refresh();
        if self.gesture_rec.as_ref().is_some_and(|r| Instant::now() >= r.until) {
            self.write_gesture_recording();
        }
        if self.pad.as_ref().is_some_and(|p| p.seen.elapsed() > Duration::from_millis(800)) {
            self.pad_end()
        }
        // Momentum normally runs on `fast_tick`; this keeps it going if the fast timer is not wired (tests).
        if self.inertia.is_some_and(|(_, _, last, _)| last.elapsed() >= Duration::from_millis(30)) {
            self.pad_inertia()
        }
        if self.settle.is_some_and(|t| Instant::now() >= t) {
            self.pad_settle()
        }
        if self.drag.is_some() && !self.blocked() {
            self.drag_update_inner(true, true)
        }
        Ok(())
    }
    /// Write a finished `planor gestures record` capture: one meta line, then one line per event in time
    /// order: `contacts` frames (an empty `pts` is a lift), `button` clicks and `mark` step labels, each with
    /// `t` in microseconds from the start. The format `replay` and the tests read.
    pub(crate) fn write_gesture_recording(&mut self) {
        let Some(rec) = self.gesture_rec.take() else { return };
        crate::input::RECORD_ALL.store(false, std::sync::atomic::Ordering::Relaxed);
        let mut text = format!("{}\n", rec.meta);
        for e in &rec.events {
            text += &format!("{e}\n");
        }
        let result = std::fs::write(&rec.out, text);
        if self.diag.enabled {
            diag::trace(format!("GESTURES recorded {} events to {} ({result:?})", rec.events.len(), rec.out.display()));
        }
    }
    /// True while the strip moves by itself (momentum or drag edge-scroll) and should be stepped at frame rate.
    pub(crate) fn wants_fast_tick(&self) -> bool {
        self.inertia.is_some()
            || self
                .drag
                .as_ref()
                .is_some_and(|d| !d.resizing && d.moved && d.edge_scroll_at.is_some())
    }
    /// One frame-rate step (about 8 ms): momentum after a swipe or drag edge-scrolling.
    pub(crate) fn fast_tick(&mut self) {
        if self.locked {
            return;
        }
        if self.inertia.is_some() {
            self.pad_inertia()
        }
        if !self.blocked()
            && self
                .drag
                .as_ref()
                .is_some_and(|d| !d.resizing && d.moved && d.edge_scroll_at.is_some())
        {
            self.drag_update();
        }
    }
    fn refresh(&mut self) {
        if self.blocked() {
            return;
        }
        if self.last_scan.elapsed() >= Duration::from_secs(1) {
            self.last_scan = Instant::now();
            for id in self.backend.windows() {
                if self.owning(id).is_none() {
                    self.add(id)
                }
            }
        }
        if self.hot.focus_follows_mouse {
            let (px, py) = self.backend.cursor_pos();
            let id = self.backend.window_at_point(px, py);
            if self.owning(id).is_some() && self.backend.foreground() != id {
                self.select(id);
                self.backend.activate(id);
                self.dirty = true
            }
        }
    }
    pub(crate) fn scan(&mut self) {
        let _span = perf::SpanGuard::new(perf::SpanKind::DaemonScan);
        let mut candidates: Vec<(isize, Option<Rect>)> = if self.blocked() {
            Vec::new()
        } else {
            self.backend
                .windows()
                .into_iter()
                .filter(|&id| {
                    self.owning(id).is_none()
                        && !self.skipped.contains(&id)
                        && !self.pinned.contains(&id)
                        && self.backend.eligible(id, &self.cfg)
                        && self.backend.on_current_desktop(id)
                })
                .map(|id| (id, self.backend.frame(id)))
                .collect()
        };
        candidates.sort_by_key(|(_, rect)| {
            rect.map(|r| {
                let portrait = self.displays.iter().any(|(_, b, _)| {
                    r.x >= b.x
                        && r.x < b.x + b.w
                        && r.y >= b.y
                        && r.y < b.y + b.h
                        && !b.horizontal()
                });
                if portrait { r.y } else { r.x }
            })
        });
        let before: HashSet<isize> = self.journal.entries.keys().copied().collect();
        self.journal.begin_batch();
        for (id, rect) in candidates {
            self.add_with_frame(id, rect, false)
        }
        if self.journal.end_batch().is_err() {
            let unsaved: Vec<isize> = self
                .journal
                .entries
                .keys()
                .copied()
                .filter(|id| !before.contains(id))
                .collect();
            for id in unsaved {
                self.journal.entries.remove(&id);
                self.backend.remove_recovery_cookie(id);
                if let Some(k) = self.owning(id) {
                    self.strips.get_mut(&k).unwrap().remove(id);
                }
                self.skipped.insert(id);
            }
        }
        self.select(self.backend.foreground());
        self.reveal()
    }
    fn settle_pending_rolls(&mut self) {
        let mut settled_any = false;
        for s in self.strips.values_mut() {
            if s.columns.iter().any(|c| c.frac != 0.0) {
                s.settle_roll();
                settled_any = true;
            }
        }
        if settled_any {
            self.dirty = true;
        }
    }
    fn clear_scroll_state(&mut self) {
        if !self.pad_scrolling && (self.settle.is_some() || self.inertia.is_some()) {
            self.settle_pending_rolls();
        }
        self.scroll_start_focus = None;
        self.scroll_dpi = None;
        self.inertia = None;
        self.wheel_mode = None;
        self.settle = None;
    }
    fn select(&mut self, id: isize) {
        self.clear_scroll_state();
        if let Some((desk, name)) = self.owning(id)
            && Some(desk) == self.desktop
        {
            self.active = name.clone();
            self.strips.get_mut(&(desk, name)).unwrap().select(id)
        }
    }
    pub(crate) fn topology(&mut self) {
        let _span = perf::SpanGuard::new(perf::SpanKind::DaemonTopology);
        self.monitors_checked = Instant::now();
        self.max_refresh_hz = self.backend.max_refresh_hz();
        let old = std::mem::replace(&mut self.displays, self.backend.monitors());
        self.displays_arc = Arc::from(self.displays.as_slice());
        for ((_, name), s) in &mut self.strips {
            if let Some((_, area, _)) = self.displays.iter().find(|m| &m.0 == name) {
                s.set_area(*area);
                for (id, _) in s.rects(0) {
                    if self.orphans.remove(&id) {
                        self.backend.show_no_activate(id);
                    }
                }
            } else if old.iter().any(|m| &m.0 == name) {
                for (id, _) in s.rects(0) {
                    self.orphans.insert(id);
                    self.homes.insert(id, name.clone());
                    self.backend.minimize_no_activate(id);
                }
            }
        }
        self.return_home();
        if !self.displays.iter().any(|m| m.0 == self.active) {
            self.active = self
                .displays
                .first()
                .map(|m| m.0.clone())
                .unwrap_or_default()
        }
        self.placed.clear();
        self.last_tile_geometry.clear();
        self.force_place = true;
        self.dirty = true;
        // Building the overlay windows and their GPU surfaces takes ~100 ms each; start now, in parallel
        // with the window scan, so the first placement does not wait for it.
        let _ = self.renderer.send(render::Message::Warm(
            self.displays.iter().map(|m| (m.0.clone(), m.2)).collect(),
        ));
    }
    /// The strip of a monitor, created with the layout settings `add` uses.
    fn home_strip(&mut self, key: Key, area: Rect) -> &mut Strip {
        let s = self.strips.entry(key).or_insert_with(|| Strip::new(area));
        s.gap = self.hot.gap_px;
        s.split = self.hot.first_pair_split;
        s.share = self.hot.default_tile_share;
        s
    }
    /// Puts every window whose monitor is attached again back on that monitor, including windows that
    /// `pull-orphans` moved to another monitor. A window whose monitor is still detached stays remembered.
    fn return_home(&mut self) {
        let peek = self.hot.reveal_peek_px();
        let entries: Vec<(isize, String)> = self.homes.drain().collect();
        for (id, home) in entries {
            let Some(owner) = self.owning(id) else { continue };
            let Some(area) = self.displays.iter().find(|m| m.0 == home).map(|m| m.1) else {
                self.homes.insert(id, home);
                continue;
            };
            if owner.1 != home {
                if let Some(s) = self.strips.get_mut(&owner) {
                    s.remove(id);
                    s.fill();
                }
                let minimum = self.backend.minimum(id, area.horizontal());
                let s = self.home_strip((owner.0, home), area);
                s.insert(id, minimum);
                // Inserted columns are focused; show it, so it is not left parked off-screen.
                s.reveal(peek);
            }
            self.orphans.remove(&id);
        }
    }
    pub(crate) fn layout(&mut self) -> Result<()> {
        if self.blocked() || !self.dirty {
            return Ok(());
        }
        let t = Instant::now();
        let _span = perf::SpanGuard::new(perf::SpanKind::DaemonLayout);
        self.dirty = false;
        self.covering.retain(|_, covered_at| covered_at.elapsed() < Duration::from_millis(500));
        for s in self.strips.values_mut() {
            s.fill_gaps()
        }
        let settled_state = !self.blocked()
            && self.drag.is_none()
            && !self.pad_scrolling
            && self.pad.is_none()
            && self.settle.is_none()
            && self.inertia.is_none();
        if settled_state {
            let stack_peek = self.hot.stack_peek_px.unwrap_or(28);
            for ((desk, _), strip) in &self.strips {
                if Some(*desk) != self.desktop {
                    continue;
                }
                for v in planor::check_strip_invariants(strip, stack_peek) {
                    perf::record_instant("INVARIANT", "invariant", v.trace_line());
                    if self.diag.enabled {
                        diag::trace(v.trace_line());
                    }
                    if std::env::var_os("PLANOR_NO_INVARIANT_PANIC").is_none() {
                        debug_assert!(false, "{}", v.trace_line());
                    }
                }
            }
        }
        let cap: usize = self.strips.values().map(|s| s.columns.len()).sum::<usize>() + self.pinned.len();
        let mut tiles = Vec::with_capacity(cap);
        let raw_fg = self.backend.foreground();
        if raw_fg != 0 {
            self.null_fg_ticks = 0;
            self.last_foreground = raw_fg;
        } else if self.last_foreground == 0 || self.null_fg_ticks >= 2 {
            self.last_foreground = 0;
        }
        let focused_id = self.managed_foreground_id(self.last_foreground);
        // Unmanaged app windows (an elevated Task Manager, a rule-ignored app) and system flyouts:
        // the overlay must never paint over them when they sit in front of managed tiles. `holes()`
        // subtracts managed/pinned windows above them in Z-order; the renderer masks them directly
        // in Direct2D (`D2D1_PRIMITIVE_BLEND_COPY`) and only uses `SetWindowRgn(..., 0)` where an
        // active DWM thumbnail overlaps a hole.
        // Test daemons share the desktop with the developer's own windows, which they treat as unmanaged;
        // carving those out would hide every border they draw, so isolated instances skip it unless a test
        // asks for it with PLANOR_HOLES.
        let live = self.pad_scrolling || self.inertia.is_some();
        if !(live && self.holes_checked.elapsed() < Duration::from_millis(150)) {
            self.holes_checked = Instant::now();
            let holes = self.holes();
            if holes != self.last_holes {
                self.last_holes = holes;
                self.last_holes_arc = Arc::from(self.last_holes.as_slice());
            }
        }
        let hot = self.hot;
        // While a window is carried nothing dodges out of its way (that reflow was erratic: too eager or not
        // at all). Instead every window on the monitor under the cursor shows its stack zone, filled; the
        // one under the cursor is stronger, and the window a drop would swap with gets the drop border.
        let mut zones = vec![];
        if let Some(d) = &self.drag
            && d.moved
            && !d.resizing
            && !d.visuals_cleared
            && let (Some(k), Some(c)) = (&d.destination, hot.drop_target_color)
            && let Some(s) = self.strips.get(k).filter(|_| Some(k.0) == self.desktop)
        {
            let peek = hot.stack_peek_px.unwrap_or(96);
            let side = hot.stack_zone_percent;
            for (id, _, z) in drop_zones(s, peek, side, d.id) {
                let hot_zone = d.target == Some(id) && d.mode == planor::Drag::Stack;
                zones.push(render::Zone { rect: z, monitor: k.1.clone(), color: c, alpha: if hot_zone { 0.85 } else { 0.35 } });
            }
        }
        let stack_peek = hot.stack_peek_px.unwrap_or(96);
        for ((desk, name), strip) in &self.strips {
            if Some(*desk) != self.desktop {
                continue;
            }
            for (id, mut rect) in strip.rects(stack_peek) {
                if let Some(saved) = self.journal.entries.get(&id) {
                    let (i, _) = strip.locate(id).unwrap();
                    if self.drag.as_ref().is_some_and(|d| d.id == id) {
                        continue;
                    }
                    let fullscreen = strip.fullscreen_id() == Some(id);
                    if fullscreen
                        && let Some((_, _, b)) = self.displays.iter().find(|m| &m.0 == name)
                    {
                        if strip.area.horizontal() {
                            rect.y = b.y;
                            rect.h = b.h
                        } else {
                            rect.x = b.x;
                            rect.w = b.w
                        }
                    }
                    let c = if fullscreen {
                        None
                    } else if self.drag.as_ref().is_some_and(|d| {
                        d.moved
                            && !d.resizing
                            && !d.visuals_cleared
                            && d.target == Some(id)
                            && matches!(d.mode, planor::Drag::Swap | planor::Drag::Before | planor::Drag::After)
                    }) {
                        // The window a drop would swap with (or pull out beside).
                        hot.drop_target_color
                    } else if self.drag.as_ref().is_some_and(|d| {
                        // A dragged window arriving over a group lights the whole group.
                        d.moved
                            && !d.resizing
                            && !d.visuals_cleared
                            && d.destination.as_ref().is_some_and(|dest| dest.0 == *desk && dest.1 == *name)
                            && d.target.is_some_and(|t| strip.locate(t).is_some_and(|(c, _)| c == i))
                            && strip.columns[i].members.len() > 1
                            && strip.columns[i].members[strip.columns[i].active] == id
                    }) {
                        hot.group_hover_color
                    } else if strip.columns[i].members.len() > 1
                        && strip.columns[i].members[strip.columns[i].active] == id
                    {
                        hot.stacked_color
                    } else if focused_id == Some(id) && name == &self.active && strip.active() == Some(id) {
                        hot.focused_color
                    } else {
                        None
                    };
                    let preview = strip.columns[i].members[strip.columns[i].active] != id;
                    let hidden = !strip.columns[i].shown(strip.columns[i].members.iter().position(|&m| m == id).unwrap_or(0));
                    tiles.push(render::Tile {
                        id,
                        rect,
                        monitor: name.clone(),
                        pinned: false,
                        fullscreen,
                        preview,
                        hidden,
                        bound: if fullscreen {
                            self.displays.iter().find(|m| &m.0 == name).map(|m| m.2).unwrap_or(strip.area)
                        } else {
                            strip.area
                        },
                        color: c,
                        cookie: saved.cookie,
                        cover: if !fullscreen && !preview && !hidden {
                            self.covering.get(&id).copied()
                        } else {
                            None
                        },
                    });
                }
            }
        }
        for &id in &self.pinned {
            if let (Some(rect), Some(saved)) = (self.backend.frame(id), self.journal.entries.get(&id))
                && let Some((name, _, _)) = self.displays.iter().find(|(_, _, b)| {
                    rect.x >= b.x && rect.x < b.x + b.w && rect.y >= b.y && rect.y < b.y + b.h
                })
            {
                tiles.push(render::Tile {
                    id,
                    rect,
                    monitor: name.clone(),
                    color: hot.pinned_color,
                    cookie: saved.cookie,
                    pinned: true,
                    fullscreen: false,
                    preview: false,
                    hidden: false,
                    bound: rect,
                    cover: None,
                })
            }
        }
        // Consume the action for this scene so subsequent hole-only or focus-only refreshes carry a
        // neutral action (`"idle"`, zero duration) instead of re-running `"open_close"` handoffs.
        let action = std::mem::replace(&mut self.action, "idle");
        let force_place = std::mem::take(&mut self.force_place);
        let geom: Vec<(isize, Rect, String, bool, bool, bool, bool, Rect)> = tiles
            .iter()
            .map(|t| {
                (
                    t.id,
                    t.rect,
                    t.monitor.clone(),
                    t.pinned,
                    t.fullscreen,
                    t.preview,
                    t.hidden,
                    t.bound,
                )
            })
            .collect();
        let had_zones = self.last_had_zones;
        self.last_had_zones = !zones.is_empty();
        // A holes/colour-only refresh with unchanged tile geometry must NEVER supersede an active
        // animation or open/close handoff with a new scene epoch; patch holes and tile colours
        // directly on the renderer instead.
        let overlay_only = action == "idle"
            && !force_place
            && !live
            && zones.is_empty()
            && !had_zones
            && !self.instant
            && !self.quick
            && !self.last_tile_geometry.is_empty()
            && self.last_tile_geometry == geom;
        if overlay_only {
            self.overlay_seq += 1;
            let colors = tiles.iter().map(|t| (t.id, t.color)).collect();
            self.renderer.send(render::Message::OverlayUpdate {
                seq: self.seq,
                update_id: self.overlay_seq,
                holes: Arc::clone(&self.last_holes_arc),
                colors,
            })?;
            slow("layout", t);
            return Ok(());
        }
        self.seq += 1;
        for tile in &tiles {
            if !tile.pinned {
                self.placed.insert(tile.id, (self.seq, tile.rect));
            }
        }
        self.last_tile_geometry = geom;
        // The real animation time of this scene (0 when placed at once: under the fingers, a monitor
        // crossing, a fullscreen switch, or a neutral hole/focus refresh). The EXPECT lines carry
        // the same value, so the recorder's lag check waits for what is actually on screen.
        let duration = if !std::mem::take(&mut self.instant)
            && hot.anim_enabled
            && hot.anim_action_enabled(action)
        {
            // Live touchpad scrolling re-targets every few milliseconds; a short ease per update
            // smooths it without the strip trailing the fingers.
            if std::mem::take(&mut self.quick) {
                70
            } else {
                hot.anim_duration_ms
            }
        } else {
            self.quick = false;
            0
        };
        if self.diag.enabled {
            let animate_ms = duration;
            for t in &tiles {
                diag::trace(format!(
                    "EXPECT id={} rect={},{},{},{} monitor={} kind={} hidden={} animate_ms={}{}",
                    t.id,
                    t.rect.x,
                    t.rect.y,
                    t.rect.w,
                    t.rect.h,
                    t.monitor,
                    if t.fullscreen {
                        "fullscreen"
                    } else if t.hidden {
                        "parked"
                    } else if t.preview {
                        "peek"
                    } else {
                        "tile"
                    },
                    if t.hidden { 1 } else { 0 },
                    animate_ms,
                    if live { " live=1" } else { "" }
                ));
            }
        }
        let tiles_len = tiles.len();
        render::EPOCH.store(self.seq, std::sync::atomic::Ordering::Release);
        self.renderer.send(render::Message::Scene(render::Scene {
            epoch: self.seq,
            // Only an input that caused this scene counts towards gesture-to-place latency: one that changed
            // nothing (a lift, a tap) stayed pending and was charged to an unrelated scene seconds later
            // (p99 7.9 s, max 10.7 s in the user's daemon).
            input: self.input.take().filter(|t| t.elapsed() < Duration::from_millis(250)),
            input_qpc_us: self.input_qpc_us.take().filter(|&t| perf::qpc_us() - t < 250_000),
            diagnostics: self.diag.enabled,
            tiles,
            monitors: Arc::clone(&self.displays_arc),
            duration,

            spring: hot.anim_spring,
            width: hot.border_width_px,
            radius: hot.border_radius_px,
            holes: Arc::clone(&self.last_holes_arc),
            zones,
            action,
            live,
            force_place,
        }))?;
        if self.diag.enabled {
            diag::trace(format!("SCENE seq={} tiles={} holes={}", self.seq, tiles_len, self.last_holes.len()));
        }
        slow("layout", t);
        Ok(())
    }
    fn contacts(&mut self, p: Vec<(f64, f64)>) {
        if let Some(rec) = self.gesture_rec.as_mut() {
            let pts: Vec<[f64; 2]> = p.iter().map(|&(x, y)| [(x * 100.).round() / 100., (y * 100.).round() / 100.]).collect();
            rec.push(serde_json::json!({ "kind": "contacts", "pts": pts }));
            if rec.passive {
                self.gesture = Default::default();
                return;
            }
        }
        // 1 or 2 fingers only arrive while recording; to the gestures they are a lift. Five or more are
        // passed on: they veto the gesture.
        let p = if p.len() >= 3 { p } else { vec![] };
        if self.diag.enabled
            && (p.len() == 3 || p.len() == 4 || self.pad.is_some() || self.pad_scrolling)
        {
            let pts = if p.is_empty() {
                "-".to_string()
            } else {
                p.iter()
                    .map(|(x, y)| format!("{x:.2},{y:.2}"))
                    .collect::<Vec<_>>()
                    .join(";")
            };
            diag::trace(format!("PAD contacts n={} pts={pts}", p.len()));
        }
        let swipe = self.gesture.sample(
            &p,
            self.hot.axis_lock_deadzone,
            self.hot.swipe_step_px,
        );
        // Three fingers move the focus, four fingers move the focused window. With the
        // natural direction the content follows the fingers (swipe right = focus left).
        // Gesture trace (diagnostics): what was recognised and when, for lining up with a note.
        if self.diag.enabled && !matches!(swipe, Swipe::None | Swipe::DragMove(..) | Swipe::Scroll(_) | Swipe::ScrollV(_)) {
            diag::trace(format!("PAD {swipe:?}"));
        }
        match swipe {
            Swipe::DragStart => {
                if self.diag.enabled {
                    diag::trace(format!("PAD drag start, window {:?}", self.key().and_then(|k| self.strips.get(&k)).and_then(|s| s.active())));
                }
                self.pad_begin();
            }
            Swipe::DragMove(dx, dy) => self.pad_move(dx, dy),
            Swipe::DragEnd => self.pad_end(),
            Swipe::DragCancel => self.pad_cancel(),
            Swipe::Scroll(d) => self.pad_scroll(d, false),
            Swipe::ScrollV(d) => self.pad_scroll(d, true),
            Swipe::ScrollEnd => self.pad_scroll_end(),
            Swipe::Step { fingers, horizontal, direction } => {
                let natural = self.hot.swipe_natural_direction;
                let forward = (direction > 0) != natural;
                let key = match (fingers, horizontal, forward) {
                    (3, true, false) => "focus_left",
                    (3, true, true) => "focus_right",
                    (3, false, false) => "focus_up",
                    (3, false, true) => "focus_down",
                    (4, true, false) => "move_left",
                    (4, true, true) => "move_right",
                    _ => return,
                };
                diag::trace(format!("PAD step fingers={fingers} horizontal={horizontal} direction={direction} key={key}"));
                self.quick = true;
                self.keypress(key);
            }
            Swipe::None => (),
        }
    }
    fn clear(&mut self) -> Result<()> {
        self.last_tile_geometry.clear();
        self.last_had_zones = false;
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.renderer.send(render::Message::Clear(tx))?;
        rx.recv()?;
        Ok(())
    }
    fn pin(&mut self) {
        let id = self.backend.foreground();
        let _ = self.clear();
        if self.pinned.remove(&id) {
            self.backend.set_topmost(id, false);
            self.add_with_frame(id, None, false)
        } else if let Some(k) = self.owning(id) {
            self.backend.set_topmost(id, true);
            self.pinned.insert(id);
            self.strips.get_mut(&k).unwrap().remove(id);
            self.reveal()
        }
    }
    fn keypress(&mut self, key: &str) {
        self.action = if key == "stack_toggle" {
            "stack_switch"
        } else {
            "scroll"
        };
        if self.blocked() {
            return;
        }
        self.clear_scroll_state();
        // Act on the monitor that really holds focus, not the one Planor last recorded.
        let focused = self.backend.foreground();
        if self.owning(focused).is_some() {
            self.select(focused)
        }
        if key == "toggle_pin" {
            self.pin();
            return;
        }
        if key.starts_with("focus_monitor_") && !self.displays.is_empty() {
            let pos = self
                .displays
                .iter()
                .position(|m| m.0 == self.active)
                .unwrap_or(0);
            let n = self.displays.len();
            self.active = self.displays[(pos + if key.ends_with("next") { 1 } else { n - 1 }) % n]
                .0
                .clone()
        }
        let mut transferred = false;
        if let Some((dx, dy)) = match key {
            "focus_left" => Some((-1, 0)),
            "focus_right" => Some((1, 0)),
            "focus_up" => Some((0, -1)),
            "focus_down" => Some((0, 1)),
            _ => None,
        } && let Some(strip) = self.key().and_then(|k| self.strips.get(&k))
        {
            let cross = (dx != 0) != strip.area.horizontal();
            let at_edge = if cross {
                strip
                    .columns
                    .get(strip.focus)
                    .is_none_or(|c| c.members.len() == 1)
            } else if dx + dy < 0 {
                strip.focus == 0
            } else {
                strip.focus + 1 >= strip.columns.len()
            };
            if at_edge {
                let bounds = self.displays.iter().map(|m| m.2).collect::<Vec<_>>();
                if let Some(index) = self
                    .displays
                    .iter()
                    .position(|m| m.0 == self.active)
                    .and_then(|i| planor::adjacent(bounds[i], &bounds, dx, dy))
                {
                    let name=&self.displays[index].0;if self.desktop.is_some_and(|d|self.strips.get(&(d,name.clone())).and_then(|s|s.active()).is_some()){self.active=name.clone();transferred=true}
                }
            }
        }
        let Some(s) = self.key().and_then(|k| self.strips.get_mut(&k)) else {
            return;
        };
        let step = if key.ends_with("left") || key.ends_with("up") {
            -1
        } else {
            1
        };
        match key {
            _ if transferred => (),
            "focus_left" | "focus_right" | "focus_up" | "focus_down" => s.navigate(
                step,
                (key.ends_with("left") || key.ends_with("right")) != s.area.horizontal(),
            ),
            "move_left" | "move_right" => {
                let i = s.focus;
                let j = i
                    .saturating_add_signed(step)
                    .min(s.columns.len().saturating_sub(1));
                if i != j {
                    s.columns.swap(i, j);
                    s.focus = j
                }
            }
            "stack_toggle" => {
                if let Some(id) = s.active() {
                    if s.columns[s.focus].members.len() > 1 {
                        s.ungroup(id)
                    } else if s.columns.len() > 1 {
                        let i = if s.focus > 0 { s.focus - 1 } else { 1 };
                        let target = s.columns[i].members[s.columns[i].active];
                        s.group(id, target)
                    }
                }
            }
            _ => (),
        }
        let id = s.active();
        self.reveal();
        if let Some(id) = id {
            self.backend.activate(id);
            if self.hot.warp_cursor_to_focused
                && let Some(r) = self.strips[&self.key().unwrap()]
                    .rects(0)
                    .into_iter()
                    .find(|(wid, _)| *wid == id)
                    .map(|(_, r)| r)
            {
                self.backend.set_cursor_pos(r.x + r.w / 2, r.y + r.h / 2);
            }
        }
    }
    fn drag_begin(&mut self, id: isize) {
        if let Some(key) = self.owning(id)
            && let Some(original) = self.backend.frame(id)
        {
            let point = self.backend.cursor_pos();
            let pad_drag = self.pad.is_some();
            let saw_lbutton_down = !pad_drag && self.backend.primary_button_down();
            self.drag = Some(DragSession {
                id,
                key,
                original,
                point,
                sampled: Instant::now(),
                hover: Instant::now(),
                target: None,
                mode: planor::Drag::None,
                destination: None,
                resizing: false,
                edge_scroll_at: None,
                edge_scroll_frac: 0.0,
                moved: pad_drag,
                pad_drag,
                saw_lbutton_down,
                lbutton_up_since: None,
                visuals_cleared: false,
                movesize_exit_since: None,
            });
            self.dirty = true
        } else if self.drag.take().is_some() {
            // Starting a move/size on an unmanaged window (e.g. a torn-off browser tab) clears any
            // stray drag session left behind on an earlier window.
            self.dirty = true;
        }
    }
    /// Three fingers sideways: the strip follows the fingers live (no animation) and the focus moves to
    /// whichever window is nearest the centre, so scrolling and focusing are one continuous motion.
    /// Three fingers sideways (`vertical == false`) or up and down. Called once per touchpad report.
    fn pad_scroll(&mut self, d: f64, vertical: bool) {
        if self.blocked() || self.pad.is_some() {
            return;
        }
        if !self.pad_scrolling {
            self.pad_scrolling = true;
            self.scroll_frac = 0.;
            self.inertia = None;
            self.wheel_mode = None;
            self.v_travel = 0.;
            self.hopped = false;
            self.scroll_samples.clear();
            // Resuming within the settle delay keeps the focus the scroll already chose.
            if self.settle.is_none() {
                let focused = self.backend.foreground();
                if self.owning(focused).is_some() {
                    self.select(focused)
                }
            }
            self.scroll_dpi = self.backend.contact_dpi();
            self.scroll_start_focus = self
                .key()
                .and_then(|k| self.strips.get(&k).and_then(|s| s.active()).map(|id| (k, id)));
        }
        self.settle = None;
        self.inertia = None;
        let now = Instant::now();
        self.scroll_samples.push_back((now, d, vertical));
        while self.scroll_samples.front().is_some_and(|s| now.duration_since(s.0) > Duration::from_millis(150)) {
            self.scroll_samples.pop_front();
        }
        let dpi = self.scroll_dpi;
        let _ = self.scroll_by(d, vertical, true, dpi);
    }
    /// Move the strip (or roll the focused group) by `d` finger units. Shared by the live gesture and the
    /// momentum that carries on after the fingers lift. Returns `(changed, clamped)`.
    fn scroll_by(&mut self, d: f64, vertical: bool, live: bool, contact_dpi: Option<f64>) -> (bool, bool) {
        let gain = self.hot.swipe_scroll_gain;
        let natural = self.hot.swipe_natural_direction;
        let signed = if natural { -d } else { d } * gain;
        if self.hopped {
            return (false, true);
        }
        let Some(s) = self.key().and_then(|k| self.strips.get_mut(&k)) else {
            return (false, true);
        };
        // The same controls on every monitor, turned with it: fingers along the strip scroll it (sideways
        // on a landscape monitor, up and down on a portrait one); fingers across it roll the group.
        let along = vertical != s.area.horizontal();
        if !along {
            let prev = s.columns.get(s.focus).map(|c| (c.active, c.frac));
            if !s.roll(group_roll_delta(signed, contact_dpi)) {
                // No group here: a long swipe across the strip hops to the monitor that way instead
                // (up/down from a landscape monitor, left/right from a portrait one), once per gesture.
                self.v_travel += d;
                if self.v_travel.abs() >= 90. {
                    let forward = (self.v_travel > 0.) != natural;
                    self.v_travel = 0.;
                    self.hopped = true;
                    self.keypress(match (vertical, forward) {
                        (true, true) => "focus_down",
                        (true, false) => "focus_up",
                        (false, true) => "focus_right",
                        (false, false) => "focus_left",
                    });
                }
                return (false, false);
            }
            let after = s.columns.get(s.focus).map(|c| (c.active, c.frac));
            if after == prev {
                return (false, signed.abs() > 1e-6);
            }
        } else {
            self.scroll_frac += strip_scroll_px(signed, s.area.length(), contact_dpi);
            let whole = self.scroll_frac.trunc();
            self.scroll_frac -= whole;
            let delta_px = whole as i32;
            if delta_px == 0 {
                return (false, false);
            }
            let prev_offset = s.offset;
            let prev_focus = s.focus;
            s.scroll(delta_px);
            s.focus_nearest_center();
            if s.offset == prev_offset && s.focus == prev_focus {
                return (false, true);
            }
        }
        // Under the fingers the strip follows with no easing at all (one to one). The momentum after the
        // lift is stepped at frame rate (see `fast_tick`), so it is placed directly too: re-targeting a
        // 70 ms ease every step restarted the curve each time, the stutter in "scroll is not smooth".
        let _ = live;
        self.instant = true;
        if self.diag.enabled {
            let frac = s.columns.get(s.focus).map(|c| c.frac).unwrap_or(0.0);
            diag::trace(format!(
                "PAD scroll live={live} axis={} d={d:.2} offset={} focus={} frac={frac:.4}",
                if vertical { 'v' } else { 'h' },
                s.offset,
                s.focus
            ));
        }
        self.action = "scroll";
        self.dirty = true;
        (true, false)
    }
    /// Win + mouse wheel: scroll the strip of the monitor under the cursor along its own axis (sideways on
    /// a landscape monitor, up and down on a portrait one); wheel down / tilt right moves forward. With
    /// Shift (or on a portrait monitor, the tilt wheel) it rolls the focused group instead. Focus follows
    /// the window nearest the centre and settles shortly after the wheel stops, like a touchpad scroll.
    fn wheel(&mut self, delta: i32, tilt: bool, shift: bool, at: (i32, i32)) {
        if let Some(rec) = self.gesture_rec.as_mut() {
            rec.push(serde_json::json!({ "kind": "wheel", "delta": delta, "tilt": tilt, "shift": shift, "x": at.0, "y": at.1 }));
        }
        if self.blocked() || self.drag.is_some() || self.pad.is_some() {
            return;
        }
        let Some(name) = self.displays.iter().find(|m| contains(m.2, at)).map(|m| m.0.clone()) else {
            return;
        };
        if self.desktop.is_some_and(|d| self.strips.contains_key(&(d, name.clone()))) {
            self.active = name;
        }
        let step = self.hot.win_wheel_step_px;
        // Wheel down is a negative delta; the tilt wheel is positive to the right. Mouse only: fine touchpad
        // steps (not whole notches of 120) are left alone (the hook already lets them through). A fast spin can
        // pack several notches into one event: at most two per event.
        if delta % 120 != 0 {
            return;
        }
        let delta = delta.clamp(-240, 240);
        let notches = if tilt { delta } else { -delta } as f64 / 120.;
        if !self.pad_scrolling && (self.inertia.is_some() || self.settle.is_some()) {
            self.settle_pending_rolls();
        }
        self.scroll_start_focus = None;
        self.scroll_dpi = None;
        self.inertia = None;
        let now = Instant::now();
        let Some(key) = self.key() else { return };
        let Some(s) = self.strips.get_mut(&key) else { return };
        let focused_is_group = s.columns.get(s.focus).is_some_and(|c| c.members.len() > 1);
        let roll = if tilt {
            !s.area.horizontal()
        } else if let Some((prev_key, prev_at, prev_shift, prev_roll)) = &self.wheel_mode
            && *prev_key == key
            && now.duration_since(*prev_at) <= Duration::from_millis(260)
            && *prev_shift == shift
        {
            let r = *prev_roll;
            if r && !focused_is_group {
                self.wheel_mode = None;
                false
            } else {
                self.wheel_mode = Some((key.clone(), now, shift, r));
                r
            }
        } else {
            let r = focused_is_group ^ shift;
            if r && !focused_is_group {
                self.wheel_mode = None;
                return;
            }
            self.wheel_mode = Some((key.clone(), now, shift, r));
            r
        };
        if roll {
            // One member per notch: a wheel has no lift to settle on, so it never leaves the deck half rolled.
            let prev = s.columns.get(s.focus).map(|c| (c.active, c.frac));
            if !s.roll(notches.signum() * notches.abs().round().max(1.))
                || s.columns.get(s.focus).map(|c| (c.active, c.frac)) == prev
            {
                return;
            }
        } else {
            s.scroll((notches * step).round() as i32);
            s.focus_nearest_center();
        }
        if self.diag.enabled {
            diag::trace(format!("WHEEL delta={delta} tilt={tilt} shift={shift} offset={} focus={}", s.offset, s.focus));
        }
        self.action = "scroll";
        self.quick = true;
        self.dirty = true;
        self.settle = Some(Instant::now() + Duration::from_millis(260));
    }
    fn pad_scroll_end(&mut self) {
        if !std::mem::take(&mut self.pad_scrolling) {
            return;
        }
        self.scroll_frac = 0.;
        let scroll_dpi = self.scroll_dpi.take();
        if self.hopped {
            self.scroll_samples.clear();
            self.clear_scroll_state();
            return;
        }
        // Momentum: a swipe that was still moving when the fingers lifted keeps going and slows down, the
        // way a page or a phone list does.
        let now = Instant::now();
        let vertical = self.scroll_samples.back().map(|s| s.2).unwrap_or(false);
        let mut recent = Vec::new();
        for (t, d, v) in &self.scroll_samples {
            let age = now.duration_since(*t);
            if age <= Duration::from_millis(100) && *v == vertical {
                recent.push((age.as_secs_f64(), *d));
            }
        }
        self.scroll_samples.clear();
        let unit_dpi = scroll_dpi.unwrap_or(96.0).max(1.0);
        let total_travel_in: f64 = recent.iter().map(|&(_, d)| d).sum::<f64>() / unit_dpi;
        let speed = scroll_fling_velocity(&recent);
        let speed_ips = speed / unit_dpi;
        let fling_dir = fling_advance_direction(
            speed_ips,
            total_travel_in,
            self.hot.swipe_natural_direction,
            250.0 / 96.0,
            18.0 / 96.0,
        );
        // Deliberate fling while still on (or just reaching) the adjacent target: choose the destination at
        // lift and animate directly to it in one continuous motion (no inertia decay pause + second slide).
        if let (Some(dir), Some((start_key, start_win))) = (fling_dir, self.scroll_start_focus.take())
            && self.key().as_ref() == Some(&start_key)
            && let Some(s) = self.strips.get_mut(&start_key)
        {
            let along = vertical != s.area.horizontal();
            let mut handled = false;
            if along {
                if s.active() == Some(start_win) && !s.columns.is_empty() {
                    s.settle_roll();
                    let max_col = s.columns.len().saturating_sub(1) as isize;
                    s.focus = (s.focus as isize + dir).clamp(0, max_col) as usize;
                    handled = true;
                }
            } else if let Some((start_col, start_member)) = s.locate(start_win)
                && s.focus == start_col
                && let Some(c) = s.columns.get_mut(start_col)
                && c.members.len() > 1
            {
                let n = c.members.len() as isize;
                let target = if c.wraps() {
                    (start_member as isize + dir).rem_euclid(n) as usize
                } else {
                    (start_member as isize + dir).clamp(0, n - 1) as usize
                };
                let pos = c.active as f64 + c.frac;
                let nearest = if c.wraps() {
                    (pos.round() as isize).rem_euclid(n) as usize
                } else {
                    (pos.round() as isize).clamp(0, n - 1) as usize
                };
                if nearest == start_member || nearest == target {
                    c.active = target;
                    c.frac = 0.0;
                    handled = true;
                }
            }
            if handled {
                let id = s.active();
                self.inertia = None;
                self.settle = None;
                self.wheel_mode = None;
                self.action = "scroll";
                self.quick = true;
                self.reveal();
                if let Some(id) = id {
                    self.backend.activate(id);
                }
                return;
            }
        }
        self.scroll_start_focus = None;
        let enabled = self.hot.swipe_inertia;
        if enabled && speed_ips.abs() > (150.0 / 96.0) {
            self.inertia = Some((vertical, speed, now, scroll_dpi));
            self.settle = None;
        } else {
            // Settle only after the fingers have stayed off for a moment, so a swipe that continues
            // right away is not interrupted by a snap and a focus change.
            self.settle = Some(now + Duration::from_millis(220));
        }
    }
    /// One tick of momentum.
    fn pad_inertia(&mut self) {
        let Some((vertical, speed, last, dpi)) = self.inertia else { return };
        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f64().min(0.1);
        let (_, clamped) = self.scroll_by(speed * dt, vertical, false, dpi);
        let speed = speed * 0.9f64.powf(dt / 0.032);
        let speed_ips = speed / dpi.unwrap_or(96.0).max(1.0);
        if clamped || speed_ips.abs() < (40.0 / 96.0) || self.blocked() {
            self.inertia = None;
            self.settle = Some(now + Duration::from_millis(120));
        } else {
            self.inertia = Some((vertical, speed, now, dpi));
        }
    }
    fn pad_settle(&mut self) {
        self.settle = None;
        self.scroll_start_focus = None;
        self.scroll_dpi = None;
        self.wheel_mode = None;
        if self.blocked() {
            return;
        }
        if let Some(s) = self.key().and_then(|k| self.strips.get_mut(&k)) {
            s.settle_roll();
        }
        let id = self.key().and_then(|k| self.strips.get(&k)).and_then(|s| s.active());
        self.action = "scroll";
        self.quick = true;
        self.reveal();
        if let Some(id) = id {
            self.backend.activate(id)
        }
    }
    fn pad_begin(&mut self) {
        if self.blocked() || self.drag.is_some() {
            return;
        }
        self.clear_scroll_state();
        let focused = self.backend.foreground();
        if self.owning(focused).is_some() {
            self.select(focused)
        }
        let Some(id) = self.key().and_then(|k| self.strips.get(&k)).and_then(|s| s.active()) else {
            return;
        };
        let Some(frame) = self.backend.frame(id) else { return };
        let Some(rect) = self.backend.window_rect(id) else {
            return;
        };
        self.drag_begin(id);
        let Some(d) = self.drag.as_mut() else {
            return;
        };
        d.pad_drag = true;
        d.moved = true;
        d.saw_lbutton_down = false;
        // Grab the window at its centre so the cursor and the window stay together.
        let (cx, cy) = (frame.x + frame.w / 2, frame.y + frame.h / 2);
        self.backend.set_cursor_pos(cx, cy);
        // Carrying a window across monitors with different DPI makes the app resize and re-render on
        // every step (the recording shows it flickering to odd sizes and freezing). By default the drag
        // stays on its own monitor and a fling does the crossing in one step.
        let cross = self.hot.swipe_drag_cross_monitors;
        let own = self
            .displays
            .iter()
            .find(|m| cx >= m.2.x && cx < m.2.x + m.2.w && cy >= m.2.y && cy < m.2.y + m.2.h)
            .map(|m| m.2);
        let b = self
            .displays
            .iter()
            .map(|m| m.2)
            .filter(|r| cross || own.is_none_or(|o| *r == o))
            .collect::<Vec<_>>();
        let b = b.iter();
        let limits = (
            b.clone().map(|r| r.x).min().unwrap_or(0) as f64,
            b.clone().map(|r| r.y).min().unwrap_or(0) as f64,
            b.clone().map(|r| r.x + r.w).max().unwrap_or(1) as f64 - 1.,
            b.map(|r| r.y + r.h).max().unwrap_or(1) as f64 - 1.,
        );
        self.pad = Some(PadDrag {
            id,
            grab: ((cx - rect.x) as f64, (cy - rect.y) as f64),
            cursor: (cx as f64, cy as f64),
            limits,
            seen: Instant::now(),
            speed: 0.,
            moved: Instant::now(),
            samples: Default::default(),
        });
    }
    fn pad_move(&mut self, dx: f64, dy: f64) {
        let base = self.hot.swipe_drag_gain;
        let accel = self.hot.swipe_drag_accel;
        let Some(p) = self.pad.as_mut() else { return };
        let now = Instant::now();
        // Acceleration: the faster the fingers move, the further the window goes per unit.
        let dt = now.duration_since(p.seen).as_secs_f64().clamp(0.008, 0.1);
        let speed = dx.hypot(dy) / dt;
        p.speed = p.speed * 0.8 + speed * 0.2;
        let gain = base * (1. + accel * p.speed / 1000.).min(1.6);
        p.seen = now;
        p.samples.push_back((now, dx, dy));
        while p.samples.front().is_some_and(|s| now.duration_since(s.0) > Duration::from_millis(150)) {
            p.samples.pop_front();
        }
        let (l, t, r, b) = p.limits;
        p.cursor.0 = (p.cursor.0 + dx * gain).clamp(l, r);
        p.cursor.1 = (p.cursor.1 + dy * gain).clamp(t, b);
        self.backend.set_cursor_pos(p.cursor.0.round() as i32, p.cursor.1.round() as i32);
        // Asynchronous and rate-limited (scales cleanly past 120+ Hz, latest position wins): a heavy
        // window cannot keep up with every touchpad report, and a queue of stale moves would make the
        // window trail the fingers.
        let min_move_us = planor::pacing::pad_move_min_interval_us(self.max_refresh_hz);
        if now.duration_since(p.moved) >= Duration::from_micros(min_move_us) {
            p.moved = now;
            let _gspan = perf::SpanGuard::new(perf::SpanKind::GestureToPlace);
            self.backend.move_window_async(
                p.id,
                (p.cursor.0 - p.grab.0).round() as i32,
                (p.cursor.1 - p.grab.1).round() as i32,
            );
            // Targets and zones follow the fingers at this rate, not only on the 32 ms tick.
            self.drag_update();
        }
    }
    /// Five fingers while carrying a window: nothing is dropped, the layout puts the window back.
    fn pad_cancel(&mut self) {
        if self.pad.take().is_some() {
            self.drag = None;
            self.flung = false;
            self.instant = true;
            self.force_place = true;
            self.dirty = true;
        }
    }
    fn pad_end(&mut self) {
        let Some(p) = self.pad.take() else { return };
        // Fling: a fast flick at lift-off throws the window to the neighbouring screen in that direction.
        let now = Instant::now();
        let (mut vx, mut vy) = (0., 0.);
        for (t, dx, dy) in &p.samples {
            if now.duration_since(*t) <= Duration::from_millis(120) {
                vx += dx;
                vy += dy;
            }
        }
        let (vx, vy) = (vx / 0.12, vy / 0.12);
        // Settle the window synchronously where the cursor is, so the drop reads its real position
        // (the moves above were asynchronous).
        self.backend.move_window_async(
            p.id,
            (p.cursor.0 - p.grab.0).round() as i32,
            (p.cursor.1 - p.grab.1).round() as i32,
        );
        let threshold = self.hot.swipe_fling_speed;
        if self.hot.swipe_fling_enabled
            && vx.hypot(vy) >= threshold
        {
            let (dx, dy) = if vx.abs() >= vy.abs() {
                (if vx > 0. { 1 } else { -1 }, 0)
            } else {
                (0, if vy > 0. { 1 } else { -1 })
            };
            let current = self
                .drag
                .as_ref()
                .and_then(|d| d.destination.as_ref().map(|k| k.1.clone()))
                .or_else(|| self.key().map(|k| k.1));
            let bounds = self.displays.iter().map(|m| m.2).collect::<Vec<_>>();
            let dest = current
                .and_then(|n| self.displays.iter().position(|m| m.0 == n))
                .and_then(|i| planor::adjacent(bounds[i], &bounds, dx, dy));
            if let Some(i) = dest {
                // No teleport of the window here: a synchronous SetWindowPos across a DPI boundary
                // waits for the app to finish its WM_DPICHANGED relayout (seconds for a browser) and
                // froze this thread. Only the cursor moves; drag_end re-slots the window on the
                // destination monitor with the layout (placed off this thread).
                let b = bounds[i];
                self.backend.set_cursor_pos(b.x + b.w / 2, b.y + b.h / 2);
                self.flung = true;
            }
        }
        self.drag_end(p.id)
    }
    fn drag_update(&mut self) {
        self.drag_update_inner(true, false);
    }
    fn drag_update_inner(&mut self, allow_edge_scroll: bool, poll_release: bool) {
        let Some((drag_id, drag_key)) = self.drag.as_ref().map(|d| (d.id, d.key.clone())) else {
            return;
        };
        if !self.backend.is_window(drag_id)
            || !self.backend.is_visible(drag_id)
            || self.backend.is_iconic(drag_id)
            || self.owning(drag_id) != Some(drag_key)
        {
            self.drag = None;
            self.dirty = true;
            return;
        }
        let Some(d) = self.drag.as_mut() else { return };
        // The first release tick samples the final drop position. Subsequent updates,
        // including MOVESIZEEND, use that sample even if another press follows.
        let release_sampled = d.lbutton_up_since.is_some();
        if !d.pad_drag && poll_release {
            if !release_sampled && self.backend.primary_button_down() {
                d.saw_lbutton_down = true;
            } else if d.saw_lbutton_down {
                let since = *d.lbutton_up_since.get_or_insert_with(Instant::now);
                if self.backend.in_movesize(drag_id) == Some(false) {
                    let exited = d.movesize_exit_since.get_or_insert_with(Instant::now);
                    if exited.elapsed() >= Duration::from_millis(500) {
                        self.drag = None;
                        self.force_place = true;
                        self.dirty = true;
                        return;
                    }
                } else {
                    d.movesize_exit_since = None;
                }
                if since.elapsed() >= Duration::from_millis(250) && !d.visuals_cleared {
                    d.visuals_cleared = true;
                    self.dirty = true;
                }
            }
        }
        if release_sampled {
            return;
        }
        let allow_edge_scroll = allow_edge_scroll && d.lbutton_up_since.is_none();
        // Latch the first classification (edge resize vs title-bar move) so a later WM_DPICHANGED
        // or drag-restore size change during a move never flips `resizing` and clears `destination`.
        if !d.pad_drag
            && !d.resizing
            && !d.moved
            && let Some(cur) = self.backend.frame(d.id)
        {
            if is_edge_resize(d.original, cur) {
                d.resizing = true;
            } else if (cur.x - d.original.x).abs() > 2
                || (cur.y - d.original.y).abs() > 2
                || (cur.w - d.original.w).abs() > 2
                || (cur.h - d.original.h).abs() > 2
            {
                d.moved = true;
            }
        }
        if d.resizing || !d.moved {
            if d.target.is_some() || d.mode != planor::Drag::None || d.destination.is_some() {
                d.target = None;
                d.mode = planor::Drag::None;
                d.destination = None;
                self.dirty = true;
            }
            return;
        }
        let p = self.backend.cursor_pos();
        let elapsed = d.sampled.elapsed().as_secs_f64().max(0.001);
        let velocity = ((p.0 - d.point.0) as f64).hypot((p.1 - d.point.1) as f64) / elapsed;
        d.point = p;
        d.sampled = Instant::now();
        let Some((monitor, _, _)) = self
            .displays
            .iter()
            .find(|(_, _, r)| p.0 >= r.x && p.0 < r.x + r.w && p.1 >= r.y && p.1 < r.y + r.h)
        else {
            d.target = None;
            d.mode = planor::Drag::None;
            d.destination = None;
            d.edge_scroll_at = None;
            d.edge_scroll_frac = 0.0;
            self.dirty = true;
            return;
        };
        let k = (d.key.0, monitor.clone());
        if d.destination.as_ref() != Some(&k) {
            d.destination = Some(k.clone());
            self.instant = true;
            self.dirty = true;
        }
        // Dragging to the leading or trailing edge of the strip scrolls it smoothly over time, unless
        // another monitor lies that way (then the drag is heading there instead). Skipped on `drag_end`
        // so releasing near the edge never shifts targets out from under the cursor.
        if allow_edge_scroll && k == d.key {
            let bounds: Vec<Rect> = self.displays.iter().map(|m| m.2).collect();
            let at = self.displays.iter().position(|m| m.0 == k.1);
            if let Some(s) = self.strips.get_mut(&k) {
                let horizontal = s.area.horizontal();
                let (pos, lo, hi) = if horizontal {
                    (p.0, s.area.x, s.area.x + s.area.w)
                } else {
                    (p.1, s.area.y, s.area.y + s.area.h)
                };
                let dir = if pos < lo + 48 {
                    -1
                } else if pos > hi - 48 {
                    1
                } else {
                    0
                };
                let beyond = at.and_then(|i| {
                    planor::adjacent(bounds[i], &bounds, if horizontal { dir } else { 0 }, if horizontal { 0 } else { dir })
                });
                if dir != 0 && beyond.is_none() {
                    let hz = self.max_refresh_hz.clamp(30.0, 240.0);
                    let elapsed_s = d
                        .edge_scroll_at
                        .map(|t| t.elapsed().as_secs_f64())
                        .unwrap_or(1.0 / hz);
                    if let Some((px, rem)) =
                        edge_scroll_step(elapsed_s, hz, dir, d.edge_scroll_frac)
                    {
                        d.edge_scroll_at = Some(Instant::now());
                        d.edge_scroll_frac = rem;
                        if px != 0 {
                            let before = s.offset;
                            s.scroll(px);
                            if s.offset != before {
                                self.instant = true;
                                self.dirty = true;
                            }
                        }
                    }
                } else {
                    d.edge_scroll_at = None;
                    d.edge_scroll_frac = 0.0;
                }
            }
        } else if k != d.key {
            d.edge_scroll_at = None;
            d.edge_scroll_frac = 0.0;
        }
        let _ = velocity;
        let peek = self.hot.stack_peek_px.unwrap_or(96);
        let side = self.hot.stack_zone_percent;
        // Hit-test the visible part of each window (a group member beyond its peek is clipped away).
        let target = self.strips.get(&k).and_then(|s| {
            s.rects(peek)
                .into_iter()
                .filter(|(id, _)| *id != d.id)
                .filter_map(|(id, r)| clip(r, s.area).map(|v| (id, v)))
                .find(|(_, v)| contains(*v, p))
                .map(|(id, v)| {
                    let fraction = if s.area.horizontal() {
                        (p.0 - v.x) as f64 / v.w.max(1) as f64
                    } else {
                        (p.1 - v.y) as f64 / v.h.max(1) as f64
                    };
                    let in_zone = drop_zones(s, peek, side, d.id)
                        .iter()
                        .any(|z| z.0 == id && contains(z.2, p));
                    (id, fraction, in_zone)
                })
        });
        let id = target.map(|t| t.0);
        if id != d.target {
            self.instant = true;
            self.dirty = true;
            d.hover = Instant::now();
            d.target = id
        }
        let home = self.strips.get(&d.key);
        let column = home.and_then(|s| s.locate(d.id)).map(|(i, _)| i);
        let grouped = home.zip(column).is_some_and(|(s, i)| s.columns[i].members.len() > 1);
        // Holding the drag modifier (Shift by default) makes the drop do nothing: the window goes back.
        let held = self.backend.key_down(self.hot.drag_stack_modifier_vk);
        let mode = target
            .filter(|_| !held)
            .map(|(t, fraction, in_zone)| {
                let sibling = k == d.key
                    && home.and_then(|s| s.locate(t)).map(|(i, _)| i) == column;
                planor::drag(in_zone, fraction, grouped, sibling, k != d.key)
            })
            .unwrap_or(planor::Drag::None);
        if mode != d.mode {
            d.mode = mode;
            self.action = "drag_reflow";
            self.instant = true;
            self.dirty = true
        }
    }
    fn drag_end(&mut self, id: isize) {
        let flung = std::mem::take(&mut self.flung);
        if self.drag.as_ref().is_some_and(|d| d.id != id) {
            // MOVESIZEEND arrived for a different window (e.g. a torn-off browser tab): end the
            // stale drag session cleanly without applying a drop to the source window.
            self.drag = None;
            self.force_place = true;
            self.dirty = true;
            return;
        }
        let was_instant = self.instant;
        self.drag_update_inner(false, false);
        self.instant = was_instant;
        let Some(mut d) = self.drag.take() else { return };
        self.force_place = true;
        if let Some(rec) = self.gesture_rec.as_mut() {
            // What the drop was about to do, so a recording shows Planor's answer to each gesture.
            rec.push(serde_json::json!({
                "kind": "drop", "window": d.id, "mode": format!("{:?}", if flung { planor::Drag::None } else { d.mode }),
                "target": if flung { None } else { d.target }, "flung": flung,
                "to_other_monitor": d.destination.as_ref().is_some_and(|k| k != &d.key),
            }));
        }
        self.dirty = true;
        if flung {
            // A fling only changes monitor; it never reorders or stacks onto what is under the cursor.
            d.target = None;
            d.mode = planor::Drag::None;
        }
        if id != d.id
            || self.blocked()
            || self.owning(id) != Some(d.key.clone())
            || self.backend.current_desktop() != Some(d.key.0)
        {
            return;
        }
        // The moves during a pad drag are asynchronous, so a flung window may not have moved yet.
        let Some(r) = (if flung { Some(d.original) } else { self.backend.frame(id) }) else { return };
        let s = self.strips.get_mut(&d.key).unwrap();

        // Cancelling an edge resize restores the OS frame, which can differ from the slot.
        if r == d.original && !flung {
            return;
        }
        // A resize keeps at least one horizontal edge and one vertical edge anchored (a move shifts
        // both edges on at least one axis). Dragging an edge far beyond the monitor puts the cursor on
        // another monitor; a latched resize still completes as a resize, not a transfer.
        let home = self.displays.iter().find(|m| m.0 == d.key.1).map(|m| m.2);
        let on_home = home.is_some_and(|b| {
            let w = (r.x + r.w).min(b.x + b.w) - r.x.max(b.x);
            let h = (r.y + r.h).min(b.y + b.h) - r.y.max(b.y);
            w > 0 && h > 0 && (w as i64 * h as i64) * 2 >= r.w as i64 * r.h as i64
        });
        let resized = !flung
            && !d.pad_drag
            && (d.resizing || (!d.moved && on_home && is_edge_resize(d.original, r)));
        if resized {
            let horizontal = s.area.horizontal();
            let leading = if horizontal {
                r.x != d.original.x
            } else {
                r.y != d.original.y
            };
            s.resize(
                s.locate(id).unwrap().0,
                if horizontal { r.w } else { r.h },
                leading,
            );
            self.action = "resize";
            self.reveal();
            return;
        }
        let Some(dest) = d.destination else { return };
        if self.displays != self.backend.monitors() {
            self.deadline = Some(Instant::now() + Duration::from_millis(1500));
            return;
        }
        let Some((_, area, _)) = self.displays.iter().find(|m| m.0 == dest.1).cloned() else {
            return;
        };
        if let Some(target) = d.target
            && (!self.backend.is_window(target) || self.owning(target) != Some(dest.clone()))
        {
            return;
        }
        if let (planor::Drag::Swap, Some(target), true) = (d.mode, d.target, dest == d.key) {
            // Dropped on a window of the same monitor (not in its stack zone): the two exchange places.
            // Within a group this reorders the group.
            self.strips.get_mut(&dest).unwrap().swap(id, target);
            self.active = dest.1.clone();
            self.strips.get_mut(&dest).unwrap().select(id);
            self.action = "drag_reflow";
            self.reveal();
            return;
        }
        if dest != d.key {
            if self.backend.minimum(id, true) > area.w
                || self.backend.minimum(id, false) > area.h
                || !self.transfer_dialogs(id, d.original, area)
            {
                return;
            }
            self.strips.get_mut(&d.key).unwrap().remove(id);
            self.strips.get_mut(&d.key).unwrap().fill();
            let s = self
                .strips
                .entry(dest.clone())
                .or_insert_with(|| Strip::new(area));
            s.insert(id, self.backend.minimum(id, area.horizontal()))
        }
        if dest != d.key {
            // Crossing monitors: snap into the new slot at once, behind a placeholder cover while
            // the app handles WM_DPICHANGED and finishes repainting.
            self.instant = true;
            self.mark_cover(id);
        }
        self.active = dest.1.clone();
        let s = self.strips.get_mut(&dest).unwrap();
        if let Some(target) = d.target {
            match d.mode {
                planor::Drag::Stack => s.group(id, target),
                planor::Drag::Before | planor::Drag::After => {
                    s.reorder(id, target, d.mode == planor::Drag::After)
                }
                _ => (),
            }
        }
        s.select(id);
        self.action = "drag_reflow";
        self.reveal()
    }
    fn transfer_dialogs(&mut self, id: isize, owner: Rect, area: Rect) -> bool {
        let mut moves = vec![];
        for dlg in self.backend.windows() {
            if self.backend.owner(dlg) == id {
                let Some(r) = self.backend.frame(dlg) else {
                    return false;
                };
                if r.w > area.w || r.h > area.h || self.journal.capture(dlg, &self.backend).is_err() {
                    return false;
                }
                moves.push((
                    dlg,
                    Rect {
                        x: (area.x + r.x - owner.x).clamp(area.x, area.x + area.w - r.w),
                        y: (area.y + r.y - owner.y).clamp(area.y, area.y + area.h - r.h),
                        ..r
                    },
                ))
            }
        }
        moves.is_empty() || self.backend.place(&moves).is_ok()
    }
    fn window_state(&mut self, id: isize) {
        if self.drag.is_some() {
            return;
        }
        let Some(key) = self.owning(id) else { return };
        let Some((_, _, bounds)) = self.displays.iter().find(|m| m.0 == key.1) else {
            return;
        };
        let s = self.strips.get_mut(&key).unwrap();
        if self.backend.is_zoomed(id) {
            self.backend.restore_window(id);
            s.select(id);
            s.maximize();
            if !self.zoomed.remove(&id) {
                self.zoomed.insert(id);
            }
            self.reveal();
            return;
        }
        let Some(r) = self.backend.frame(id) else { return };
        let full = r == *bounds && !self.backend.has_caption(id);
        let existing = s.fullscreen_id() == Some(id);
        if full != existing && (full || self.backend.has_caption(id)) {
            s.select(id);
            s.fullscreen(full, *bounds);
            // The app resizes itself to full screen at the same moment; animating the strip as well
            // made the two fight and the transition lag.
            self.instant = true;
            self.reveal();
            return;
        }
        self.hold_newborn(id, r)
    }
    pub(crate) fn event(&mut self, e: Event) {
        match e {
            Event::Peek(id) if !self.blocked() => {
                self.action = "scroll";
                self.select(id);
                self.reveal();
                self.backend.activate(id)
            }
            Event::Rejected(seq, entries) if seq == self.seq && !self.blocked() => {
                for (id, r) in entries {
                    if let Some(k) = self.owning(id)
                        && self.placed.get(&id).is_some_and(|(n, _)| *n == seq)
                    {
                        let s = self.strips.get_mut(&k).unwrap();
                        let i = s.locate(id).unwrap().0;
                        let length = if s.area.horizontal() { r.w } else { r.h };
                        if length > s.columns[i].length {
                            s.resize(i, length, false);
                            self.force_place = true;
                            self.dirty = true
                        }
                    }
                }
            }
            Event::Frame(f) => {
                if self.diag.enabled {
                    self.diag.record(f);
                    if let Some(dir) = self.backend.frames_log_dir() {
                        let _ = diag::append(
                            &dir,
                            &serde_json::to_vec(&f).unwrap_or_default(),
                            self.hot.max_log_bytes,
                            self.hot.max_log_files,
                        );
                    }
                }
            }
            Event::Suspend(v) => {
                self.clear_scroll_state();
                let _ = self.clear();
                self.locked = v;
                if !v {
                    self.deadline = Some(Instant::now() + Duration::from_millis(1500))
                }
            }
            Event::Topology => {
                self.clear_scroll_state();
                self.drag = None;
                let _ = self.clear();
                self.deadline = Some(Instant::now() + Duration::from_millis(1500))
            }
            Event::Key(k) => {
                self.input = Some(Instant::now());
                self.input_qpc_us = Some(perf::qpc_us());
                self.keypress(&k);
                if !self.dirty {
                    self.input = None;
                    self.input_qpc_us = None;
                }
            }
            Event::Window(EVENT_OBJECT_DESTROY,id) if self.blocked()=>{if let Some(k)=self.owning(id){self.strips.get_mut(&k).unwrap().remove(id);}self.pinned.remove(&id);self.orphans.remove(&id);self.skipped.remove(&id);self.covering.remove(&id);self.dirty=true;},
            Event::Window(e, id) if !self.blocked() => match e {
                EVENT_OBJECT_SHOW | EVENT_SYSTEM_MINIMIZEEND => {
                    if self.hot.spawn_on_cursor_monitor
                        && e == EVENT_OBJECT_SHOW
                    {
                        let (px, py) = self.backend.cursor_pos();
                        self.spawn_hint = self
                            .displays
                            .iter()
                            .find(|(_, _, r)| px >= r.x && px < r.x + r.w && py >= r.y && py < r.y + r.h)
                            .map(|m| m.0.clone());
                    }
                    self.add(id);
                    self.spawn_hint = None
                }
                // While a touchpad scroll or drag is in progress it owns the focus; the normal
                // "reveal the focused window" reaction would fight it (and snap the view back).
                EVENT_SYSTEM_FOREGROUND if self.pad_scrolling || self.settle.is_some() || self.pad.is_some() => (),
                EVENT_SYSTEM_FOREGROUND => {
                    if id != 0 {
                        self.null_fg_ticks = 0;
                    }
                    let target = self.managed_foreground_id(id);
                    if self.diag.enabled {
                        diag::trace(format!("FG {id} managed={} external={} pad={}", target.is_some(), self.backend.external(id), self.pad_scrolling));
                    }
                    let active_id = self
                        .key()
                        .and_then(|k| self.strips.get(&k))
                        .and_then(|s| s.active());
                    // Focus moving to or from an unmanaged window changes the border and the overlay order.
                    self.dirty = true;
                    if let Some(managed_id) = target
                        && active_id != Some(managed_id)
                    {
                        // A window (or its owned dialog) focused by any route must be selected and
                        // visible: otherwise focus lands on a parked window or highlights the wrong tile.
                        self.select(managed_id);
                        let visible = self
                            .owning(managed_id)
                            .and_then(|k| self.strips.get(&k))
                            .is_some_and(|s| s.in_view(managed_id));
                        if visible {
                            // Already on screen (e.g. a click on a visible tile): only move the border.
                            self.dirty = true
                        } else {
                            self.action = "scroll";
                            self.reveal()
                        }
                    }
                }
                // A window that is being closed hides itself a moment before it is destroyed; reacting to the hide
                // (not only the destroy) takes its border away and starts the neighbour sliding in at once.
                EVENT_OBJECT_HIDE
                    if self.backend.is_visible(id)
                        || self.backend.is_iconic(id)
                        || self.owning(id).is_none() => {}
                EVENT_OBJECT_DESTROY | EVENT_SYSTEM_MINIMIZESTART | EVENT_OBJECT_HIDE => {
                    if self.drag.as_ref().is_some_and(|d| d.id == id) {
                        self.drag = None;
                        self.dirty = true;
                    }
                    self.covering.remove(&id);
                    if !self.orphans.contains(&id) {
                        if let Some(k) = self.owning(id) {
                            let focused =
                                self.strips[&k].active() == Some(id) && k.1 == self.active;
                            self.strips.get_mut(&k).unwrap().remove(id);
                            self.strips.get_mut(&k).unwrap().fill();
                            if focused
                                && let Some(next) = self.strips[&k].active()
                            {
                                self.backend.activate(next)
                            }
                            self.placed.remove(&id);
                            self.action = "open_close";
                            self.reveal()
                        }
                        if e == EVENT_OBJECT_DESTROY {
                            self.skipped.remove(&id);
                            self.pinned.remove(&id);
                        }
                    }
                }
                EVENT_SYSTEM_MOVESIZESTART => self.drag_begin(id),
                EVENT_SYSTEM_MOVESIZEEND => self.drag_end(id),
                EVENT_OBJECT_LOCATIONCHANGE => self.window_state(id),
                _ => (),
            },
            Event::Wheel { delta, tilt, shift, x, y } => self.wheel(delta, tilt, shift, (x, y)),
            Event::Button { button, down, x, y } => {
                if let Some(rec) = self.gesture_rec.as_mut() {
                    rec.push(serde_json::json!({ "kind": "button", "button": button, "down": down, "x": x, "y": y }));
                }
            }
            Event::Contacts(p)
                if !self.blocked()
                    && self.hot.three_finger_enabled =>
            {
                self.input = Some(Instant::now());
                self.input_qpc_us = Some(perf::qpc_us());
                self.contacts(p);
                // Only keep input timestamps that actually produced a layout scene; otherwise a
                // deadzone/no-op move would bill its stale age to an unrelated scene tens of ms later.
                if !self.dirty {
                    self.input = None;
                    self.input_qpc_us = None;
                }
            }
            _ => (),
        }
    }
}
fn clip(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let w = (a.x + a.w).min(b.x + b.w) - x;
    let h = (a.y + a.h).min(b.y + b.h) - y;
    (w > 0 && h > 0).then_some(Rect { x, y, w, h })
}
fn contains(r: Rect, p: (i32, i32)) -> bool {
    p.0 >= r.x && p.0 < r.x + r.w && p.1 >= r.y && p.1 < r.y + r.h
}
/// Universal strip-scroll sensitivity: fraction of the active monitor's strip span traversed per
/// inch of physical finger travel at `swipe_scroll_gain = 1.0` (`0.0375 * 4.0 = 15%` of viewport
/// per inch). Derived dynamically from the contact frame's physical DPI (`None` for raw-unit
/// digitizers, which stay independent of `cursor_dpi`) and the active strip's live span.
const STRIP_SCROLL_VIEWPORT_PER_INCH_PER_GAIN: f64 = 0.0375;
/// Universal group-roll sensitivity: deck steps per inch of physical finger travel at
/// `swipe_scroll_gain = 1.0` (`0.138 * 4.0 ≈ 0.552` deck steps/inch, matching the external
/// landscape reference feel across all display resolutions and DPIs).
const GROUP_ROLL_STEPS_PER_INCH_PER_GAIN: f64 = 0.138;

pub(crate) fn strip_scroll_px(signed_delta: f64, area_len: i32, contact_dpi: Option<f64>) -> f64 {
    let inches = signed_delta / contact_dpi.unwrap_or(96.0).max(1.0);
    inches * area_len.max(1) as f64 * STRIP_SCROLL_VIEWPORT_PER_INCH_PER_GAIN
}

pub(crate) fn group_roll_delta(signed_delta: f64, contact_dpi: Option<f64>) -> f64 {
    let inches = signed_delta / contact_dpi.unwrap_or(96.0).max(1.0);
    inches * GROUP_ROLL_STEPS_PER_INCH_PER_GAIN
}

/// Compute touchpad release velocity (finger units/s) from `(age_s, delta)` samples within the
/// recent 100 ms window. Requires at least 2 samples and a recent newest sample (`<= 40 ms`, so a
/// pause before lifting never counts as a fling), excluding the oldest sample's delta so `n - 1`
/// deltas are divided by `n - 1` intervals symmetrically in both directions.
pub(crate) fn scroll_fling_velocity(samples: &[(f64, f64)]) -> f64 {
    if samples.len() < 2 {
        return 0.0;
    }
    let newest = samples.iter().map(|&(a, _)| a).fold(f64::INFINITY, f64::min);
    if newest > 0.04 {
        return 0.0;
    }
    let oldest = samples.iter().map(|&(a, _)| a).fold(0.0f64, f64::max);
    let sum: f64 = samples[1..].iter().map(|&(_, d)| d).sum();
    let span = (oldest - newest).clamp(0.03, 0.10);
    sum / span
}

/// Direction (`+1` next, `-1` prev) to advance on lift when a touchpad swipe exceeds both the
/// fling speed threshold and minimum travel, honoring `natural` scroll direction symmetrically.
pub(crate) fn fling_advance_direction(
    speed: f64,
    total_travel: f64,
    natural: bool,
    threshold: f64,
    min_travel: f64,
) -> Option<isize> {
    if speed.abs() < threshold || total_travel.abs() < min_travel {
        return None;
    }
    let signed = if natural { -speed } else { speed };
    Some(if signed > 0.0 { 1 } else { -1 })
}

/// Smooth time-based edge-scroll step during a drag: at most one step per display refresh,
/// advancing at a steady pixel/second velocity with fractional carry and per-tick clamping.
pub(crate) fn edge_scroll_step(
    elapsed_s: f64,
    refresh_hz: f64,
    dir: i32,
    carry: f64,
) -> Option<(i32, f64)> {
    const EDGE_SCROLL_PX_PER_SEC: f64 = 900.0;
    if dir == 0 {
        return None;
    }
    let hz = refresh_hz.clamp(30.0, 240.0);
    let min_dt = 1.0 / hz;
    if elapsed_s + 1e-6 < min_dt {
        return None;
    }
    let dt = elapsed_s.min(0.05);
    let raw = carry + dir as f64 * EDGE_SCROLL_PX_PER_SEC * dt;
    let px = raw.trunc() as i32;
    Some((px, raw - px as f64))
}

/// True when `current` looks like an edge or corner resize of `original`: width or height changed
/// by more than 2 px while at least one horizontal edge AND at least one vertical edge stayed
/// anchored within 2 px. Rejects cross-DPI moves and drag-restore of snapped windows (where both
/// horizontal edges shift).
pub(crate) fn is_edge_resize(original: Rect, current: Rect) -> bool {
    let changed = |a: i32, b: i32| (a - b).abs() > 2;
    let same = |a: i32, b: i32| (a - b).abs() <= 2;
    let x_anchored = same(current.x, original.x)
        || same(current.x + current.w, original.x + original.w);
    let y_anchored = same(current.y, original.y)
        || same(current.y + current.h, original.y + original.h);
    (changed(current.w, original.w) || changed(current.h, original.h))
        && x_anchored
        && y_anchored
}

/// Subtract every rectangle in `blockers` from `r`, returning the remaining non-empty
/// non-overlapping pieces. Used for Z-order-aware overlay hole occlusion.
pub(crate) fn subtract_rects(r: Rect, blockers: &[Rect]) -> Vec<Rect> {
    if r.w <= 0 || r.h <= 0 {
        return Vec::new();
    }
    let mut pieces = vec![r];
    for &b in blockers {
        if b.w <= 0 || b.h <= 0 {
            continue;
        }
        let mut next = Vec::with_capacity(pieces.len());
        for p in pieces {
            let Some(ov) = clip(p, b) else {
                next.push(p);
                continue;
            };
            // Top strip
            if ov.y > p.y {
                next.push(Rect { x: p.x, y: p.y, w: p.w, h: ov.y - p.y });
            }
            // Bottom strip
            let p_bot = p.y + p.h;
            let ov_bot = ov.y + ov.h;
            if p_bot > ov_bot {
                next.push(Rect { x: p.x, y: ov_bot, w: p.w, h: p_bot - ov_bot });
            }
            // Left middle strip
            if ov.x > p.x {
                next.push(Rect { x: p.x, y: ov.y, w: ov.x - p.x, h: ov.h });
            }
            // Right middle strip
            let p_right = p.x + p.w;
            let ov_right = ov.x + ov.w;
            if p_right > ov_right {
                next.push(Rect { x: ov_right, y: ov.y, w: p_right - ov_right, h: ov.h });
            }
        }
        pieces = next;
        if pieces.is_empty() {
            break;
        }
    }
    pieces
}
/// The stack drop zones of a strip while `dragged` is carried: one small box in the middle of the visible
/// part of every column's active window (not the dragged window's own column). (window, visible rect, zone)
pub(crate) fn drop_zones(s: &Strip, peek: i32, side: i32, dragged: isize) -> Vec<(isize, Rect, Rect)> {
    let own = s.locate(dragged).map(|(i, _)| i);
    s.rects(peek)
        .into_iter()
        .filter(|(id, _)| {
            *id != dragged
                && s.locate(*id).is_some_and(|(i, j)| Some(i) != own && s.columns[i].active == j)
        })
        .filter_map(|(id, r)| clip(r, s.area).map(|v| (id, v, planor::stack_zone(v, side))))
        .collect()
}
/// Log any step of the daemon loop that blocks for noticeable time (finds UI stalls).
fn slow(what: &str, since: Instant) {
    let ms = since.elapsed().as_millis();
    if ms >= 100 {
        diag::trace(format!("SLOW daemon {what}: {ms} ms"));
    }
}
pub fn run() -> Result<()> {
    tracing::info!("starting Planor");
    let _com = unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
    };
    let mutex = unsafe { CreateMutexW(null(), 0, wide(&format!("Local\\Planor.Daemon{}", instance())).as_ptr()) };
    if mutex.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err("daemon already running or unavailable".into());
    }
    let _mutex = unsafe { File::from_raw_handle(mutex) };
    let backend = Win32Backend;
    let mut server = PipeServer::new(&pipe_name())?;
    let mut journal = Journal::open(folder("LOCALAPPDATA")?.join("recovery.json"))?;
    journal.restore(&backend)?;
    win32::tweak_system(&folder("LOCALAPPDATA")?.join("system-settings.json"));
    let cfg = config()?;
    if !crate::test_mode() {
        // Hand edits of planor.toml apply live, like changes made in the settings window.
        crate::config_watch::spawn(folder("APPDATA")?.join("planor.toml"), || {
            let _ = client(&pipe_name(), &Command::Reload);
        });
    }
    crate::tray::spawn();
    let diag = diag::Telemetry::new(cfg["diagnostics"]["enabled"].as_bool().unwrap());
    perf::init(diag.enabled);
    perf::register_thread("daemon");
    let _watchdog = watchdog::Watchdog::start(diag.enabled, crate::test_mode());
    if diag.enabled && !crate::test_mode() {
        record_input();
    }
    let (tx, events) = crossbeam_channel::unbounded();
    let (reload, rx) = crossbeam_channel::unbounded();
    win32::events(tx, cfg.clone(), rx);
    let desktop = backend.current_desktop();
    let max_refresh_hz = backend.max_refresh_hz();
    let hot = HotCfg::from_toml(&cfg);
    let mut engine = Engine {
        backend,
        pad: None,
        pad_scrolling: false,
        settle: None,
        scroll_frac: 0.,
        renderer: render::start(),
        action: "open_close",
        cfg,
        hot,
        journal,
        diag,
        strips: Default::default(),
        displays: vec![],
        displays_arc: Arc::from([]),
        desktop,
        active: String::new(),
        pinned: Default::default(),
        orphans: Default::default(),
        homes: Default::default(),
        skipped: Default::default(),
        paused: false,
        locked: false,
        deadline: None,
        dirty: false,
        instant: false,
        force_place: false,
        flung: false,
        spawn_hint: None,
        quick: false,
        last_holes: vec![],
        last_holes_arc: Arc::from([]),
        last_tile_geometry: vec![],
        last_had_zones: false,
        overlay_seq: 0,
        last_foreground: 0,
        null_fg_ticks: 0,
        holes_checked: Instant::now(),
        monitors_checked: Instant::now(),
        desktop_checked: Instant::now(),
        scroll_samples: Default::default(),
        scroll_start_focus: None,
        scroll_dpi: None,
        wheel_mode: None,
        inertia: None,
        v_travel: 0.,
        hopped: false,
        born: HashMap::new(),
        gesture_rec: None,
        gesture: Default::default(),
        seq: 0,
        input: None,
        input_qpc_us: None,
        started: Instant::now(),
        config_version: 1,
        zoomed: Default::default(),
        drag: None,
        last_scan: Instant::now(),
        placed: Default::default(),
        covering: HashMap::new(),
        max_refresh_hz,
    };
    engine.topology();
    if engine.cfg["layout"]["tile_existing_on_startup"].as_bool() == Some(true) {
        engine.scan()
    } else {
        let existing = engine.backend.windows();
        engine.skipped.extend(existing)
    }
    let tick = crossbeam_channel::tick(Duration::from_millis(32));
    // Frame-rate steps while something moves by itself (momentum, edge scroll during a drag); a
    // never-firing channel otherwise, so an idle daemon does not wake unnecessarily.
    let mut fast_ms = planor::pacing::fast_tick_ms(engine.max_refresh_hz);
    let mut fast = crossbeam_channel::tick(Duration::from_millis(fast_ms));
    let never = crossbeam_channel::never::<Instant>();
    let mut _hi_res: Option<win32::HighResTimer> = None;
    let mut last_ipc_poll = Instant::now() - Duration::from_millis(10);
    let result = (|| -> Result<()> {
        loop {
            perf::heartbeat_daemon();
            engine.layout()?;
            let ipc_done = if server.connected
                || server.has_pending_write()
                || last_ipc_poll.elapsed() >= Duration::from_millis(4)
            {
                last_ipc_poll = Instant::now();
                server.poll(|cmd| engine.command(cmd, &reload))?
            } else {
                false
            };
            if ipc_done {
                break;
            }
            if engine.dirty && !server.closing {
                engine.layout()?;
            }
            if server.has_pending_write() {
                while let Ok(e) = events.try_recv() {
                    match e {
                        Event::Stop => return Ok(()),
                        Event::Key(k) if k == "panic_exit" => return Ok(()),
                        e if !server.closing => {
                            let t = Instant::now();
                            let name = e.name();
                            {
                                let _span = perf::SpanGuard::new(perf::SpanKind::DaemonEvent);
                                engine.event(e);
                            }
                            slow(name, t);
                        }
                        _ => (),
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            let want_ms = planor::pacing::fast_tick_ms(engine.max_refresh_hz);
            if want_ms != fast_ms {
                fast_ms = want_ms;
                fast = crossbeam_channel::tick(Duration::from_millis(fast_ms));
            }
            if engine.wants_fast_tick() {
                if _hi_res.is_none() {
                    _hi_res = Some(win32::HighResTimer::acquire());
                }
            } else {
                _hi_res = None;
            }
            crossbeam_channel::select! {recv(events)->e=>match e?{Event::Stop=>break,Event::Key(k) if k=="panic_exit"=>break,e if !server.closing=>{let t=Instant::now();let name=e.name();if engine.diag.enabled&&!matches!(name,"frame"|"contacts"){let detail=match &e{Event::Window(k,id)=>format!(" {k:#x} {id}"),Event::Rejected(s,v)=>format!(" seq={s} n={} first={:?}",v.len(),v.first()),Event::Peek(id)=>format!(" {id}"),_=>String::new()};diag::trace(format!("EV {name}{detail}"))}let _span=perf::SpanGuard::new(perf::SpanKind::DaemonEvent);engine.event(e);slow(name,t)},_=>()},recv(tick)->_=>{if !server.closing{let t=Instant::now();let _span=perf::SpanGuard::new(perf::SpanKind::DaemonTick);engine.tick()?;slow("tick",t)}},recv(if engine.wants_fast_tick(){&fast}else{&never})->_=>{if !server.closing{engine.fast_tick()}}}
        }
        Ok(())
    })();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let _ = engine.renderer.send(render::Message::Stop(tx));
    let _ = rx.recv();
    let restored = engine.journal.restore(&engine.backend);
    if let Ok(dir) = folder("LOCALAPPDATA") {
        win32::restore_system(&dir.join("system-settings.json"));
    }
    win32::SHUTDOWN_DONE.store(true, std::sync::atomic::Ordering::Release);
    restored?;
    result
}
/// Rescue path for `planor stop` when no daemon is running: restore every window recorded by a
/// previous (crashed or force-killed) session and delete the journal.
pub fn recover() -> Result<usize> {
    win32::restore_system(&folder("LOCALAPPDATA")?.join("system-settings.json"));
    let path = folder("LOCALAPPDATA")?.join("recovery.json");
    if !path.exists() {
        return Ok(0);
    }
    let mut journal = Journal::open(path)?;
    let count = journal.entries.len();
    journal.restore(&Win32Backend)?;
    Ok(count)
}
/// Diagnostics: records the cursor position, mouse buttons and non-text keys with the same timestamps as
/// the rest of the trace, so a black screen or flicker can be lined up with exactly what was done.
/// Polled (no extra hook, so it cannot slow input). Keys that type text are only counted, never named:
/// the log must not become a record of what was typed.
fn record_input() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    std::thread::spawn(|| {
        let mut down = [false; 256];
        let mut at = (i32::MIN, i32::MIN);
        let mut chars = 0u32;
        loop {
            std::thread::sleep(Duration::from_millis(15));
            let mut p = POINT::default();
            if unsafe { GetCursorPos(&mut p) } != 0 && (p.x, p.y) != at {
                at = (p.x, p.y);
                diag::trace(format!("IN mouse {} {}", p.x, p.y));
            }
            for (vk, slot) in down.iter_mut().enumerate().take(255).skip(1) {
                let now = unsafe { GetAsyncKeyState(vk as i32) } < 0;
                if now == *slot {
                    continue;
                }
                *slot = now;
                let text = matches!(vk, 0x30..=0x5A | 0xBA..=0xC0 | 0xDB..=0xDF | 0x60..=0x6F | 0x20);
                if text {
                    if now {
                        chars += 1;
                    }
                    continue;
                }
                if chars > 0 {
                    diag::trace(format!("IN text-keys x{chars}"));
                    chars = 0;
                }
                let name = match vk {
                    1 => "mouse-left".to_string(),
                    2 => "mouse-right".to_string(),
                    4 => "mouse-middle".to_string(),
                    5 => "mouse-x1".to_string(),
                    6 => "mouse-x2".to_string(),
                    _ => format!("vk{vk:#04x}"),
                };
                diag::trace(format!("IN {} {name}", if now { "down" } else { "up" }));
            }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_journal() {
        let path = std::env::temp_dir().join(format!("journal-{}.json", std::process::id()));
        let mut j = Journal::open(path.clone()).unwrap();
        j.entries.insert(
            -1,
            Saved {
                pid: 0,
                cookie: 2,
                flags: 0,
                show: 1,
                min: [0; 2],
                max: [0; 2],
                normal: [0; 4],
                rect: [0; 4],
                topmost: false,
            },
        );
        j.save().unwrap();
        Journal::open(path.clone()).unwrap().restore(&Win32Backend).unwrap();
        assert!(!path.exists())
    }
}

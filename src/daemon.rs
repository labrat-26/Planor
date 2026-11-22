use super::*;
use std::collections::BTreeMap;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Saved {
    pid: u32,
    cookie: usize,
    flags: u32,
    show: u32,
    min: [i32; 2],
    max: [i32; 2],
    normal: [i32; 4],
    rect: [i32; 4],
    topmost: bool,
}
struct Journal {
    path: PathBuf,
    entries: BTreeMap<isize, Saved>,
}
impl Journal {
    fn open(path: PathBuf) -> Result<Self> {
        let entries = match fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, entries })
    }
    fn save(&self) -> Result<()> {
        if self.entries.is_empty() {
            if self.path.exists() {
                fs::remove_file(&self.path)?
            }
            return Ok(());
        }
        let tmp = self.path.with_extension("tmp");
        let mut f = File::create(&tmp)?;
        serde_json::to_writer(&mut f, &self.entries)?;
        f.sync_all()?;
        drop(f);
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
    fn capture(&mut self, id: isize) -> Result<()> {
        unsafe {
            let hwnd = id as HWND;
            let tag = wide("Planor.Recovery.Identity");
            if self
                .entries
                .get(&id)
                .is_some_and(|s| s.cookie == GetPropW(hwnd, tag.as_ptr()) as usize)
            {
                return Ok(());
            }
            let mut p = WINDOWPLACEMENT::default();
            p.length = size_of::<WINDOWPLACEMENT>() as _;
            let mut r = RECT::default();
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if GetWindowPlacement(hwnd, &mut p) == 0 || GetWindowRect(hwnd, &mut r) == 0 {
                return Err(io::Error::last_os_error().into());
            }
            let cookie = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos() as usize)
                | 1;
            let n = p.rcNormalPosition;
            self.entries.insert(
                id,
                Saved {
                    pid,
                    cookie,
                    flags: p.flags,
                    show: p.showCmd,
                    min: [p.ptMinPosition.x, p.ptMinPosition.y],
                    max: [p.ptMaxPosition.x, p.ptMaxPosition.y],
                    normal: [n.left, n.top, n.right, n.bottom],
                    rect: [r.left, r.top, r.right, r.bottom],
                    topmost: GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0,
                },
            );
            self.save()?;
            if SetPropW(hwnd, tag.as_ptr(), cookie as HANDLE) == 0 {
                return Err(io::Error::last_os_error().into());
            }
            Ok(())
        }
    }
    fn restore(&mut self) -> Result<()> {
        let tag = wide("Planor.Recovery.Identity");
        self.entries.retain(|&id, s| unsafe {
            let h = id as HWND;
            let mut pid = 0;
            GetWindowThreadProcessId(h, &mut pid);
            if pid != s.pid || GetPropW(h, tag.as_ptr()) as usize != s.cookie {
                return false;
            }
            let [left, top, right, bottom] = s.normal;
            let p = WINDOWPLACEMENT {
                length: size_of::<WINDOWPLACEMENT>() as _,
                flags: s.flags,
                showCmd: s.show,
                ptMinPosition: POINT {
                    x: s.min[0],
                    y: s.min[1],
                },
                ptMaxPosition: POINT {
                    x: s.max[0],
                    y: s.max[1],
                },
                rcNormalPosition: RECT {
                    left,
                    top,
                    right,
                    bottom,
                },
            };
            if SetWindowPlacement(h, &p) == 0 {
                return true;
            }
            let [x, y, r, b] = s.rect;
            let flags = SWP_NOACTIVATE
                | if s.show == SW_SHOWNORMAL as u32 {
                    0
                } else {
                    SWP_NOMOVE | SWP_NOSIZE
                };
            if SetWindowPos(
                h,
                if s.topmost {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                },
                x,
                y,
                r - x,
                b - y,
                flags,
            ) == 0
            {
                return true;
            }
            RemovePropW(h, tag.as_ptr());
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
    time::{Duration, Instant},
};
use win32::Event;
type Key = (u128, String);
struct DragSession {
    id: isize,
    key: Key,
    original: Rect,
    point: POINT,
    sampled: Instant,
    hover: Instant,
    target: Option<isize>,
    mode: planor::Drag,
    destination: Option<Key>,
}
/// A window being carried by a four-finger touchpad swipe. Planor moves it itself (there is no OS move
/// loop without a mouse button) and moves the cursor with it, so the normal drag logic sees an
/// ordinary drag.
struct PadDrag {
    id: isize,
    /// Cursor position minus the window's top-left corner, fixed for the whole drag.
    grab: (f64, f64),
    cursor: (f64, f64),
    /// Union of all monitors (left, top, right, bottom): the cursor, and so the window, never leaves it.
    limits: (f64, f64, f64, f64),
    seen: Instant,
    /// Smoothed finger speed (units/s) for the acceleration; a raw per-report speed is noisy, which made
    /// the same swipe move the window by different amounts.
    speed: f64,
    /// When the real window was last moved: moves are rate-limited so they cannot queue up behind a busy app.
    moved: Instant,
    /// Recent raw finger deltas, newest last, for the lift-off velocity (fling).
    samples: std::collections::VecDeque<(Instant, f64, f64)>,
}
struct Engine {
    pad: Option<PadDrag>,
    pad_scrolling: bool,
    /// When set, a finished touchpad scroll settles (reveal + focus) at this time unless scrolling resumes.
    settle: Option<Instant>,
    scroll_frac: f64,
    renderer: crossbeam_channel::Sender<render::Message>,
    action: &'static str,
    cfg: toml::Value,
    journal: Journal,
    strips: BTreeMap<Key, Strip>,
    displays: Vec<(String, Rect, Rect)>,
    desktop: Option<u128>,
    active: String,
    pinned: HashSet<isize>,
    orphans: HashSet<isize>,
    /// Monitor each orphan lived on before its monitor was detached. `pull-orphans` may move the window onto
    /// another monitor; when its own monitor is attached again it goes back there.
    homes: HashMap<isize, String>,
    skipped: HashSet<isize>,
    paused: bool,
    locked: bool,
    deadline: Option<Instant>,
    dirty: bool,
    instant: bool,
    flung: bool,
    spawn_hint: Option<String>,
    quick: bool,
    last_holes: Vec<Rect>,
    holes_checked: Instant,
    scroll_samples: std::collections::VecDeque<(Instant, f64, bool)>,
    inertia: Option<(bool, f64, Instant)>,
    v_travel: f64,
    gesture: Gesture,
    seq: u64,
    input: Option<Instant>,
    zoomed: HashSet<isize>,
    drag: Option<DragSession>,
    last_scan: Instant,
    placed: HashMap<isize, (u64, Rect)>,
    diag: diag::Telemetry,
}
fn desktop() -> Option<u128> {
    winvd::get_current_desktop()
        .ok()?
        .get_id()
        .ok()
        .map(|g| g.to_u128())
}
impl Engine {
    fn key(&self) -> Option<Key> {
        Some((self.desktop?, self.active.clone()))
    }
    fn owning(&self, id: isize) -> Option<Key> {
        self.strips
            .iter()
            .find(|(_, s)| s.locate(id).is_some())
            .map(|(k, _)| k.clone())
    }
    fn blocked(&self) -> bool {
        self.paused || self.locked || self.deadline.is_some() || self.desktop.is_none()
    }
    fn reveal(&mut self) {
        let width = if self.cfg["layout"]["peek_enabled"].as_bool() == Some(true) {
            self.cfg["layout"]["peek_width_px"].as_integer().unwrap() as i32
                + self.cfg["layout"]["gap_px"].as_integer().unwrap() as i32
        } else {
            0
        };
        if let Some(s) = self.key().and_then(|k| self.strips.get_mut(&k)) {
            s.reveal(width)
        }
        self.dirty = true
    }
    fn add(&mut self, id: isize) {
        if self.blocked()
            || self.owning(id).is_some()
            || self.skipped.contains(&id)
            || !win32::eligible(id, &self.cfg)
        {
            return;
        }
        if winvd::is_window_on_current_desktop(windows58::Win32::Foundation::HWND(id as _)).ok()
            != Some(true)
        {
            return;
        }
        let hint = self.spawn_hint.take();
        let Some(rect) = win32::frame(id) else { return };
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
        if self.journal.capture(id).is_err() {
            self.skipped.insert(id);
            return;
        }
        self.active = name.clone();self.action="open_close";
        let s = self
            .strips
            .entry((self.desktop.unwrap(), name))
            .or_insert_with(|| Strip::new(area));
        s.gap = self.cfg["layout"]["gap_px"].as_integer().unwrap() as _;
        s.split = self.cfg["layout"]["first_pair_split"].as_bool().unwrap();
        s.share = self.cfg["layout"]["default_tile_share"].as_float().unwrap();
        s.insert(id, win32::minimum(id, area.horizontal()));
        self.reveal()
    }
    fn command(
        &mut self,
        cmd: Command,
        reload: &crossbeam_channel::Sender<toml::Value>,
    ) -> Result<String> {
        match cmd {
            Command::Stop => {
                self.paused = true;
                self.clear()?;
                self.journal.restore()?
            }
            Command::Reload => {
                self.cfg = config()?;
                reload.send(self.cfg.clone())?;
                for s in self.strips.values_mut() {
                    s.gap = self.cfg["layout"]["gap_px"].as_integer().unwrap() as _
                }
                self.reveal()
            }
            Command::Pause => {
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
                    unsafe {
                        ShowWindow(id as _, SW_SHOWNOACTIVATE);
                    }
                    // Stays in `homes`: it goes back to its own monitor once that one is attached again.
                    let minimum = win32::minimum(id, area.horizontal());
                    self.home_strip(dest.clone(), area).insert(id, minimum);
                }
                self.reveal()
            }
            Command::Diag { action } => return self.diag.command(action),
            _ => return Err("command unavailable".into()),
        }
        Ok("ok".into())
    }
    /// Rectangles of unmanaged app windows on a monitor. Test daemons share the desktop with the developer's
    /// own windows, which they treat as unmanaged; carving those out would hide every border they draw, so an
    /// isolated instance carves out only windows of the class named in PLANOR_HOLES (none if unset).
    fn holes(&self) -> Vec<Rect> {
        let only = if instance().is_empty() {
            None
        } else {
            match std::env::var("PLANOR_HOLES") {
                Ok(class) => Some(class),
                Err(_) => return vec![],
            }
        };
        win32::windows()
            .into_iter()
            .filter(|&w| self.owning(w).is_none() && win32::external(w))
            .filter(|&w| only.as_ref().is_none_or(|c| win32::class_of(w) == *c))
            .filter_map(win32::frame)
            .filter(|r| {
                self.displays.iter().any(|m| {
                    r.x < m.2.x + m.2.w && r.x + r.w > m.2.x && r.y < m.2.y + m.2.h && r.y + r.h > m.2.y
                })
            })
            .collect()
    }
    fn tick(&mut self) -> Result<()> {
        if self.locked {
            return Ok(());
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
        if self.deadline.is_none() && self.displays != win32::monitors() {
            self.deadline = Some(Instant::now() + Duration::from_millis(1500));
        }
        if self.deadline.is_some_and(|t| Instant::now() >= t) {
            self.deadline = None;
            self.topology();
            self.scan()
        }
        let d = desktop();
        if d != self.desktop {
            self.clear()?;
            self.desktop = d;
            self.placed.clear();
            self.instant = true;
            self.scan()
        }
        self.refresh();
        if self.pad.as_ref().is_some_and(|p| p.seen.elapsed() > Duration::from_millis(800)) {
            self.pad_end()
        }
        if self.inertia.is_some() {
            self.pad_inertia()
        }
        if self.settle.is_some_and(|t| Instant::now() >= t) {
            self.pad_settle()
        }
        if self.drag.is_some() && !self.blocked() {
            self.drag_update()
        }
        Ok(())
    }
    fn refresh(&mut self) {
        if self.blocked() {
            return;
        }
        if self.last_scan.elapsed() >= Duration::from_secs(1) {
            self.last_scan = Instant::now();
            for id in win32::windows() {
                if self.owning(id).is_none() {
                    self.add(id)
                }
            }
        }
        if self.cfg["layout"]["focus_follows_mouse"].as_bool() == Some(true) {
            unsafe {
                let mut p = POINT::default();
                GetCursorPos(&mut p);
                let id = GetAncestor(WindowFromPoint(p), GA_ROOT) as isize;
                if self.owning(id).is_some() && GetForegroundWindow() as isize != id {
                    self.select(id);
                    win32::activate(id);
                    self.dirty = true
                }
            }
        }
    }
    fn scan(&mut self) {
        let mut ids = win32::windows();
        ids.sort_by_key(|&id| {
            win32::frame(id).map(|r| {
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
        for id in ids {
            self.add(id)
        }
        self.select(unsafe { GetForegroundWindow() } as isize);
        self.reveal()
    }
    fn select(&mut self, id: isize) {
        if let Some((desk, name)) = self.owning(id) {
            if Some(desk) == self.desktop {
                self.active = name.clone();
                self.strips.get_mut(&(desk, name)).unwrap().select(id)
            }
        }
    }
    fn topology(&mut self) {
        let old = std::mem::replace(&mut self.displays, win32::monitors());
        for ((_, name), s) in &mut self.strips {
            if let Some((_, area, _)) = self.displays.iter().find(|m| &m.0 == name) {
                s.set_area(*area);
                for (id, _) in s.rects(0) {
                    if self.orphans.remove(&id) {
                        unsafe {
                            ShowWindow(id as _, SW_SHOWNOACTIVATE);
                        }
                    }
                }
            } else if old.iter().any(|m| &m.0 == name) {
                for (id, _) in s.rects(0) {
                    self.orphans.insert(id);
                    self.homes.insert(id, name.clone());
                    unsafe {
                        ShowWindow(id as _, SW_SHOWMINNOACTIVE);
                    }
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
        self.dirty = true;
        // Building the overlay windows and their GPU surfaces takes ~100 ms each; start now, in parallel
        // with the window scan, so the first placement does not wait for it.
        let _ = self.renderer.send(render::Message::Warm(
            self.displays.iter().map(|m| (m.0.clone(), m.2)).collect(),
        ));
    }
    /// The strip of a monitor, created with the layout settings `add` uses.
    fn home_strip(&mut self, key: Key, area: Rect) -> &mut Strip {
        let gap = self.cfg["layout"]["gap_px"].as_integer().unwrap() as i32;
        let split = self.cfg["layout"]["first_pair_split"].as_bool().unwrap();
        let share = self.cfg["layout"]["default_tile_share"].as_float().unwrap();
        let s = self.strips.entry(key).or_insert_with(|| Strip::new(area));
        s.gap = gap;
        s.split = split;
        s.share = share;
        s
    }
    /// Puts every window whose monitor is attached again back on that monitor, including windows that
    /// `pull-orphans` moved to another monitor. A window whose monitor is still detached stays remembered.
    fn return_home(&mut self) {
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
                let peek = if self.cfg["layout"]["peek_enabled"].as_bool() == Some(true) {
                    self.cfg["layout"]["peek_width_px"].as_integer().unwrap() as i32
                        + self.cfg["layout"]["gap_px"].as_integer().unwrap() as i32
                } else {
                    0
                };
                let minimum = win32::minimum(id, area.horizontal());
                let s = self.home_strip((owner.0, home), area);
                s.insert(id, minimum);
                // Inserted columns are focused; show it, so it is not left parked off-screen.
                s.reveal(peek);
            }
            self.orphans.remove(&id);
        }
    }
    fn layout(&mut self) -> Result<()> {
        if self.blocked() || !self.dirty {
            return Ok(());
        }
        self.dirty = false;
        self.seq += 1;
        for s in self.strips.values_mut() {
            s.fill_gaps()
        }
        let mut tiles = Vec::new();
        let foreground = unsafe { GetForegroundWindow() } as isize;
        let external = self.owning(foreground).is_none() && win32::external(foreground);
        // Unmanaged app windows (an elevated Task Manager, a rule-ignored app): the overlay must never be
        // painted over them, whatever the z-order does. They become holes in the overlay (a window region),
        // so borders and peeks stop at their edge and clicks reach them.
        // Test daemons share the desktop with the developer's own windows, which they treat as unmanaged;
        // carving those out would hide every border they draw, so isolated instances skip it unless a test
        // asks for it with PLANOR_HOLES.
        let holes = self.holes();
        self.last_holes = holes.clone();
        let color = |k: &str| {
            if self.cfg["borders"]["enabled"].as_bool() == Some(true) {
                self.cfg["borders"][k]
                    .as_str()
                    .and_then(|v| u32::from_str_radix(v.trim_start_matches('#'), 16).ok())
            } else {
                None
            }
        };
        let mut preview = self.strips.clone();
        if let Some(d) = &self.drag {
            if let (Some(k), Some(target)) = (&d.destination, d.target) {
                if k == &d.key && matches!(d.mode, planor::Drag::Before | planor::Drag::After) {
                    if let Some(s) = preview.get_mut(k) {
                        s.reorder(d.id, target, d.mode == planor::Drag::After)
                    }
                }
            }
        }
        for ((desk, name), strip) in &preview {
            if Some(*desk) != self.desktop {
                continue;
            }
            for (id, mut rect) in
                strip.rects(self.cfg["layout"]["stack_peek_px"].as_integer().unwrap() as i32)
            {
                if let Some(saved) = self.journal.entries.get(&id) {
                    let (i, _) = strip.locate(id).unwrap();
                    if self.drag.as_ref().is_some_and(|d| d.id == id) {
                        continue;
                    }
                    let fullscreen = strip.fullscreen_id() == Some(id);
                    if fullscreen {
                        if let Some((_, _, b)) = self.displays.iter().find(|m| &m.0 == name) {
                            if strip.area.horizontal() {
                                rect.y = b.y;
                                rect.h = b.h
                            } else {
                                rect.x = b.x;
                                rect.w = b.w
                            }
                        }
                    }
                    let c = if fullscreen {
                        None
                    } else if self
                        .drag
                        .as_ref()
                        .is_some_and(|d| d.target == Some(id) && d.mode == planor::Drag::Stack)
                    {
                        color("drop_target_color")
                    } else if self.drag.as_ref().is_some_and(|d| {
                        // A dragged window arriving over a group lights the whole group.
                        d.destination.as_ref() == Some(&(*desk, name.clone()))
                            && d.target.is_some_and(|t| strip.locate(t).is_some_and(|(c, _)| c == i))
                            && strip.columns[i].members.len() > 1
                            && strip.columns[i].members[strip.columns[i].active] == id
                    }) {
                        color("group_hover_color")
                    } else if strip.columns[i].members.len() > 1
                        && strip.columns[i].members[strip.columns[i].active] == id
                    {
                        color("stacked_color")
                    } else if !external && name == &self.active && strip.active() == Some(id) {
                        color("focused_color")
                    } else {
                        None
                    };
                    tiles.push(render::Tile {
                        id,
                        rect,
                        monitor: name.clone(),
                        pinned: false,
                        fullscreen,
                        preview: strip.columns[i].members[strip.columns[i].active] != id,
                        hidden: !strip.columns[i].shown(strip.columns[i].members.iter().position(|&m| m == id).unwrap_or(0)),
                        bound: if fullscreen {
                            self.displays.iter().find(|m| &m.0 == name).map(|m| m.2).unwrap_or(strip.area)
                        } else {
                            strip.area
                        },
                        color: c,
                        cookie: saved.cookie,
                    });
                    self.placed.insert(id, (self.seq, rect));
                }
            }
        }
        for &id in &self.pinned {
            if let (Some(rect), Some(saved)) = (win32::frame(id), self.journal.entries.get(&id)) {
                if let Some((name, _, _)) = self.displays.iter().find(|(_, _, b)| {
                    rect.x >= b.x && rect.x < b.x + b.w && rect.y >= b.y && rect.y < b.y + b.h
                }) {
                    tiles.push(render::Tile {
                        id,
                        rect,
                        monitor: name.clone(),
                        color: color("pinned_color"),
                        cookie: saved.cookie,
                        pinned: true,
                        fullscreen: false,
                        preview: false,
                        hidden: false,
                        bound: rect,
                    })
                }
            }
        }
        if self.diag.enabled {
            let animate_ms = if self.cfg["animation"]["enabled"].as_bool() == Some(true)
                && self.cfg["animation"][self.action].as_bool() == Some(true)
            {
                self.cfg["animation"]["duration_ms"]
                    .as_integer()
                    .unwrap_or(180) as u64
            } else {
                0
            };
            for t in &tiles {
                diag::trace(format!(
                    "EXPECT id={} rect={},{},{},{} monitor={} kind={} hidden={} animate_ms={}",
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
                    animate_ms
                ));
            }
        }
        let tiles_len = tiles.len();
        render::EPOCH.store(self.seq, std::sync::atomic::Ordering::Release);
        self.renderer.send(render::Message::Scene(render::Scene {
            epoch: self.seq,
            input: self.input.take(),
            diagnostics: self.diag.enabled,
            tiles,
            monitors: self.displays.clone(),
            duration: if !std::mem::take(&mut self.instant)
                && self.cfg["animation"]["enabled"].as_bool() == Some(true)
                && self.cfg["animation"][self.action].as_bool() == Some(true)
            {
                // Live touchpad scrolling re-targets every few milliseconds; a short ease per update
                // smooths it without the strip trailing the fingers.
                if std::mem::take(&mut self.quick) {
                    70
                } else {
                    self.cfg["animation"]["duration_ms"].as_integer().unwrap() as u64
                }
            } else {
                self.quick = false;
                0
            },
            spring: self.cfg["animation"]["curve"].as_str() == Some("spring"),
            width: self.cfg["borders"]["width_px"].as_integer().unwrap() as f32,
            radius: self.cfg["borders"]["corner_radius_px"]
                .as_integer()
                .unwrap() as f32,
            holes: holes.clone(),
        }))?;
        if self.diag.enabled {
            diag::trace(format!("SCENE seq={} tiles={} holes={}", self.seq, tiles_len, holes.len()));
        }
        Ok(())
    }
    fn clear(&self) -> Result<()> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.renderer.send(render::Message::Clear(tx))?;
        rx.recv()?;
        Ok(())
    }
    fn pin(&mut self) {
        let id = unsafe { GetForegroundWindow() } as isize;
        let _ = self.clear();
        if self.pinned.remove(&id) {
            unsafe {
                SetWindowPos(
                    id as _,
                    HWND_NOTOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            self.add(id)
        } else if let Some(k) = self.owning(id) {
            unsafe {
                SetWindowPos(
                    id as _,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
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
        // Act on the monitor that really holds focus, not the one Planor last recorded.
        let focused = unsafe { GetForegroundWindow() } as isize;
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
        } {
            if let Some(strip) = self.key().and_then(|k| self.strips.get(&k)) {
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
            win32::activate(id);
            if self.cfg["layout"]["warp_cursor_to_focused"].as_bool() == Some(true) {
                if let Some(r) = self.strips[&self.key().unwrap()]
                    .rects(0)
                    .into_iter()
                    .find(|(wid, _)| *wid == id)
                    .map(|(_, r)| r)
                {
                    unsafe {
                        SetCursorPos(r.x + r.w / 2, r.y + r.h / 2);
                    }
                }
            }
        }
    }
    fn drag_begin(&mut self, id: isize) {
        if let Some(key) = self.owning(id) {
            if let Some(original) = win32::frame(id) {
                let mut point = POINT::default();
                unsafe { GetCursorPos(&mut point) };
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
                });
                self.dirty = true
            }
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
            self.v_travel = 0.;
            self.scroll_samples.clear();
            // Resuming within the settle delay keeps the focus the scroll already chose.
            if self.settle.is_none() {
                let focused = unsafe { GetForegroundWindow() } as isize;
                if self.owning(focused).is_some() {
                    self.select(focused)
                }
            }
        }
        self.settle = None;
        self.inertia = None;
        let now = Instant::now();
        self.scroll_samples.push_back((now, d, vertical));
        while self.scroll_samples.front().is_some_and(|s| now.duration_since(s.0) > Duration::from_millis(150)) {
            self.scroll_samples.pop_front();
        }
        self.scroll_by(d, vertical, true)
    }
    /// Move the strip (or roll the focused group) by `d` finger units. Shared by the live gesture and the
    /// momentum that carries on after the fingers lift.
    fn scroll_by(&mut self, d: f64, vertical: bool, live: bool) {
        let gain = self.cfg["touchpad"]["swipe_scroll_gain"].as_float().unwrap_or(1.);
        let natural = self.cfg["touchpad"]["swipe_natural_direction"].as_bool() == Some(true);
        let signed = if natural { -d } else { d } * gain;
        let Some(s) = self.key().and_then(|k| self.strips.get_mut(&k)) else {
            return;
        };
        if vertical {
            let depth = if s.area.horizontal() { s.area.h } else { s.area.w } as f64;
            if !s.roll(signed / (depth * 0.5)) {
                // No group here: a long vertical swipe hops to the monitor above or below instead.
                self.v_travel += d;
                if self.v_travel.abs() >= 90. {
                    let down = (self.v_travel > 0.) != natural;
                    self.v_travel = 0.;
                    self.keypress(if down { "focus_down" } else { "focus_up" });
                }
                return;
            }
        } else {
            self.scroll_frac += signed;
            let whole = self.scroll_frac.trunc();
            self.scroll_frac -= whole;
            s.scroll(whole as i32);
            s.focus_nearest_center();
        }
        // Under the fingers the strip follows with no easing at all (one to one); the momentum after the
        // lift is eased a little because it only updates 30 times a second.
        if live {
            self.instant = true;
        } else {
            self.quick = true;
        }
        self.action = "scroll";
        self.dirty = true
    }
    fn pad_scroll_end(&mut self) {
        if !std::mem::take(&mut self.pad_scrolling) {
            return;
        }
        self.scroll_frac = 0.;
        // Momentum: a swipe that was still moving when the fingers lifted keeps going and slows down, the
        // way a page or a phone list does.
        let now = Instant::now();
        let (mut sum, mut vertical) = (0., false);
        for (t, d, v) in &self.scroll_samples {
            if now.duration_since(*t) <= Duration::from_millis(100) {
                sum += d;
                vertical = *v;
            }
        }
        self.scroll_samples.clear();
        let speed = sum / 0.1;
        let enabled = self.cfg["touchpad"]["swipe_inertia"].as_bool() == Some(true);
        if enabled && speed.abs() > 150. {
            self.inertia = Some((vertical, speed, now));
            self.settle = None;
        } else {
            // Settle only after the fingers have stayed off for a moment, so a swipe that continues
            // right away is not interrupted by a snap and a focus change.
            self.settle = Some(now + Duration::from_millis(220));
        }
    }
    /// One tick of momentum.
    fn pad_inertia(&mut self) {
        let Some((vertical, speed, last)) = self.inertia else { return };
        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f64().min(0.1);
        self.scroll_by(speed * dt, vertical, false);
        let speed = speed * 0.9f64.powf(dt / 0.032);
        if speed.abs() < 40. || self.blocked() {
            self.inertia = None;
            self.settle = Some(now + Duration::from_millis(120));
        } else {
            self.inertia = Some((vertical, speed, now));
        }
    }
    fn pad_settle(&mut self) {
        self.settle = None;
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
            win32::activate(id)
        }
    }
    fn pad_begin(&mut self) {
        if self.blocked() || self.drag.is_some() {
            return;
        }
        let focused = unsafe { GetForegroundWindow() } as isize;
        if self.owning(focused).is_some() {
            self.select(focused)
        }
        let Some(id) = self.key().and_then(|k| self.strips.get(&k)).and_then(|s| s.active()) else {
            return;
        };
        let Some(frame) = win32::frame(id) else { return };
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(id as _, &mut rect) } == 0 {
            return;
        }
        self.drag_begin(id);
        if self.drag.is_none() {
            return;
        }
        // Grab the window at its centre so the cursor and the window stay together.
        let (cx, cy) = (frame.x + frame.w / 2, frame.y + frame.h / 2);
        unsafe { SetCursorPos(cx, cy) };
        // Carrying a window across monitors with different DPI makes the app resize and re-render on
        // every step (the recording shows it flickering to odd sizes and freezing). By default the drag
        // stays on its own monitor and a fling does the crossing in one step.
        let cross = self.cfg["touchpad"]["swipe_drag_cross_monitors"].as_bool() == Some(true);
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
            grab: ((cx - rect.left) as f64, (cy - rect.top) as f64),
            cursor: (cx as f64, cy as f64),
            limits,
            seen: Instant::now(),
            speed: 0.,
            moved: Instant::now(),
            samples: Default::default(),
        });
    }
    fn pad_move(&mut self, dx: f64, dy: f64) {
        let base = self.cfg["touchpad"]["swipe_drag_gain"].as_float().unwrap_or(1.);
        let accel = self.cfg["touchpad"]["swipe_drag_accel"].as_float().unwrap_or(0.);
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
        unsafe {
            SetCursorPos(p.cursor.0.round() as i32, p.cursor.1.round() as i32);
            // Asynchronous and rate-limited (about 120 per second, latest position wins): a heavy window
            // (a browser) cannot keep up with every touchpad report, and a queue of stale moves is what
            // made the window trail the fingers by a growing amount.
            if now.duration_since(p.moved) >= Duration::from_millis(8) {
                p.moved = now;
                SetWindowPos(
                    p.id as _,
                    null_mut(),
                    (p.cursor.0 - p.grab.0).round() as i32,
                    (p.cursor.1 - p.grab.1).round() as i32,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
                );
            }
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
        unsafe {
            SetWindowPos(
                p.id as _,
                null_mut(),
                (p.cursor.0 - p.grab.0).round() as i32,
                (p.cursor.1 - p.grab.1).round() as i32,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
            );
        }
        let threshold = self.cfg["touchpad"]["swipe_fling_speed"].as_integer().unwrap_or(1500) as f64;
        if self.cfg["touchpad"]["swipe_fling_enabled"].as_bool() == Some(true)
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
                unsafe { SetCursorPos(b.x + b.w / 2, b.y + b.h / 2) };
                self.flung = true;
            }
        }
        self.drag_end(p.id)
    }
    fn drag_update(&mut self) {
        let Some(d) = self.drag.as_mut() else { return };
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p) };
        let elapsed = d.sampled.elapsed().as_secs_f64().max(0.001);
        let velocity = ((p.x - d.point.x) as f64).hypot((p.y - d.point.y) as f64) / elapsed;
        d.point = p;
        d.sampled = Instant::now();
        let Some((monitor, _, _)) = self
            .displays
            .iter()
            .find(|(_, _, r)| p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h)
        else {
            d.target = None;
            d.mode = planor::Drag::None;
            d.destination = None;
            self.dirty = true;
            return;
        };
        let k = (d.key.0, monitor.clone());
        d.destination = Some(k.clone());
        // Dragging to the leading or trailing edge of the strip scrolls it, unless another monitor lies that
        // way (then the drag is heading there instead).
        if k == d.key {
            let bounds: Vec<Rect> = self.displays.iter().map(|m| m.2).collect();
            let at = self.displays.iter().position(|m| m.0 == k.1);
            if let Some(s) = self.strips.get_mut(&k) {
                let horizontal = s.area.horizontal();
                let (pos, lo, hi) = if horizontal {
                    (p.x, s.area.x, s.area.x + s.area.w)
                } else {
                    (p.y, s.area.y, s.area.y + s.area.h)
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
                    s.scroll(dir * 28);
                    self.dirty = true;
                }
            }
        }
        let target = self.strips.get(&k).and_then(|s| {
            s.rects(self.cfg["layout"]["stack_peek_px"].as_integer().unwrap() as _)
                .into_iter()
                .find(|(id, r)| {
                    *id != d.id && p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h
                })
                .map(|(id, r)| {
                    let length = if s.area.horizontal() { r.w } else { r.h } as f64;
                    (
                        id,
                        if s.area.horizontal() {
                            (p.x - r.x) as f64 / r.w as f64
                        } else {
                            (p.y - r.y) as f64 / r.h as f64
                        },
                        length,
                    )
                })
        });
        let id = target.map(|t| t.0);
        if id != d.target {
            self.dirty=true;            d.hover = Instant::now();
            d.target = id
        }
        let vk = match self.cfg["layout"]["drag_stack_modifier"].as_str() {
            Some("Ctrl") => 17,
            Some("Alt") => 18,
            _ => 16,
        };
        let freeze =
            unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk) } < 0;
        let slow = self.cfg["animation"]["drag_slow_velocity_px_s"].as_integer().unwrap() as f64;
        let hover_ms = self.cfg["layout"]["stack_hover_ms"].as_integer().unwrap() as u64;
        let zone = self.cfg["layout"]["stack_center_px"].as_integer().unwrap_or(20) as f64;
        let crossing = k != d.key;
        let mode = target
            .map(|(_, fraction, length)| {
                // Stacking is offered only on the same monitor and only near the middle of the
                // window; elsewhere the drop can only reorder (before / after).
                let may_stack = !crossing && (fraction - 0.5).abs() * length <= zone;
                let dwell = if may_stack { d.hover.elapsed().as_millis() as u64 } else { 0 };
                let hover = if may_stack { hover_ms } else { u64::MAX };
                planor::drag(fraction, velocity, slow, dwell, hover, freeze)
            })
            .unwrap_or(planor::Drag::None);
        if mode != d.mode {
            d.mode = mode;
            self.action = "drag_reflow";
            self.dirty = true
        }
    }
    fn drag_end(&mut self, id: isize) {
        let flung = std::mem::take(&mut self.flung);
        self.drag_update();let Some(mut d) = self.drag.take() else { return };
        self.dirty = true;
        if flung {
            // A fling only changes monitor; it never reorders or stacks onto what is under the cursor.
            d.target = None;
            d.mode = planor::Drag::None;
        }
        if id != d.id
            || self.blocked()
            || self.owning(id) != Some(d.key.clone())
            || desktop() != Some(d.key.0)
        {
            return;
        }
        // The moves during a pad drag are asynchronous, so a flung window may not have moved yet.
        let Some(r) = (if flung { Some(d.original) } else { win32::frame(id) }) else { return };
        let s = self.strips.get_mut(&d.key).unwrap();
        if r==d.original&&!flung{return}
        // A resize keeps one edge where it was (a move changes both). Dragging an edge far beyond the monitor
        // puts the cursor on another monitor; that is still a resize, not a transfer.
        let moved = |a: i32, b: i32| (a - b).abs() > 2;
        // and it still sits mostly on the monitor it started on (a move to another monitor does not).
        let home = self.displays.iter().find(|m| m.0 == d.key.1).map(|m| m.2);
        let on_home = home.is_some_and(|b| {
            let w = (r.x + r.w).min(b.x + b.w) - r.x.max(b.x);
            let h = (r.y + r.h).min(b.y + b.h) - r.y.max(b.y);
            w > 0 && h > 0 && (w as i64 * h as i64) * 2 >= r.w as i64 * r.h as i64
        });
        let resized = on_home && !flung
            && ((moved(r.w, d.original.w) && (r.x == d.original.x || r.x + r.w == d.original.x + d.original.w))
                || (moved(r.h, d.original.h) && (r.y == d.original.y || r.y + r.h == d.original.y + d.original.h)));
        if resized || (d.destination.as_ref().is_none_or(|k|k==&d.key)&&(d.original.w!=r.w||d.original.h!=r.h)){
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
        if r == d.original && !flung {
            // Escape cancels the OS move loop and snaps the window back; a drop that never moved the
            // window must not reorder, stack or transfer anything.
            return;
        }
        let Some(dest) = d.destination else { return };
        if self.displays!=win32::monitors(){self.deadline=Some(Instant::now()+Duration::from_millis(1500));return}
        let Some((_, area, _)) = self.displays.iter().find(|m| m.0 == dest.1).cloned() else {
            return;
        };
        if let Some(target) = d.target {
            if unsafe { IsWindow(target as _) } == 0 || self.owning(target) != Some(dest.clone()) {
                return;
            }
        }
        if dest != d.key {
            if win32::minimum(id,true)>area.w||win32::minimum(id,false)>area.h||!self.transfer_dialogs(id,d.original,area) {
                return;
            }
            self.strips.get_mut(&d.key).unwrap().remove(id);
            self.strips.get_mut(&d.key).unwrap().fill();
            let s = self
                .strips
                .entry(dest.clone())
                .or_insert_with(|| Strip::new(area));
            s.insert(id, win32::minimum(id, area.horizontal()))
        }
        if dest != d.key {
            // Crossing monitors: snap into the new slot at once. Animating the whole way across
            // monitors (and DPI boundaries) is what made flung windows wander and stall.
            self.instant = true;
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
        self.reveal()
    }
    fn transfer_dialogs(&mut self,id:isize,owner:Rect,area:Rect)->bool{
        let mut moves = vec![];
        for dlg in win32::windows() {
            if unsafe { GetWindow(dlg as _, GW_OWNER) } as isize == id {
                let Some(r) = win32::frame(dlg) else {
                    return false;
                };
                if r.w > area.w || r.h > area.h || self.journal.capture(dlg).is_err() {
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
        moves.is_empty() || win32::place(&moves).is_ok()
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
        if unsafe { IsZoomed(id as _) } != 0 {
            unsafe { ShowWindow(id as _, SW_RESTORE) };
            s.select(id);
            s.maximize();
            if !self.zoomed.remove(&id) {
                self.zoomed.insert(id);
            }
            self.reveal();
            return;
        }
        let Some(r) = win32::frame(id) else { return };
        let full =
            r == *bounds && unsafe { GetWindowLongW(id as _, GWL_STYLE) } as u32 & WS_CAPTION == 0;
        let existing = s.fullscreen_id() == Some(id);
        if full != existing
            && (full || unsafe { GetWindowLongW(id as _, GWL_STYLE) } as u32 & WS_CAPTION != 0)
        {
            s.select(id);
            s.fullscreen(full, *bounds);
            // The app resizes itself to full screen at the same moment; animating the strip as well
            // made the two fight and the transition lag.
            self.instant = true;
            self.reveal()
        }
    }
    fn event(&mut self, e: Event) {
        match e {
            Event::Peek(id) if !self.blocked() => {
                self.select(id);
                self.reveal();
                win32::activate(id)
            }
            Event::Rejected(seq, entries) if seq == self.seq && !self.blocked() => {
                for (id, r) in entries {
                    if let Some(k) = self.owning(id) {
                        if self.placed.get(&id).is_some_and(|(n, _)| *n == seq) {
                            let s = self.strips.get_mut(&k).unwrap();
                            let i = s.locate(id).unwrap().0;
                            let length = if s.area.horizontal() { r.w } else { r.h };
                            if length > s.columns[i].length {
                                s.resize(i, length, false);
                                self.dirty = true
                            }
                        }
                    }
                }
            }
            Event::Frame(f) => {
                if self.diag.enabled {
                    self.diag.record(f);
                    if let Ok(dir) = folder("LOCALAPPDATA") {
                        let _ = diag::append(
                            &dir,
                            &serde_json::to_vec(&f).unwrap_or_default(),
                            self.cfg["diagnostics"]["max_log_mb"].as_integer().unwrap() as u64
                                * 1024
                                * 1024,
                            self.cfg["diagnostics"]["max_log_files"]
                                .as_integer()
                                .unwrap() as usize,
                        );
                    }
                }
            }
            Event::Suspend(v) => {
                let _ = self.clear();
                self.locked = v;
                if !v {
                    self.deadline = Some(Instant::now() + Duration::from_millis(1500))
                }
            }
            Event::Topology => {
                self.drag = None;
                let _ = self.clear();
                self.deadline = Some(Instant::now() + Duration::from_millis(1500))
            }
            Event::Key(k) => {
                self.input = Some(Instant::now());
                self.keypress(&k)
            }
            Event::Window(EVENT_OBJECT_DESTROY,id) if self.blocked()=>{if let Some(k)=self.owning(id){self.strips.get_mut(&k).unwrap().remove(id);}self.pinned.remove(&id);self.orphans.remove(&id);self.skipped.remove(&id);self.dirty=true;},
            Event::Window(e, id) if !self.blocked() => match e {
                EVENT_OBJECT_SHOW | EVENT_SYSTEM_MINIMIZEEND => {
                    if self.cfg["layout"]["spawn_on_cursor_monitor"].as_bool() == Some(true)
                        && e == EVENT_OBJECT_SHOW
                    {
                        let mut p = POINT::default();
                        unsafe { GetCursorPos(&mut p) };
                        self.spawn_hint = self
                            .displays
                            .iter()
                            .find(|(_, _, r)| p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h)
                            .map(|m| m.0.clone());
                    }
                    self.add(id);
                    self.spawn_hint = None
                }
                // While a touchpad scroll or drag is in progress it owns the focus; the normal
                // "reveal the focused window" reaction would fight it (and snap the view back).
                EVENT_SYSTEM_FOREGROUND if self.pad_scrolling || self.settle.is_some() || self.pad.is_some() => (),
                EVENT_SYSTEM_FOREGROUND => {
                    if self.diag.enabled {
                        diag::trace(format!("FG {id} managed={} external={} pad={}", self.owning(id).is_some(), win32::external(id), self.pad_scrolling));
                    }
                    let changed = self
                        .key()
                        .and_then(|k| self.strips.get(&k))
                        .and_then(|s| s.active())
                        != Some(id);
                    // Focus moving to or from an unmanaged window changes the border and the overlay order.
                    self.dirty = true;
                    if changed && self.owning(id).is_some() {
                        // A window focused by any route (taskbar, Alt+Tab, click, app request) must
                        // be visible: otherwise focus lands on a parked, off-screen window.
                        self.select(id);
                        let visible = self
                            .owning(id)
                            .and_then(|k| self.strips.get(&k))
                            .is_some_and(|s| s.in_view(id));
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
                    if unsafe { IsWindowVisible(id as _) } != 0
                        || unsafe { IsIconic(id as _) } != 0
                        || self.owning(id).is_none() => {}
                EVENT_OBJECT_DESTROY | EVENT_SYSTEM_MINIMIZESTART | EVENT_OBJECT_HIDE => {
                    if !self.orphans.contains(&id) {
                        if let Some(k) = self.owning(id) {
                            let focused =
                                self.strips[&k].active() == Some(id) && k.1 == self.active;
                            self.strips.get_mut(&k).unwrap().remove(id);
                            self.strips.get_mut(&k).unwrap().fill();
                            if focused {
                                if let Some(next) = self.strips[&k].active() {
                                    win32::activate(next)
                                }
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
            Event::Contacts(p)
                if !self.blocked()
                    && self.cfg["touchpad"]["three_finger_enabled"].as_bool() == Some(true) =>
            {
                self.input = Some(Instant::now());
                let swipe = self.gesture.sample(
                    &p,
                    self.cfg["touchpad"]["axis_lock_deadzone"]
                        .as_integer()
                        .unwrap() as f64,
                    self.cfg["touchpad"]["swipe_step_px"].as_integer().unwrap() as f64,
                );
                // Three fingers move the focus, four fingers move the focused window. With the
                // natural direction the content follows the fingers (swipe right = focus left).
                // Gesture trace (diagnostics): what was recognised and when, for lining up with a note.
                if self.diag.enabled && !matches!(swipe, Swipe::None | Swipe::DragMove(..) | Swipe::Scroll(_) | Swipe::ScrollV(_)) {
                    diag::trace(format!("PAD {swipe:?}"));
                }
                match swipe {
                    Swipe::DragStart => {
                        diag::trace(format!("PAD drag start, window {:?}", self.key().and_then(|k| self.strips.get(&k)).and_then(|s| s.active())));
                        return self.pad_begin();
                    }
                    Swipe::DragMove(dx, dy) => return self.pad_move(dx, dy),
                    Swipe::DragEnd => return self.pad_end(),
                    Swipe::Scroll(d) => return self.pad_scroll(d, false),
                    Swipe::ScrollV(d) => return self.pad_scroll(d, true),
                    Swipe::ScrollEnd => return self.pad_scroll_end(),
                    _ => (),
                }
                if let Swipe::Step { fingers, horizontal, direction } = swipe {
                    let natural = self.cfg["touchpad"]["swipe_natural_direction"].as_bool() == Some(true);
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
                    self.keypress(key)
                }
            }
            _ => (),
        }
    }
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
    let mut server = PipeServer::new(&pipe_name())?;
    let mut journal = Journal::open(folder("LOCALAPPDATA")?.join("recovery.json"))?;
    journal.restore()?;
    win32::tweak_system(&folder("LOCALAPPDATA")?.join("system-settings.json"));
    let cfg = config()?;
    let diag = diag::Telemetry::new(cfg["diagnostics"]["enabled"].as_bool().unwrap());
    if diag.enabled {
        record_input();
    }
    let (tx, events) = crossbeam_channel::unbounded();
    let (reload, rx) = crossbeam_channel::unbounded();
    win32::events(tx, cfg.clone(), rx);
    let mut engine = Engine {
        pad: None,
        pad_scrolling: false,
        settle: None,
        scroll_frac: 0.,
        renderer: render::start(),
        action: "open_close",
        cfg,
        journal,
        diag,
        strips: Default::default(),
        displays: vec![],
        desktop: desktop(),
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
        flung: false,
        spawn_hint: None,
        quick: false,
        last_holes: vec![],
        holes_checked: Instant::now(),
        scroll_samples: Default::default(),
        inertia: None,
        v_travel: 0.,
        gesture: Default::default(),
        seq: 0,
        input: None,
        zoomed: Default::default(),
        drag: None,
        last_scan: Instant::now(),
        placed: Default::default(),
    };
    engine.topology();
    if engine.cfg["layout"]["tile_existing_on_startup"].as_bool() == Some(true) {
        engine.scan()
    } else {
        engine.skipped.extend(win32::windows())
    }
    let tick = crossbeam_channel::tick(Duration::from_millis(32));
    let result = (|| -> Result<()> {
        loop {
            let t = Instant::now();
            engine.layout()?;
            slow("layout", t);
            if server.poll(|cmd| engine.command(cmd, &reload))? {
                break;
            }
            crossbeam_channel::select! {recv(events)->e=>match e?{Event::Stop=>break,Event::Key(k) if k=="panic_exit"=>break,e if !server.closing=>{let t=Instant::now();let name=e.name();if engine.diag.enabled&&!matches!(name,"frame"|"contacts"){let detail=match &e{Event::Window(k,id)=>format!(" {k:#x} {id}"),Event::Rejected(s,v)=>format!(" seq={s} n={} first={:?}",v.len(),v.first()),Event::Peek(id)=>format!(" {id}"),_=>String::new()};diag::trace(format!("EV {name}{detail}"))}engine.event(e);slow(name,t)},_=>()},recv(tick)->_=>{if !server.closing{let t=Instant::now();engine.tick()?;slow("tick",t)}}}
        }
        Ok(())
    })();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let _ = engine.renderer.send(render::Message::Stop(tx));
    let _ = rx.recv();
    let restored = engine.journal.restore();
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
    journal.restore()?;
    Ok(count)
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
        Journal::open(path.clone()).unwrap().restore().unwrap();
        assert!(!path.exists())
    }
}
/// Diagnostics: records the cursor position, mouse buttons and non-text keys with the same timestamps as
/// the rest of the trace, so a black screen or flicker can be lined up with exactly what was done.
/// Polled (no extra hook, so it cannot slow input). Keys that type text are only counted, never named:
/// the log must not become a record of what was typed.
fn record_input() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
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
            for vk in 1..255usize {
                let now = unsafe { GetAsyncKeyState(vk as i32) } < 0;
                if now == down[vk] {
                    continue;
                }
                down[vk] = now;
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

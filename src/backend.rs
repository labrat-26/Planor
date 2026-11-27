use crate::daemon::Saved;
use crate::win32;
use crate::{Result, config, folder, instance, wide};
use planor::Rect;
use std::io;
use windows_sys::Win32::{
    Foundation::*,
    UI::Input::KeyboardAndMouse::GetAsyncKeyState,
    UI::WindowsAndMessaging::*,
};

#[derive(Clone, Debug)]
pub(crate) struct WindowPlacementInfo {
    pub pid: u32,
    pub flags: u32,
    pub show: u32,
    pub min: [i32; 2],
    pub max: [i32; 2],
    pub normal: [i32; 4],
    pub rect: [i32; 4],
    pub topmost: bool,
}

/// Abstraction over Win32/DWM/VirtualDesktop OS queries and mutations so `Engine` and `Journal`
/// can run identically against either the real desktop (`Win32Backend`) or a deterministic
/// simulated desktop (`FakeBackend` in tests).
pub(crate) trait WindowBackend {
    fn get_recovery_cookie(&self, id: isize) -> usize;
    fn set_recovery_cookie(&self, id: isize, cookie: usize) -> Result<()>;
    fn remove_recovery_cookie(&self, id: isize);
    fn get_window_pid(&self, id: isize) -> Option<u32>;
    fn get_window_placement(&self, id: isize) -> Option<WindowPlacementInfo>;
    fn restore_window_placement(&self, id: isize, saved: &Saved) -> bool;

    fn monitors(&self) -> Vec<(String, Rect, Rect)>;
    fn current_desktop(&self) -> Option<u128>;
    fn on_current_desktop(&self, id: isize) -> bool;

    fn windows(&self) -> Vec<isize>;
    fn eligible(&self, id: isize, cfg: &toml::Value) -> bool;
    fn external(&self, id: isize) -> bool;
    fn overlay_occluder(&self, id: isize) -> bool;
    fn class_of(&self, id: isize) -> String;
    fn frame(&self, id: isize) -> Option<Rect>;
    fn window_rect(&self, id: isize) -> Option<Rect>;
    fn minimum(&self, id: isize, horizontal: bool) -> i32;
    fn minimum_size(&self, id: isize) -> (i32, i32) {
        (self.minimum(id, true), self.minimum(id, false))
    }
    fn owner(&self, id: isize) -> isize;
    fn root_owner(&self, id: isize) -> isize;
    fn foreground(&self) -> isize;
    fn is_window(&self, id: isize) -> bool;
    fn is_visible(&self, id: isize) -> bool;
    fn is_iconic(&self, id: isize) -> bool;
    fn is_cloaked(&self, id: isize) -> bool;
    fn is_zoomed(&self, id: isize) -> bool;
    fn in_movesize(&self, id: isize) -> Option<bool>;
    fn has_caption(&self, id: isize) -> bool;
    fn window_at_point(&self, x: i32, y: i32) -> isize;

    fn activate(&self, id: isize);
    fn place(&self, items: &[(isize, Rect)]) -> Result<()>;
    fn place_async(&self, items: &[(isize, Rect)]) -> Result<()> {
        self.place(items)
    }
    fn show_no_activate(&self, id: isize);
    fn minimize_no_activate(&self, id: isize);
    fn restore_window(&self, id: isize);
    fn set_topmost(&self, id: isize, topmost: bool);
    fn move_window_async(&self, id: isize, x: i32, y: i32);

    fn cursor_pos(&self) -> (i32, i32);
    /// #67: the last mouse-button release seen by the low-level hook, as `(sequence, point)`; the sequence grows
    /// on every release and 0 means none was seen (hooks not installed).
    fn last_release(&self) -> (u64, (i32, i32)) {
        (0, (0, 0))
    }
    fn set_cursor_pos(&self, x: i32, y: i32);
    fn key_down(&self, vk: i32) -> bool;
    fn primary_button_down(&self) -> bool {
        self.key_down(0x01)
    }

    fn load_config(&self) -> Result<toml::Value>;
    /// `None` => skip overlay holes; `Some(None)` => all unmanaged windows;
    /// `Some(Some(cls))` => only unmanaged windows of class `cls`.
    fn holes_class_filter(&self) -> Option<Option<String>>;
    fn frames_log_dir(&self) -> Option<std::path::PathBuf>;
    fn max_refresh_hz(&self) -> f64 {
        60.0
    }
    fn contact_dpi(&self) -> Option<f64> {
        Some(96.0)
    }
}

const RECOVERY_PROP: &str = "Planor.Recovery.Identity";

pub(crate) struct Win32Backend;

impl WindowBackend for Win32Backend {
    fn get_recovery_cookie(&self, id: isize) -> usize {
        let tag = wide(RECOVERY_PROP);
        unsafe { GetPropW(id as HWND, tag.as_ptr()) as usize }
    }

    fn set_recovery_cookie(&self, id: isize, cookie: usize) -> Result<()> {
        let tag = wide(RECOVERY_PROP);
        if unsafe { SetPropW(id as HWND, tag.as_ptr(), cookie as HANDLE) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn remove_recovery_cookie(&self, id: isize) {
        let tag = wide(RECOVERY_PROP);
        unsafe { RemovePropW(id as HWND, tag.as_ptr()) };
    }

    fn get_window_pid(&self, id: isize) -> Option<u32> {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(id as HWND, &mut pid) };
        Some(pid)
    }

    fn get_window_placement(&self, id: isize) -> Option<WindowPlacementInfo> {
        unsafe {
            let hwnd = id as HWND;
            let mut p = WINDOWPLACEMENT {
                length: size_of::<WINDOWPLACEMENT>() as _,
                ..Default::default()
            };
            let mut r = RECT::default();
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if GetWindowPlacement(hwnd, &mut p) == 0 || GetWindowRect(hwnd, &mut r) == 0 {
                return None;
            }
            let n = p.rcNormalPosition;
            Some(WindowPlacementInfo {
                pid,
                flags: p.flags,
                show: p.showCmd,
                min: [p.ptMinPosition.x, p.ptMinPosition.y],
                max: [p.ptMaxPosition.x, p.ptMaxPosition.y],
                normal: [n.left, n.top, n.right, n.bottom],
                rect: [r.left, r.top, r.right, r.bottom],
                topmost: GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0,
            })
        }
    }

    fn restore_window_placement(&self, id: isize, s: &Saved) -> bool {
        unsafe {
            let h = id as HWND;
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
                return false;
            }
            let [x, y, r, b] = s.rect;
            let flags = SWP_NOACTIVATE
                | if s.show == SW_SHOWNORMAL as u32 {
                    0
                } else {
                    SWP_NOMOVE | SWP_NOSIZE
                };
            SetWindowPos(
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
            ) != 0
        }
    }

    fn monitors(&self) -> Vec<(String, Rect, Rect)> {
        win32::monitors()
    }

    fn current_desktop(&self) -> Option<u128> {
        winvd::get_current_desktop()
            .ok()?
            .get_id()
            .ok()
            .map(|g| g.to_u128())
    }

    fn on_current_desktop(&self, id: isize) -> bool {
        winvd::is_window_on_current_desktop(windows58::Win32::Foundation::HWND(id as _)).ok()
            == Some(true)
    }

    fn windows(&self) -> Vec<isize> {
        win32::windows()
    }

    fn eligible(&self, id: isize, cfg: &toml::Value) -> bool {
        win32::eligible(id, cfg)
    }

    fn external(&self, id: isize) -> bool {
        win32::external(id)
    }

    fn overlay_occluder(&self, id: isize) -> bool {
        win32::overlay_occluder(id)
    }

    fn class_of(&self, id: isize) -> String {
        win32::class_of(id)
    }

    fn frame(&self, id: isize) -> Option<Rect> {
        win32::frame(id)
    }

    fn window_rect(&self, id: isize) -> Option<Rect> {
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(id as _, &mut rect) } != 0 {
            Some(win32::rect(rect))
        } else {
            None
        }
    }

    fn minimum(&self, id: isize, horizontal: bool) -> i32 {
        win32::minimum(id, horizontal)
    }

    fn minimum_size(&self, id: isize) -> (i32, i32) {
        win32::minimum_size(id)
    }

    fn owner(&self, id: isize) -> isize {
        unsafe { GetWindow(id as _, GW_OWNER) as isize }
    }

    fn root_owner(&self, id: isize) -> isize {
        let root = unsafe { GetAncestor(id as _, GA_ROOTOWNER) as isize };
        if root != 0 { root } else { id }
    }

    fn foreground(&self) -> isize {
        unsafe { GetForegroundWindow() as isize }
    }

    fn is_window(&self, id: isize) -> bool {
        unsafe { IsWindow(id as _) != 0 }
    }

    fn is_visible(&self, id: isize) -> bool {
        unsafe { IsWindowVisible(id as _) != 0 }
    }

    fn is_iconic(&self, id: isize) -> bool {
        unsafe { IsIconic(id as _) != 0 }
    }

    fn is_cloaked(&self, id: isize) -> bool {
        win32::is_cloaked(id)
    }

    fn is_zoomed(&self, id: isize) -> bool {
        unsafe { IsZoomed(id as _) != 0 }
    }

    fn in_movesize(&self, id: isize) -> Option<bool> {
        win32::in_movesize(id)
    }

    fn has_caption(&self, id: isize) -> bool {
        unsafe { GetWindowLongW(id as _, GWL_STYLE) as u32 & WS_CAPTION != 0 }
    }

    fn window_at_point(&self, x: i32, y: i32) -> isize {
        unsafe { GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT) as isize }
    }

    fn activate(&self, id: isize) {
        win32::activate(id)
    }

    fn place(&self, items: &[(isize, Rect)]) -> Result<()> {
        win32::place(items)
    }

    fn place_async(&self, items: &[(isize, Rect)]) -> Result<()> {
        win32::place_async_checked(items)
    }

    fn show_no_activate(&self, id: isize) {
        unsafe { ShowWindow(id as _, SW_SHOWNOACTIVATE) };
    }

    fn minimize_no_activate(&self, id: isize) {
        unsafe { ShowWindow(id as _, SW_SHOWMINNOACTIVE) };
    }

    fn restore_window(&self, id: isize) {
        unsafe { ShowWindow(id as _, SW_RESTORE) };
    }

    fn set_topmost(&self, id: isize, topmost: bool) {
        unsafe {
            SetWindowPos(
                id as _,
                if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST },
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    fn move_window_async(&self, id: isize, x: i32, y: i32) {
        unsafe {
            SetWindowPos(
                id as _,
                std::ptr::null_mut(),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
            );
        }
    }

    fn last_release(&self) -> (u64, (i32, i32)) {
        win32::last_release()
    }

    fn cursor_pos(&self) -> (i32, i32) {
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p) };
        (p.x, p.y)
    }

    fn set_cursor_pos(&self, x: i32, y: i32) {
        unsafe { SetCursorPos(x, y) };
    }

    fn key_down(&self, vk: i32) -> bool {
        unsafe { GetAsyncKeyState(vk) < 0 }
    }

    fn primary_button_down(&self) -> bool {
        let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
        self.key_down(if swapped { 0x02 } else { 0x01 })
    }

    fn load_config(&self) -> Result<toml::Value> {
        config()
    }

    fn holes_class_filter(&self) -> Option<Option<String>> {
        if instance().is_empty() {
            Some(None)
        } else {
            std::env::var("PLANOR_HOLES").ok().map(Some)
        }
    }

    fn frames_log_dir(&self) -> Option<std::path::PathBuf> {
        folder("LOCALAPPDATA").ok()
    }

    fn max_refresh_hz(&self) -> f64 {
        win32::max_monitor_refresh_hz()
    }

    fn contact_dpi(&self) -> Option<f64> {
        crate::input::last_contacts_dpi()
    }
}

#[cfg(test)]
pub(crate) mod sim {
    use super::*;
    use crate::render;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug)]
    pub(crate) struct SimWindow {
        pub id: isize,
        pub pid: u32,
        pub process: String,
        pub class: String,
        pub desktop: u128,
        pub rect: Rect,
        pub normal_rect: Rect,
        pub min_w: i32,
        pub min_h: i32,
        /// When set (> min_w/min_h), simulates an application refusing smaller sizes on `place`.
        pub min_enforced_w: i32,
        pub min_enforced_h: i32,
        pub visible: bool,
        pub iconic: bool,
        pub zoomed: bool,
        pub cloaked: bool,
        pub topmost: bool,
        pub toolwindow: bool,
        pub noactivate: bool,
        pub thickframe: bool,
        pub caption: bool,
        pub owner: isize,
        pub cookie: usize,
        pub elevated: bool,
        pub in_movesize: Option<bool>,
    }

    impl SimWindow {
        pub fn new(id: isize, rect: Rect) -> Self {
            Self {
                id,
                pid: 1000 + (id.unsigned_abs() as u32),
                process: "notepad.exe".into(),
                class: "Notepad".into(),
                desktop: 1,
                rect,
                normal_rect: rect,
                min_w: 160,
                min_h: 120,
                min_enforced_w: 0,
                min_enforced_h: 0,
                visible: true,
                iconic: false,
                zoomed: false,
                cloaked: false,
                topmost: false,
                toolwindow: false,
                noactivate: false,
                thickframe: true,
                caption: true,
                owner: 0,
                cookie: 0,
                elevated: false,
                in_movesize: None,
            }
        }
    }

    #[derive(Clone, Debug)]
    pub(crate) struct SimSceneSummary {
        pub epoch: u64,
        pub duration: u64,
        pub tiles_len: usize,
        pub tiles: Vec<(isize, Rect)>,
        pub action: &'static str,
        pub holes: Vec<Rect>,
        pub zones: Vec<render::Zone>,
        pub zone_icons: Vec<render::ZoneIcon>,
        pub covered: Vec<isize>,
        pub tile_colors: Vec<(isize, Option<u32>)>,
        /// `(id, os_frame)` for fullscreen tiles: the backend-sampled frame the renderer preserves (#80).
        pub os_frames: Vec<(isize, Option<Rect>)>,
    }

    pub(crate) struct SimDesktop {
        pub monitors: Vec<(String, Rect, Rect)>,
        pub max_refresh_hz: f64,
        pub contact_dpi: Option<f64>,
        pub current_desktop: Option<u128>,
        pub windows: BTreeMap<isize, SimWindow>,
        pub z_order: Vec<isize>,
        pub foreground: isize,
        pub cursor: (i32, i32),
        /// Simulated low-level-hook button release `(sequence, point)` (see `WindowBackend::last_release`).
        pub release: (u64, (i32, i32)),
        pub keys_down: HashSet<i32>,
        pub config: toml::Value,
        pub holes_filter: Option<Option<String>>,
        pub scenes: Vec<SimSceneSummary>,
        pub overlay_updates: usize,
        pub peeks: Vec<(Rect, isize)>,
        pub next_id: isize,
    }

    impl SimDesktop {
        pub fn new(monitors: Vec<(String, Rect, Rect)>, config: toml::Value) -> Self {
            Self {
                monitors,
                max_refresh_hz: 60.0,
                contact_dpi: Some(96.0),
                current_desktop: Some(1),
                windows: BTreeMap::new(),
                z_order: Vec::new(),
                foreground: 0,
                cursor: (100, 100),
                release: (0, (0, 0)),
                keys_down: HashSet::new(),
                config,
                holes_filter: Some(None),
                scenes: Vec::new(),
                overlay_updates: 0,
                peeks: Vec::new(),
                next_id: 100,
            }
        }

        pub fn add_window(&mut self, mut w: SimWindow) -> isize {
            if w.id == 0 {
                w.id = self.next_id;
                self.next_id += 1;
            }
            let id = w.id;
            self.windows.insert(id, w);
            self.z_order.retain(|&x| x != id);
            self.z_order.insert(0, id);
            if self.foreground == 0 {
                self.foreground = id;
            }
            id
        }

        pub fn apply_scene(
            &mut self,
            scene: &render::Scene,
        ) -> Vec<(isize, Rect)> {
            let park = scene
                .monitors
                .iter()
                .map(|m| m.2.x + m.2.w)
                .max()
                .unwrap_or(0)
                + 1024;
            let mut placements = Vec::new();
            let mut peeks = Vec::new();
            let mut mon_bounds = HashMap::new();
            for (n, _, b) in scene.monitors.iter() {
                mon_bounds.insert(n.as_str(), *b);
            }
            for tile in &scene.tiles {
                let Some(w) = self.windows.get(&tile.id) else {
                    continue;
                };
                if w.cookie != tile.cookie || w.iconic || tile.pinned {
                    continue;
                }
                let Some(&mb) = mon_bounds.get(tile.monitor.as_str()) else {
                    continue;
                };
                let bound = intersect(tile.bound, mb).unwrap_or(mb);
                let clip = if tile.hidden {
                    None
                } else {
                    intersect(tile.rect, bound)
                };
                let proxy = clip != Some(tile.rect) || tile.preview || tile.hidden;
                if let Some(c) = clip
                    && proxy
                    && !tile.fullscreen
                {
                    peeks.push((c, tile.id));
                }
                let target = if proxy {
                    Rect { x: park, ..tile.rect }
                } else {
                    tile.rect
                };
                placements.push((tile.id, target));
            }
            self.peeks = peeks;
            let mut rejected = Vec::new();
            for (id, want) in placements {
                if let Some(w) = self.windows.get_mut(&id) {
                    let mut actual = want;
                    let mut constrained = false;
                    if want.x != park && w.min_enforced_w > actual.w {
                        actual.w = w.min_enforced_w;
                        constrained = true;
                    }
                    if want.x != park && w.min_enforced_h > actual.h {
                        actual.h = w.min_enforced_h;
                        constrained = true;
                    }
                    if constrained {
                        rejected.push((id, actual));
                    }
                    w.rect = actual;
                }
            }
            self.scenes.push(SimSceneSummary {
                epoch: scene.epoch,
                duration: scene.duration,
                tiles_len: scene.tiles.len(),
                tiles: scene.tiles.iter().map(|t| (t.id, t.rect)).collect(),
                action: scene.action,
                holes: scene.holes.to_vec(),
                zones: scene.zones.clone(),
                zone_icons: {
                    let dark = crate::theme::system_is_dark();
                    scene.zones.iter().map(|z| render::zone_icon_spec(z, dark)).collect()
                },
                covered: scene
                    .tiles
                    .iter()
                    .filter(|t| t.cover.is_some())
                    .map(|t| t.id)
                    .collect(),
                tile_colors: scene.tiles.iter().map(|t| (t.id, t.color)).collect(),
                os_frames: scene.tiles.iter().filter(|t| t.fullscreen).map(|t| (t.id, t.os_frame)).collect(),
            });
            rejected
        }
    }

    fn intersect(a: Rect, b: Rect) -> Option<Rect> {
        let x = a.x.max(b.x);
        let y = a.y.max(b.y);
        let w = (a.x + a.w).min(b.x + b.w) - x;
        let h = (a.y + a.h).min(b.y + b.h) - y;
        (w > 0 && h > 0).then_some(Rect { x, y, w, h })
    }

    #[derive(Clone)]
    pub(crate) struct FakeBackend(pub Arc<Mutex<SimDesktop>>);

    impl FakeBackend {
        pub fn new(desktop: SimDesktop) -> Self {
            Self(Arc::new(Mutex::new(desktop)))
        }
    }

    impl WindowBackend for FakeBackend {
        fn get_recovery_cookie(&self, id: isize) -> usize {
            self.0
                .lock()
                .unwrap()
                .windows
                .get(&id)
                .map(|w| w.cookie)
                .unwrap_or(0)
        }

        fn set_recovery_cookie(&self, id: isize, cookie: usize) -> Result<()> {
            let mut g = self.0.lock().unwrap();
            let Some(w) = g.windows.get_mut(&id) else {
                return Err("missing simulated window".into());
            };
            w.cookie = cookie;
            Ok(())
        }

        fn remove_recovery_cookie(&self, id: isize) {
            if let Some(w) = self.0.lock().unwrap().windows.get_mut(&id) {
                w.cookie = 0;
            }
        }

        fn get_window_pid(&self, id: isize) -> Option<u32> {
            self.0.lock().unwrap().windows.get(&id).map(|w| w.pid)
        }

        fn get_window_placement(&self, id: isize) -> Option<WindowPlacementInfo> {
            let g = self.0.lock().unwrap();
            let w = g.windows.get(&id)?;
            let show = if w.iconic {
                SW_SHOWMINIMIZED as u32
            } else if w.zoomed {
                SW_SHOWMAXIMIZED as u32
            } else {
                SW_SHOWNORMAL as u32
            };
            Some(WindowPlacementInfo {
                pid: w.pid,
                flags: 0,
                show,
                min: [0, 0],
                max: [0, 0],
                normal: [
                    w.normal_rect.x,
                    w.normal_rect.y,
                    w.normal_rect.x + w.normal_rect.w,
                    w.normal_rect.y + w.normal_rect.h,
                ],
                rect: [w.rect.x, w.rect.y, w.rect.x + w.rect.w, w.rect.y + w.rect.h],
                topmost: w.topmost,
            })
        }

        fn restore_window_placement(&self, id: isize, s: &Saved) -> bool {
            let mut g = self.0.lock().unwrap();
            let Some(w) = g.windows.get_mut(&id) else {
                return false;
            };
            let [nl, nt, nr, nb] = s.normal;
            w.normal_rect = Rect {
                x: nl,
                y: nt,
                w: nr - nl,
                h: nb - nt,
            };
            let [l, t, r, b] = s.rect;
            w.rect = Rect {
                x: l,
                y: t,
                w: r - l,
                h: b - t,
            };
            w.iconic = s.show == SW_SHOWMINIMIZED as u32
                || s.show == SW_SHOWMINNOACTIVE as u32
                || s.show == SW_MINIMIZE as u32;
            w.zoomed = s.show == SW_SHOWMAXIMIZED as u32;
            w.topmost = s.topmost;
            true
        }

        fn monitors(&self) -> Vec<(String, Rect, Rect)> {
            self.0.lock().unwrap().monitors.clone()
        }

        fn current_desktop(&self) -> Option<u128> {
            self.0.lock().unwrap().current_desktop
        }

        fn on_current_desktop(&self, id: isize) -> bool {
            let g = self.0.lock().unwrap();
            let Some(cur) = g.current_desktop else {
                return false;
            };
            g.windows.get(&id).is_some_and(|w| w.desktop == cur)
        }

        fn windows(&self) -> Vec<isize> {
            let g = self.0.lock().unwrap();
            g.z_order
                .iter()
                .copied()
                .filter(|id| g.windows.contains_key(id))
                .collect()
        }

        fn eligible(&self, id: isize, cfg: &toml::Value) -> bool {
            let g = self.0.lock().unwrap();
            let Some(w) = g.windows.get(&id) else {
                return false;
            };
            if !w.visible
                || w.iconic
                || w.owner != 0
                || w.toolwindow
                || w.topmost
                || w.noactivate
                || !w.thickframe
                || w.cloaked
                || w.elevated
            {
                return false;
            }
            if let Some(rules) = cfg.get("rules").and_then(|v| v.as_array()) {
                for rule in rules {
                    let class_ok = rule
                        .get("class")
                        .and_then(|v| v.as_str())
                        .is_none_or(|s| s == "*" || s.eq_ignore_ascii_case(&w.class));
                    let proc_ok = rule
                        .get("process")
                        .and_then(|v| v.as_str())
                        .is_none_or(|s| s.eq_ignore_ascii_case(&w.process));
                    if class_ok && proc_ok {
                        return rule.get("action").and_then(|v| v.as_str()) == Some("tile");
                    }
                }
            }
            true
        }

        fn external(&self, id: isize) -> bool {
            let g = self.0.lock().unwrap();
            let Some(w) = g.windows.get(&id) else {
                return false;
            };
            if !w.visible
                || w.iconic
                || w.owner != 0
                || w.toolwindow
                || w.noactivate
                || w.cloaked
                || w.rect.w < 100
                || w.rect.h < 100
            {
                return false;
            }
            !matches!(
                w.class.as_str(),
                "Shell_TrayWnd"
                    | "Shell_SecondaryTrayWnd"
                    | "Progman"
                    | "WorkerW"
                    | "XamlExplorerHostIslandWindow"
                    | "Windows.UI.Core.CoreWindow"
                    | "TopLevelWindowForOverflowXamlIsland"
            ) && !w.class.starts_with("Planor")
        }

        fn overlay_occluder(&self, id: isize) -> bool {
            if self.external(id) {
                return true;
            }
            let g = self.0.lock().unwrap();
            let Some(w) = g.windows.get(&id) else {
                return false;
            };
            if !w.visible || w.iconic || w.cloaked || w.rect.w < 40 || w.rect.h < 40 {
                return false;
            }
            win32::is_overlay_occluder_class(&w.class)
                || (w.topmost && win32::topmost_overlay_occluder(&w.class, WS_EX_TOPMOST))
        }

        fn class_of(&self, id: isize) -> String {
            self.0
                .lock()
                .unwrap()
                .windows
                .get(&id)
                .map(|w| w.class.clone())
                .unwrap_or_default()
        }

        fn frame(&self, id: isize) -> Option<Rect> {
            self.0.lock().unwrap().windows.get(&id).map(|w| w.rect)
        }

        fn window_rect(&self, id: isize) -> Option<Rect> {
            self.frame(id)
        }

        fn minimum(&self, id: isize, horizontal: bool) -> i32 {
            self.0
                .lock()
                .unwrap()
                .windows
                .get(&id)
                .map(|w| if horizontal { w.min_w } else { w.min_h }.max(1))
                .unwrap_or(1)
        }

        fn owner(&self, id: isize) -> isize {
            self.0
                .lock()
                .unwrap()
                .windows
                .get(&id)
                .map(|w| w.owner)
                .unwrap_or(0)
        }

        fn root_owner(&self, id: isize) -> isize {
            let g = self.0.lock().unwrap();
            let mut cur = id;
            for _ in 0..16 {
                let Some(next) = g.windows.get(&cur).map(|w| w.owner).filter(|&o| o != 0 && o != cur) else {
                    break;
                };
                cur = next;
            }
            cur
        }

        fn foreground(&self) -> isize {
            self.0.lock().unwrap().foreground
        }

        fn is_window(&self, id: isize) -> bool {
            self.0.lock().unwrap().windows.contains_key(&id)
        }

        fn is_visible(&self, id: isize) -> bool {
            self.0
                .lock()
                .unwrap()
                .windows
                .get(&id)
                .is_some_and(|w| w.visible)
        }

        fn is_iconic(&self, id: isize) -> bool {
            self.0
                .lock()
                .unwrap()
                .windows
                .get(&id)
                .is_some_and(|w| w.iconic)
        }

        fn is_cloaked(&self, id: isize) -> bool {
            self.0
                .lock()
                .unwrap()
                .windows
                .get(&id)
                .is_some_and(|w| w.cloaked)
        }

        fn is_zoomed(&self, id: isize) -> bool {
            self.0
                .lock()
                .unwrap()
                .windows
                .get(&id)
                .is_some_and(|w| w.zoomed)
        }

        fn in_movesize(&self, id: isize) -> Option<bool> {
            self.0
                .lock()
                .unwrap()
                .windows
                .get(&id)
                .and_then(|w| w.in_movesize)
        }

        fn has_caption(&self, id: isize) -> bool {
            self.0
                .lock()
                .unwrap()
                .windows
                .get(&id)
                .is_some_and(|w| w.caption)
        }

        fn window_at_point(&self, x: i32, y: i32) -> isize {
            let g = self.0.lock().unwrap();
            for &id in &g.z_order {
                if let Some(w) = g.windows.get(&id)
                    && w.visible
                    && !w.iconic
                    && x >= w.rect.x
                    && x < w.rect.x + w.rect.w
                    && y >= w.rect.y
                    && y < w.rect.y + w.rect.h
                {
                    return id;
                }
            }
            0
        }

        fn activate(&self, id: isize) {
            let mut g = self.0.lock().unwrap();
            if let Some(topmost) = g.windows.get(&id).map(|w| w.topmost) {
                g.foreground = id;
                g.z_order.retain(|&x| x != id);
                // As in Windows: activating a normal window raises it to the top of the normal band,
                // still below every WS_EX_TOPMOST window (#106).
                let at = if topmost {
                    0
                } else {
                    g.z_order
                        .iter()
                        .position(|x| !g.windows.get(x).is_some_and(|w| w.topmost))
                        .unwrap_or(g.z_order.len())
                };
                g.z_order.insert(at, id);
            }
        }

        fn place(&self, items: &[(isize, Rect)]) -> Result<()> {
            let mut g = self.0.lock().unwrap();
            for &(id, r) in items {
                if let Some(w) = g.windows.get_mut(&id) {
                    w.rect = r;
                }
            }
            Ok(())
        }

        fn show_no_activate(&self, id: isize) {
            if let Some(w) = self.0.lock().unwrap().windows.get_mut(&id) {
                w.iconic = false;
                w.visible = true;
            }
        }

        fn minimize_no_activate(&self, id: isize) {
            if let Some(w) = self.0.lock().unwrap().windows.get_mut(&id) {
                w.iconic = true;
            }
        }

        fn restore_window(&self, id: isize) {
            if let Some(w) = self.0.lock().unwrap().windows.get_mut(&id) {
                w.iconic = false;
                w.zoomed = false;
            }
        }

        fn set_topmost(&self, id: isize, topmost: bool) {
            if let Some(w) = self.0.lock().unwrap().windows.get_mut(&id) {
                w.topmost = topmost;
            }
        }

        fn move_window_async(&self, id: isize, x: i32, y: i32) {
            if let Some(w) = self.0.lock().unwrap().windows.get_mut(&id) {
                w.rect.x = x;
                w.rect.y = y;
            }
        }

        fn last_release(&self) -> (u64, (i32, i32)) {
            self.0.lock().unwrap().release
        }

        fn cursor_pos(&self) -> (i32, i32) {
            self.0.lock().unwrap().cursor
        }

        fn set_cursor_pos(&self, x: i32, y: i32) {
            self.0.lock().unwrap().cursor = (x, y);
        }

        fn key_down(&self, vk: i32) -> bool {
            self.0.lock().unwrap().keys_down.contains(&vk)
        }

        fn load_config(&self) -> Result<toml::Value> {
            Ok(self.0.lock().unwrap().config.clone())
        }

        fn holes_class_filter(&self) -> Option<Option<String>> {
            self.0.lock().unwrap().holes_filter.clone()
        }

        fn frames_log_dir(&self) -> Option<std::path::PathBuf> {
            None
        }

        fn max_refresh_hz(&self) -> f64 {
            self.0.lock().unwrap().max_refresh_hz
        }

        fn contact_dpi(&self) -> Option<f64> {
            self.0.lock().unwrap().contact_dpi
        }
    }
}

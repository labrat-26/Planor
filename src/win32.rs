use super::*;
use crossbeam_channel::Sender;
use planor::Rect;
use std::sync::OnceLock;
use windows_sys::Win32::{
    Graphics::{Dwm::*, Gdi::*},
    System::{Console::*, LibraryLoader::*, RemoteDesktop::*},
    UI::{Accessibility::*, Input::*, WindowsAndMessaging::*},
};
impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Rejected(..) => "rejected",
            Event::Peek(_) => "peek",
            Event::Frame(_) => "frame",
            Event::Window(..) => "window",
            Event::Key(_) => "key",
            Event::Contacts(_) => "contacts",
            Event::Suspend(_) => "suspend",
            Event::Topology => "topology",
            Event::Stop => "stop",
        }
    }
}
pub enum Event {
    Rejected(u64, Vec<(isize, Rect)>),
    Peek(isize),
    Frame(diag::Frame),
    Window(u32, isize),
    Key(String),
    Contacts(Vec<(f64, f64)>),
    Suspend(bool),
    Topology,
    Stop,
}
static BUS: OnceLock<Sender<Event>> = OnceLock::new();
pub fn emit(e: Event) {
    if let Some(tx) = BUS.get() {
        let _ = tx.send(e);
    }
}
/// Screen rectangles of the visible peeks (parked windows shown as DWM thumbnails) and the window each
/// one reveals. Published by the render thread, read by the low-level mouse hook.
static PEEKS: std::sync::Mutex<Vec<(Rect, isize)>> = std::sync::Mutex::new(Vec::new());
static SWALLOW_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn set_peeks(peeks: Vec<(Rect, isize)>) {
    if let Ok(mut p) = PEEKS.lock() {
        *p = peeks
    }
}
/// The overlay is click-through, so a click on a peek never reaches it. This hook sees every left
/// click first; one that lands on a peek is consumed and turned into a Peek event.
unsafe extern "system" fn mouse(code: i32, w: usize, l: isize) -> isize {
    use std::sync::atomic::Ordering::*;
    if code >= 0 {
        if w == WM_LBUTTONDOWN as usize {
            let p = unsafe { (*(l as *const MSLLHOOKSTRUCT)).pt };
            let hit = PEEKS.try_lock().ok().and_then(|v| {
                v.iter()
                    .find(|(r, _)| p.x >= r.x && p.y >= r.y && p.x < r.x + r.w && p.y < r.y + r.h)
                    .map(|e| e.1)
            });
            if let Some(id) = hit {
                SWALLOW_UP.store(true, Release);
                emit(Event::Peek(id));
                return 1;
            }
        } else if w == WM_LBUTTONUP as usize && SWALLOW_UP.swap(false, AcqRel) {
            return 1;
        }
    }
    unsafe { CallNextHookEx(null_mut(), code, w, l) }
}
/// Win+W (diagnostics builds only): open the note box instead of the Widgets panel. Runs in a low-level
/// hook, so it only spawns a thread and returns.
unsafe extern "system" fn keyboard(code: i32, w: usize, l: isize) -> isize {
    if code >= 0 && (w == WM_KEYDOWN as usize || w == WM_SYSKEYDOWN as usize) {
        let k = unsafe { &*(l as *const KBDLLHOOKSTRUCT) };
        let win = unsafe {
            KeyboardAndMouse::GetAsyncKeyState(0x5B) < 0
                || KeyboardAndMouse::GetAsyncKeyState(0x5C) < 0
        };
        if k.vkCode == 0x57 && win {
            diag::trace("Win+W seen by the keyboard hook");
            crate::note::open();
            // Pressing Win and releasing it with nothing in between opens the Start menu; a keystroke
            // that maps to nothing in between cancels that.
            unsafe {
                KeyboardAndMouse::keybd_event(0xE8, 0, 0, 0);
                KeyboardAndMouse::keybd_event(0xE8, 0, KeyboardAndMouse::KEYEVENTF_KEYUP, 0);
            }
            return 1;
        }
    }
    unsafe { CallNextHookEx(null_mut(), code, w, l) }
}
/// Set by the daemon once every window has been restored and the journal removed.
pub static SHUTDOWN_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
unsafe extern "system" fn ctrl(_: u32) -> i32 {
    emit(Event::Stop);
    // For CTRL_CLOSE/LOGOFF/SHUTDOWN Windows kills the process as soon as this handler returns (or
    // after ~5s). Returning early would strand every window parked off-screen, so block until the
    // daemon has finished restoring them.
    let until = std::time::Instant::now() + std::time::Duration::from_millis(4500);
    while !SHUTDOWN_DONE.load(std::sync::atomic::Ordering::Acquire)
        && std::time::Instant::now() < until
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    1
}
unsafe extern "system" fn hook(
    _: HWINEVENTHOOK,
    e: u32,
    h: HWND,
    obj: i32,
    child: i32,
    _: u32,
    _: u32,
) {
    if obj == 0 && child == 0 && !h.is_null() {
        emit(Event::Window(e, h as isize))
    }
}
unsafe extern "system" fn proc(h: HWND, msg: u32, w: usize, l: isize) -> isize {
    match msg {
        WM_INPUT => {
            for pts in input::contacts(l) {
                emit(Event::Contacts(pts))
            }
        }
        WM_DISPLAYCHANGE => emit(Event::Topology),
        WM_POWERBROADCAST => match w {
            4 => emit(Event::Suspend(true)),
            7 | 18 => emit(Event::Suspend(false)),
            _ => (),
        },
        WM_WTSSESSION_CHANGE => match w {
            7 => emit(Event::Suspend(true)),
            8 => emit(Event::Suspend(false)),
            _ => (),
        },
        _ => (),
    }
    unsafe { DefWindowProcW(h, msg, w, l) }
}
pub fn events(
    tx: Sender<Event>,
    cfg: toml::Value,
    reload: crossbeam_channel::Receiver<toml::Value>,
) {
    let _ = BUS.set(tx);
    let note_hotkey = cfg["diagnostics"]["enabled"].as_bool() == Some(true);
    let test_mode = crate::test_mode();
    std::thread::spawn(move || unsafe {
        SetConsoleCtrlHandler(Some(ctrl), 1);
        let name = wide("Planor.Events");
        let inst = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            lpszClassName: name.as_ptr(),
            hInstance: inst,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let h = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            name.as_ptr(),
            name.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            inst,
            null(),
        );
        if h.is_null() {
            emit(Event::Stop);
            return;
        }
        WTSRegisterSessionNotification(h, 0);
        if !test_mode {
            let raw = RAWINPUTDEVICE {
                usUsagePage: 13,
                usUsage: 5,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: h,
            };
            RegisterRawInputDevices(&raw, 1, size_of::<RAWINPUTDEVICE>() as _);
        }
        let mut keys = if test_mode {
            Vec::new()
        } else {
            input::bindings(h, &cfg)
        };
        let taskbar = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        let hooks = [
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
            (EVENT_OBJECT_SHOW, EVENT_OBJECT_HIDE),
            (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
            (EVENT_OBJECT_DESTROY, EVENT_OBJECT_DESTROY),
            (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
            (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
        ]
        .map(|(a, b)| {
            SetWinEventHook(
                a,
                b,
                null_mut(),
                Some(hook),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        });
        if test_mode {
            if note_hotkey {
                diag::trace("test mode: skipping global hotkeys and low-level input hooks");
            }
        } else {
            // The mouse hook gets a thread of its own: a low-level hook must answer within a few
            // milliseconds or all system input lags, so it must never share a thread with anything that
            // can block (touchpad parsing, window events).
            std::thread::spawn(move || {
                let install = || {
                    let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), GetModuleHandleW(null()), 0);
                    let keys = if note_hotkey {
                        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), GetModuleHandleW(null()), 0)
                    } else {
                        null_mut()
                    };
                    (hook, keys)
                };
                let (mut hook, mut keys) = install();
                diag::trace(format!("input hooks installed: mouse={} keyboard={}", !hook.is_null(), !keys.is_null()));
                // Windows silently removes a low-level hook that ever answers too slowly (and after some session
                // changes), which is how Win+W and peek clicks would stop working with nothing logged. Put the
                // hooks back every few seconds.
                SetTimer(null_mut(), 1, 4000, None);
                let mut m = MSG::default();
                while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
                    if m.message == WM_TIMER {
                        if !hook.is_null() {
                            UnhookWindowsHookEx(hook);
                        }
                        if !keys.is_null() {
                            UnhookWindowsHookEx(keys);
                        }
                        (hook, keys) = install();
                        continue;
                    }
                    TranslateMessage(&m);
                    DispatchMessageW(&m);
                }
                if !hook.is_null() {
                    UnhookWindowsHookEx(hook);
                }
                if !keys.is_null() {
                    UnhookWindowsHookEx(keys);
                }
            });
        }
        SetTimer(h, 1, 200, None);
        let mut m = MSG::default();
        while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
            if m.message == WM_HOTKEY {
                if let Some(key) = keys.get(m.wParam.wrapping_sub(1)) {
                    emit(Event::Key(key.clone()))
                }
            } else if m.message == taskbar {
                emit(Event::Topology)
            } else if m.message == WM_TIMER {
                if let Ok(cfg) = reload.try_recv() {
                    if !test_mode {
                        for id in 1..=keys.len() {
                            KeyboardAndMouse::UnregisterHotKey(h, id as _);
                        }
                        keys = input::bindings(h, &cfg)
                    }
                }
            }
            TranslateMessage(&m);
            DispatchMessageW(&m);
        }
        for hook in hooks {
            UnhookWinEvent(hook);
        }
        DestroyWindow(h);
    });
}
pub fn rect(r: RECT) -> Rect {
    Rect {
        x: r.left,
        y: r.top,
        w: r.right - r.left,
        h: r.bottom - r.top,
    }
}
pub fn frame(id: isize) -> Option<Rect> {
    unsafe {
        let mut r = RECT::default();
        if DwmGetWindowAttribute(
            id as _,
            DWMWA_EXTENDED_FRAME_BOUNDS as _,
            (&mut r as *mut RECT).cast(),
            size_of::<RECT>() as _,
        ) >= 0
            || GetWindowRect(id as _, &mut r) != 0
        {
            Some(rect(r))
        } else {
            None
        }
    }
}
pub fn monitors() -> Vec<(String, Rect, Rect)> {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, l: isize) -> i32 {
        unsafe {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as _;
            if GetMonitorInfoW(m, &mut info.monitorInfo) != 0 {
                let mut dev = DISPLAY_DEVICEW::default();
                dev.cb = size_of::<DISPLAY_DEVICEW>() as _;
                EnumDisplayDevicesW(info.szDevice.as_ptr(), 0, &mut dev, 1);
                let text = |s: &[u16]| {
                    String::from_utf16_lossy(
                        &s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())],
                    )
                };
                let mut key = text(&dev.DeviceID);
                if key.is_empty() {
                    key = text(&info.szDevice)
                }
                let v = &mut *(l as *mut Vec<(String, Rect, Rect)>);
                let item = (
                    key,
                    rect(info.monitorInfo.rcWork),
                    rect(info.monitorInfo.rcMonitor),
                );
                if info.monitorInfo.dwFlags & 1 != 0 {
                    v.insert(0, item)
                } else {
                    v.push(item)
                }
            }
            1
        }
    }
    let mut v = vec![];
    unsafe { EnumDisplayMonitors(null_mut(), null(), Some(cb), &mut v as *mut _ as isize) };
    v
}
pub fn windows() -> Vec<isize> {
    unsafe extern "system" fn cb(h: HWND, l: isize) -> i32 {
        unsafe { (*(l as *mut Vec<isize>)).push(h as isize) };
        1
    }
    let mut v = vec![];
    unsafe { EnumWindows(Some(cb), &mut v as *mut _ as isize) };
    v
}
fn elevated(process: HANDLE) -> Option<bool> {
    unsafe {
        let mut t = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut t) == 0 {
            return None;
        }
        let _guard = File::from_raw_handle(t);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut n = 0;
        (GetTokenInformation(
            t,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as _,
            &mut n,
        ) != 0)
            .then_some(elevation.TokenIsElevated != 0)
    }
}
pub fn class_of(id: isize) -> String {
    let mut class = [0u16; 128];
    let n = unsafe { GetClassNameW(id as HWND, class.as_mut_ptr(), 128) }.max(0) as usize;
    String::from_utf16_lossy(&class[..n])
}
/// A normal top-level app window Planor does not manage (an elevated Task Manager, a rule-ignored app).
/// The overlay must sit below it and the focus border must not stay on the previously focused tile.
pub fn external(id: isize) -> bool {
    unsafe {
        let h = id as HWND;
        if h.is_null()
            || IsWindowVisible(h) == 0
            || IsIconic(h) != 0
            || !GetWindow(h, GW_OWNER).is_null()
            || GetWindowLongW(h, GWL_EXSTYLE) as u32 & (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) != 0
        {
            return false;
        }
        let mut cloaked = 0u32;
        DwmGetWindowAttribute(h, DWMWA_CLOAKED as _, (&mut cloaked as *mut u32).cast(), 4);
        let mut size = RECT::default();
        GetWindowRect(h, &mut size);
        if cloaked != 0 || size.right - size.left < 100 || size.bottom - size.top < 100 {
            return false;
        }
        let mut class = [0u16; 128];
        let n = GetClassNameW(h, class.as_mut_ptr(), 128).max(0) as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        !matches!(
            class.as_str(),
            "Shell_TrayWnd"
                | "Shell_SecondaryTrayWnd"
                | "Progman"
                | "WorkerW"
                | "XamlExplorerHostIslandWindow"
                | "Windows.UI.Core.CoreWindow"
                | "TopLevelWindowForOverflowXamlIsland"
        ) && !class.starts_with("Planor")
    }
}
pub fn eligible(id: isize, cfg: &toml::Value) -> bool {
    unsafe {
        let h = id as HWND;
        if IsWindowVisible(h) == 0
            || IsIconic(h) != 0
            || !GetWindow(h, GW_OWNER).is_null()
            || GetWindowLongW(h, GWL_EXSTYLE) as u32
                & (WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE)
                != 0
            || GetWindowLongW(h, GWL_STYLE) as u32 & WS_THICKFRAME == 0
        {
            return false;
        }
        let mut cloaked = 0u32;
        DwmGetWindowAttribute(h, DWMWA_CLOAKED as _, (&mut cloaked as *mut u32).cast(), 4);
        if cloaked != 0 {
            return false;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(h, &mut pid);
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let _process = File::from_raw_handle(handle);
        if elevated(GetCurrentProcess()) != Some(true) && elevated(handle) != Some(false) {
            return false;
        }
        let mut path = [0; 1024];
        let mut len = 1024;
        let process = if QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut len) != 0 {
            String::from_utf16_lossy(&path[..len as usize])
        } else {
            String::new()
        };
        let mut class = [0; 256];
        let n = GetClassNameW(h, class.as_mut_ptr(), 256);
        let class = String::from_utf16_lossy(&class[..n as usize]);
        // Never let a user's real daemon hijack sentinel windows owned by isolated test runs.
        if instance().is_empty() && class.starts_with("PlanorSentinel") {
            return false;
        }
        for rule in cfg["rules"].as_array().unwrap() {
            if rule
                .get("class")
                .and_then(|v| v.as_str())
                .is_none_or(|s| s == "*" || s.eq_ignore_ascii_case(&class))
                && rule
                    .get("process")
                    .and_then(|v| v.as_str())
                    .is_none_or(|s| {
                        s.eq_ignore_ascii_case(process.rsplit('\\').next().unwrap_or(""))
                    })
            {
                return rule["action"].as_str() == Some("tile");
            }
        }
        true
    }
}
pub fn minimum(id: isize, horizontal: bool) -> i32 {
    unsafe {
        let mut info = MINMAXINFO::default();
        let mut result = 0;
        let ok = SendMessageTimeoutW(
            id as _,
            WM_GETMINMAXINFO,
            0,
            &mut info as *mut _ as _,
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            40,
            &mut result,
        );
        if ok == 0 {
            return 1;
        }
        let mut r = RECT::default();
        GetWindowRect(id as _, &mut r);
        let outer = rect(r);
        let visible = frame(id).unwrap_or(outer);
        if horizontal {
            (info.ptMinTrackSize.x - outer.w + visible.w).max(1)
        } else {
            (info.ptMinTrackSize.y - outer.h + visible.h).max(1)
        }
    }
}
/// DWORD settings Planor forces to 0 while it runs: Snap assist / layouts / bar / joint resize, and the
/// precision-touchpad three- and four-finger gestures (Planor reads the raw touchpad contacts itself).
// The touchpad gesture values are deliberately NOT managed any more: writing them from outside the
// Settings app does not reliably take effect, and the only mode that worked (the media preset) makes
// swipes change the volume. See docs/BUG_TRACKPAD_SYSTEM_GESTURES.md. Set the gestures to "Nothing"
// in Settings > Bluetooth & devices > Touchpad > Advanced gestures.
const SYSTEM_DWORDS: [(&str, &str); 4] = [
    (r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced", "SnapAssist"),
    (r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced", "EnableSnapAssistFlyout"),
    (r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced", "EnableSnapBar"),
    (r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced", "JointResize"),
];
fn reg_get(key: &str, name: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::*;
    unsafe {
        let mut v = 0u32;
        let mut len = 4;
        (RegGetValueW(
            HKEY_CURRENT_USER,
            wide(key).as_ptr(),
            wide(name).as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut v as *mut u32).cast(),
            &mut len,
        ) == 0)
            .then_some(v)
    }
}
fn reg_set(key: &str, name: &str, v: Option<u32>) {
    use windows_sys::Win32::System::Registry::*;
    unsafe {
        match v {
            Some(v) => {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    wide(key).as_ptr(),
                    wide(name).as_ptr(),
                    REG_DWORD,
                    (&v as *const u32).cast(),
                    4,
                );
            }
            None => {
                let mut k = null_mut();
                if RegOpenKeyExW(HKEY_CURRENT_USER, wide(key).as_ptr(), 0, KEY_SET_VALUE, &mut k) == 0 {
                    RegDeleteValueW(k, wide(name).as_ptr());
                    RegCloseKey(k);
                }
            }
        }
    }
}
fn snap_active() -> bool {
    use windows_sys::Win32::System::Registry::*;
    unsafe {
        let mut data = [0u16; 8];
        let mut len = 16;
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(r"Control Panel\Desktop").as_ptr(),
            wide("WindowArrangementActive").as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            data.as_mut_ptr().cast(),
            &mut len,
        ) == 0
            && data[0] == 49
    }
}
fn set_snap(on: bool) {
    // SPI_SETWINDOWARRANGEMENTACTIVE, persisted to the profile and broadcast.
    unsafe { SystemParametersInfoW(0x0083, on as u32, on as usize as *mut _, 3) };
}
/// The gesture values live in the registry, but Windows only re-reads them when the touchpad settings
/// are re-applied. Read the parameter block (SPI_GETTOUCHPADPARAMETERS; its size varies by Windows
/// build, so probe for it) and write it straight back, then announce the change.
fn refresh_touchpad() {
    unsafe {
        for size in (8..=512usize).step_by(4) {
            let mut buf = vec![0u32; size / 4];
            buf[0] = 1;
            if SystemParametersInfoW(0xAE, size as u32, buf.as_mut_ptr().cast(), 0) != 0 {
                SystemParametersInfoW(0xAF, size as u32, buf.as_mut_ptr().cast(), 2);
                break;
            }
        }
        let mut result = 0usize;
        SendMessageTimeoutW(
            0xffff as HWND,
            WM_SETTINGCHANGE,
            0,
            wide("PrecisionTouchPad").as_ptr() as isize,
            2,
            1000,
            &mut result,
        );
    }
}
#[derive(Serialize, Deserialize)]
struct SystemBackup {
    snap: bool,
    values: Vec<(String, String, Option<u32>)>,
}
/// Disable Windows Snap / tiling assist and the touchpad system gestures. The original values are
/// saved in `backup` the first time (a crashed session keeps its backup), and put back by
/// `restore_system`.
pub fn tweak_system(backup: &std::path::Path) {
    if !instance().is_empty() {
        return; // isolated test daemons must not touch the user's settings
    }
    if !backup.exists() {
        let mut values: Vec<(String, String, Option<u32>)> = vec![];
        for (k, n) in SYSTEM_DWORDS {
            if !values.iter().any(|(a, b, _)| a == k && b == n) {
                values.push((k.into(), n.into(), reg_get(k, n)));
            }
        }
        let saved = SystemBackup { snap: snap_active(), values };
        if let Ok(json) = serde_json::to_vec(&saved) {
            let _ = std::fs::write(backup, json);
        }
    }
    set_snap(false);
    for (k, n) in SYSTEM_DWORDS {
        reg_set(k, n, Some(0));
    }
}
/// Put back what `tweak_system` changed. Returns false when there was nothing to restore.
pub fn restore_system(backup: &std::path::Path) -> bool {
    let Some(saved) = std::fs::read(backup)
        .ok()
        .and_then(|b| serde_json::from_slice::<SystemBackup>(&b).ok())
    else {
        return false;
    };
    set_snap(saved.snap);
    for (k, n, v) in &saved.values {
        reg_set(k, n, *v);
    }
    let _ = std::fs::remove_file(backup);
    refresh_touchpad();
    true
}
pub fn activate(id: isize) {
    unsafe {
        let fg = GetWindowThreadProcessId(GetForegroundWindow(), null_mut());
        let own = GetCurrentThreadId();
        let attached = fg != 0 && fg != own && AttachThreadInput(own, fg, 1) != 0;
        SetForegroundWindow(id as _);
        if attached {
            AttachThreadInput(own, fg, 0);
        }
    }
}
pub fn place(items: &[(isize, Rect)]) -> Result<()> {
    unsafe {
        let mut batch = BeginDeferWindowPos(items.len() as _);
        for &(id, r) in items {
            let mut outer = RECT::default();
            if GetWindowRect(id as _, &mut outer) == 0 {
                continue;
            }
            let full = rect(outer);
            let vis = frame(id).unwrap_or(full);
            if r == vis {
                continue;
            }
            batch = DeferWindowPos(
                batch,
                id as _,
                null_mut(),
                r.x + full.x - vis.x,
                r.y + full.y - vis.y,
                r.w + full.w - vis.w,
                r.h + full.h - vis.h,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER,
            );
            if batch.is_null() {
                return Err(io::Error::last_os_error().into());
            }
        }
        if EndDeferWindowPos(batch) == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
}

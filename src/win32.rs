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
            Event::Wheel { .. } => "wheel",
            Event::Button { .. } => "button",
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
    /// Mouse wheel turned with the Windows key held (`delta` in notches of 120; `tilt`: the horizontal
    /// wheel; `shift`: Shift held too), at screen point `x`, `y`.
    Wheel { delta: i32, tilt: bool, shift: bool, x: i32, y: i32 },
    /// A mouse button went down or up (only reported while a gesture recording runs). `button`: left, right, middle.
    Button { button: &'static str, down: bool, x: i32, y: i32 },
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
/// #67: where a mouse button was last released, with a sequence number that grows on every release. Written by the
/// low-level mouse hook from the WM_xBUTTONUP point, read by the drag code so the drop lands where the button came
/// up, not where the cursor drifted to before the next tick (up to ~32 ms later). Left and right share one slot: a
/// low-level hook's button naming under swapped buttons is not worth depending on, and the OS move loop ends on the
/// first primary release anyway.
static RELEASE: std::sync::Mutex<(u64, (i32, i32))> = std::sync::Mutex::new((0, (0, 0)));
fn record_release(p: (i32, i32)) {
    // try_lock: a low-level hook must never block; a missed sample only falls back to the tick's cursor.
    if let Ok(mut r) = RELEASE.try_lock() {
        *r = (r.0 + 1, p);
    }
}
/// Last button release as `(sequence, point)`; sequence 0 = none seen (hooks off, e.g. test mode).
pub fn last_release() -> (u64, (i32, i32)) {
    RELEASE.lock().map(|r| *r).unwrap_or((0, (0, 0)))
}
/// The overlay is click-through, so a click on a peek never reaches it. This hook sees every left
/// click first; one that lands on a peek is consumed and turned into a Peek event.
unsafe extern "system" fn mouse(code: i32, w: usize, l: isize) -> isize {
    let _span = crate::perf::SpanGuard::new(crate::perf::SpanKind::HookMouse);
    use std::sync::atomic::Ordering::*;
    if code >= 0 {
        if w == WM_LBUTTONUP as usize || w == WM_RBUTTONUP as usize {
            let p = unsafe { (*(l as *const MSLLHOOKSTRUCT)).pt };
            record_release((p.x, p.y));
        }
        if crate::input::RECORD_ALL.load(Relaxed) {
            let button = match w as u32 {
                WM_LBUTTONDOWN | WM_LBUTTONUP => Some("left"),
                WM_RBUTTONDOWN | WM_RBUTTONUP => Some("right"),
                WM_MBUTTONDOWN | WM_MBUTTONUP => Some("middle"),
                _ => None,
            };
            if let Some(button) = button {
                let p = unsafe { (*(l as *const MSLLHOOKSTRUCT)).pt };
                let down = matches!(w as u32, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN);
                emit(Event::Button { button, down, x: p.x, y: p.y });
            }
        }
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
        } else if w == WM_MOUSEWHEEL as usize || w == WM_MOUSEHWHEEL as usize {
            // Win + MOUSE wheel scrolls the strip under the cursor (Win + Shift + wheel rolls its group). Only the
            // mouse (owner's choice): a precision touchpad scrolls in fine steps (-3, -7, ... in the owner's
            // recording) and a mouse wheel in whole notches of 120, so fine steps pass through to the app.
            let down = |vk| unsafe { KeyboardAndMouse::GetAsyncKeyState(vk) < 0 };
            let info = unsafe { &*(l as *const MSLLHOOKSTRUCT) };
            let delta = (info.mouseData >> 16) as u16 as i16 as i32;
            if (down(0x5B) || down(0x5C)) && delta != 0 && delta % 120 == 0 {
                emit(Event::Wheel {
                    delta,
                    tilt: w == WM_MOUSEHWHEEL as usize,
                    shift: down(0x10),
                    x: info.pt.x,
                    y: info.pt.y,
                });
                // Releasing Win with nothing else pressed would open the Start menu; a keystroke that maps
                // to nothing in between cancels that (same as Win+W).
                unsafe {
                    KeyboardAndMouse::keybd_event(0xE8, 0, 0, 0);
                    KeyboardAndMouse::keybd_event(0xE8, 0, KeyboardAndMouse::KEYEVENTF_KEYUP, 0);
                }
                return 1;
            }
        }
    }
    unsafe { CallNextHookEx(null_mut(), code, w, l) }
}
/// LLKHF_INJECTED (0x10) or LLKHF_LOWER_IL_INJECTED (0x02): the key press was synthesised, not typed.
pub(crate) fn is_injected_key(flags: u32) -> bool {
    flags & 0x12 != 0
}
#[cfg(test)]
mod injected_key_tests {
    use super::is_injected_key;
    #[test]
    fn injected_flags_are_recognised() {
        assert!(is_injected_key(0x10));
        assert!(is_injected_key(0x02));
        assert!(!is_injected_key(0));
        assert!(!is_injected_key(0x01 | 0x20));
    }
}
/// Win+W (diagnostics builds only): open the note box instead of the Widgets panel. Runs in a low-level
/// hook, so it only spawns a thread and returns.
unsafe extern "system" fn keyboard(code: i32, w: usize, l: isize) -> isize {
    let _span = crate::perf::SpanGuard::new(crate::perf::SpanKind::HookKeyboard);
    if code >= 0 && (w == WM_KEYDOWN as usize || w == WM_SYSKEYDOWN as usize) {
        let k = unsafe { &*(l as *const KBDLLHOOKSTRUCT) };
        if is_injected_key(k.flags) {
            // #71: Planor's own synthetic Alt / 0xE8 taps (win32::activate) must never act as hotkeys.
            return unsafe { CallNextHookEx(null_mut(), code, w, l) };
        }
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
        crate::perf::register_thread("events");
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
                crate::perf::register_thread("hooks");
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
            } else if m.message == WM_TIMER
                && let Ok(cfg) = reload.try_recv()
                && !test_mode
            {
                for id in 1..=keys.len() {
                    KeyboardAndMouse::UnregisterHotKey(h, id as _);
                }
                keys = input::bindings(h, &cfg)
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
/// Offset of the visible frame inside the full window rectangle (the invisible resize borders): left, top.
pub fn insets(id: isize) -> Option<(i32, i32)> {
    unsafe {
        let mut r = RECT::default();
        if GetWindowRect(id as _, &mut r) == 0 {
            return None;
        }
        let v = frame(id)?;
        Some(((v.x - r.left).max(0), (v.y - r.top).max(0)))
    }
}
pub fn monitors() -> Vec<(String, Rect, Rect)> {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, l: isize) -> i32 {
        unsafe {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as _;
            if GetMonitorInfoW(m, &mut info.monitorInfo) != 0 {
                let mut dev = DISPLAY_DEVICEW {
                    cb: size_of::<DISPLAY_DEVICEW>() as _,
                    ..Default::default()
                };
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
static SELF_ELEVATED: OnceLock<bool> = OnceLock::new();
fn self_elevated() -> bool {
    *SELF_ELEVATED.get_or_init(|| elevated(unsafe { GetCurrentProcess() }) == Some(true))
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
pub fn is_cloaked(id: isize) -> bool {
    unsafe {
        let mut cloaked = 0u32;
        DwmGetWindowAttribute(
            id as HWND,
            DWMWA_CLOAKED as _,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as _,
        ) >= 0
            && cloaked != 0
    }
}
pub fn is_hung(id: isize) -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn IsHungAppWindow(hwnd: HWND) -> i32;
    }
    unsafe { IsHungAppWindow(id as HWND) != 0 }
}
/// Returns `Some(true)` if the window's UI thread is responsive and currently in a move/size modal
/// loop (`GUI_INMOVESIZE`), `Some(false)` if responsive and not in move/size, or `None` if the
/// thread is hung/unresponsive or cannot be queried.
pub fn in_movesize(id: isize) -> Option<bool> {
    if is_hung(id) {
        return None;
    }
    unsafe {
        let tid = GetWindowThreadProcessId(id as HWND, null_mut());
        if tid == 0 {
            return None;
        }
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(tid, &mut info) == 0 {
            return None;
        }
        Some((info.flags & GUI_INMOVESIZE) != 0)
    }
}
/// Classes of top-level system flyouts and Planor's own floating UI that the focus-border overlay
/// must never paint over when visible and uncloaked (Start menu, Search, Quick Settings, tray
/// overflow, Win+W note box, Settings, Gesture Guide).
pub fn is_overlay_occluder_class(class: &str) -> bool {
    matches!(
        class,
        "Windows.UI.Core.CoreWindow"
            | "XamlExplorerHostIslandWindow"
            | "TopLevelWindowForOverflowXamlIsland"
            | "Shell_InputSwitchTopLevelWindow"
            | "NotifyIconOverflowWindow"
            | "#32768"
            | "Planor.Note"
            | "PlanorSettings"
            | "Planor.Guide"
            | "Planor.GestureGuide"
    )
}
/// An always-on-top floating window the overlay must not paint over: a browser or player
/// Picture-in-Picture window, a PowerToys "Always On Top" app (#106). Only affects overlay holes;
/// tiling already skips every `WS_EX_TOPMOST` window (`eligible`). Excluded: the taskbars and the
/// desktop (topmost or full-screen shell surfaces that would hole whole monitors), every Planor
/// window (its own `Planor.Composition` overlay hosts are topmost and monitor-sized; its floating UI
/// is listed by class above), and click-through (`WS_EX_TRANSPARENT`) visual overlays such as game
/// or recording HUDs, which the user cannot interact with.
pub fn topmost_overlay_occluder(class: &str, ex_style: u32, fills_monitor: bool) -> bool {
    ex_style & WS_EX_TOPMOST != 0
        && ex_style & WS_EX_TRANSPARENT == 0
        // Tooltips and monitor-filling topmost windows (borderless fullscreen games, the Snipping
        // Tool full-screen frame) would hole the whole screen or flicker on hover (#102 review).
        && !fills_monitor
        && !class.eq_ignore_ascii_case("tooltips_class32")
        && !class.starts_with("Planor")
        && !matches!(class, "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Progman" | "WorkerW")
}
/// True if `id` is an unmanaged window, system flyout, or Planor floating UI window that the
/// overlay must stay clear of when it sits above managed tiles in Z-order.
pub fn overlay_occluder(id: isize) -> bool {
    if external(id) {
        return true;
    }
    unsafe {
        let h = id as HWND;
        if h.is_null() || IsWindowVisible(h) == 0 || IsIconic(h) != 0 {
            return false;
        }
        let mut cloaked = 0u32;
        DwmGetWindowAttribute(h, DWMWA_CLOAKED as _, (&mut cloaked as *mut u32).cast(), 4);
        if cloaked != 0 {
            return false;
        }
        let mut size = RECT::default();
        if GetWindowRect(h, &mut size) == 0
            || size.right - size.left < 40
            || size.bottom - size.top < 40
        {
            return false;
        }
        let class = class_of(id);
        if is_overlay_occluder_class(&class) {
            return true;
        }
        // A topmost window covering (nearly) all of its monitor is a fullscreen game/screenshot
        // frame, not a floating app window: holing it would blank the overlay for that monitor.
        let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let fills_monitor = GetMonitorInfoW(mon, &mut info) != 0
            && size.left <= info.rcMonitor.left + 2
            && size.top <= info.rcMonitor.top + 2
            && size.right >= info.rcMonitor.right - 2
            && size.bottom >= info.rcMonitor.bottom - 2;
        topmost_overlay_occluder(&class, GetWindowLongW(h, GWL_EXSTYLE) as u32, fills_monitor)
    }
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
        if !self_elevated() && elevated(handle) != Some(false) {
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
        // Never let a user's real daemon hijack sentinel windows owned by isolated test runs, and never tile
        // Planor's own settings window: it is a dialog and floats.
        if never_tiled_class(&class, instance().is_empty()) {
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
    let (w, h) = minimum_size(id);
    if horizontal { w } else { h }
}
/// Query both minimum width and minimum height in a single `WM_GETMINMAXINFO` round-trip.
pub fn minimum_size(id: isize) -> (i32, i32) {
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
            return (1, 1);
        }
        let mut r = RECT::default();
        GetWindowRect(id as _, &mut r);
        let outer = rect(r);
        let visible = frame(id).unwrap_or(outer);
        (
            (info.ptMinTrackSize.x - outer.w + visible.w).max(1),
            (info.ptMinTrackSize.y - outer.h + visible.h).max(1),
        )
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
        let fg_hwnd = GetForegroundWindow();
        let fg = GetWindowThreadProcessId(fg_hwnd, null_mut());
        let target = GetWindowThreadProcessId(id as _, null_mut());
        let own = GetCurrentThreadId();
        let att_fg = fg != 0 && fg != own && AttachThreadInput(own, fg, 1) != 0;
        let att_target =
            target != 0 && target != own && target != fg && AttachThreadInput(own, target, 1) != 0;
        let mut ret = SetForegroundWindow(id as _);
        if ret == 0 {
            // Unassigned VK 0xE8 satisfies Windows' foreground-lock input check without triggering
            // app menus or shortcuts (same technique used for Win+W / Win+Wheel).
            KeyboardAndMouse::keybd_event(0xE8, 0, 0, 0);
            KeyboardAndMouse::keybd_event(0xE8, 0, KeyboardAndMouse::KEYEVENTF_KEYUP, 0);
            ret = SetForegroundWindow(id as _);
        }
        if ret == 0 {
            // Fallback: Alt tap with 0xE8 in between so menu bars do not open.
            KeyboardAndMouse::keybd_event(
                KeyboardAndMouse::VK_MENU as u8,
                0,
                KeyboardAndMouse::KEYEVENTF_EXTENDEDKEY,
                0,
            );
            KeyboardAndMouse::keybd_event(0xE8, 0, 0, 0);
            KeyboardAndMouse::keybd_event(0xE8, 0, KeyboardAndMouse::KEYEVENTF_KEYUP, 0);
            KeyboardAndMouse::keybd_event(
                KeyboardAndMouse::VK_MENU as u8,
                0,
                KeyboardAndMouse::KEYEVENTF_EXTENDEDKEY | KeyboardAndMouse::KEYEVENTF_KEYUP,
                0,
            );
            let _ = SetForegroundWindow(id as _);
        }
        if att_target {
            AttachThreadInput(own, target, 0);
        }
        if att_fg {
            AttachThreadInput(own, fg, 0);
        }
    }
}
/// Move windows without waiting for their apps. Parking happens behind a stand-in, and the open/close
/// animation's last frame is held until the asynchronous move reaches its target (or a bounded handoff
/// timeout), then verified again after settling. A synchronous move waited up to 68 ms for apps still starting.
/// Run `apply` for every item in `items` without short-circuiting on failure, returning the first
/// error encountered (if any) once all items have been attempted.
fn place_batch_continue(
    items: &[(isize, Rect)],
    mut apply: impl FnMut(isize, Rect) -> Result<()>,
) -> Result<()> {
    let mut first_err = None;
    for &(id, r) in items {
        if let Err(e) = apply(id, r)
            && first_err.is_none()
        {
            first_err = Some(e);
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
pub fn place_async(items: &[(isize, Rect)]) {
    let _ = place_async_checked(items);
}
pub fn place_async_checked(items: &[(isize, Rect)]) -> Result<()> {
    place_batch_continue(items, |id, r| unsafe {
        let mut outer = RECT::default();
        if GetWindowRect(id as _, &mut outer) == 0 {
            return Ok(());
        }
        let full = rect(outer);
        let vis = frame(id).unwrap_or(full);
        if r == vis {
            return Ok(());
        }
        if SetWindowPos(
            id as _,
            null_mut(),
            r.x + full.x - vis.x,
            r.y + full.y - vis.y,
            r.w + full.w - vis.w,
            r.h + full.h - vis.h,
            SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
        ) == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    })
}
pub fn place(items: &[(isize, Rect)]) -> Result<()> {
    unsafe {
        let mut moves = Vec::new();
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
            moves.push((
                id,
                Rect {
                    x: r.x + full.x - vis.x,
                    y: r.y + full.y - vis.y,
                    w: r.w + full.w - vis.w,
                    h: r.h + full.h - vis.h,
                },
            ));
        }
        if moves.is_empty() {
            return Ok(());
        }
        let _span = crate::perf::SpanGuard::new(crate::perf::SpanKind::Win32Place);
        let mut batch = BeginDeferWindowPos(moves.len() as _);
        for (id, mr) in moves {
            batch = DeferWindowPos(
                batch,
                id as _,
                null_mut(),
                mr.x,
                mr.y,
                mr.w,
                mr.h,
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

#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(uPeriod: u32) -> u32;
    fn timeEndPeriod(uPeriod: u32) -> u32;
}

/// RAII guard for 1 ms Windows scheduler timer resolution during active high-refresh motion.
pub struct HighResTimer(bool);

impl HighResTimer {
    pub fn acquire() -> Self {
        Self(unsafe { timeBeginPeriod(1) == 0 })
    }
}

impl Drop for HighResTimer {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

pub fn max_monitor_refresh_hz() -> f64 {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, l: isize) -> i32 {
        unsafe {
            let best = &mut *(l as *mut f64);
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(m, &mut info.monitorInfo) != 0 {
                let mut mode = DEVMODEW {
                    dmSize: size_of::<DEVMODEW>() as u16,
                    ..Default::default()
                };
                if EnumDisplaySettingsW(info.szDevice.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) != 0
                    && let Some(hz) = planor::pacing::clamp_hz(mode.dmDisplayFrequency as f64)
                {
                    *best = (*best).max(hz);
                }
            }
            1
        }
    }
    let mut best = 0.0f64;
    unsafe { EnumDisplayMonitors(null_mut(), null(), Some(cb), &mut best as *mut f64 as isize) };
    if best > 0.0 {
        best
    } else {
        planor::pacing::DEFAULT_REFRESH_HZ
    }
}

pub fn dwm_refresh_period_us() -> Option<f64> {
    unsafe {
        let mut t = DWM_TIMING_INFO {
            cbSize: size_of::<DWM_TIMING_INFO>() as u32,
            ..Default::default()
        };
        if DwmGetCompositionTimingInfo(null_mut(), &mut t) < 0 {
            return None;
        }
        if t.qpcRefreshPeriod > 0 {
            let freq = crate::perf::qpc_frequency().max(1) as f64;
            let us = (t.qpcRefreshPeriod as f64 * 1_000_000.0) / freq;
            if us > 0.0 && planor::pacing::clamp_hz(1_000_000.0 / us).is_some() {
                return Some(us);
            }
        }
        if t.rateRefresh.uiDenominator != 0 {
            let hz = t.rateRefresh.uiNumerator as f64 / t.rateRefresh.uiDenominator as f64;
            if let Some(hz) = planor::pacing::clamp_hz(hz) {
                return Some(planor::pacing::period_us(hz));
            }
        }
        None
    }
}

/// Windows of these classes are never managed: sentinel windows of isolated test runs (when this is the user's
/// real daemon) and Planor's own settings window, which is a dialog and floats.
pub fn never_tiled_class(class: &str, real_daemon: bool) -> bool {
    class == "PlanorSettings" || (real_daemon && class.starts_with("PlanorSentinel"))
}

#[cfg(test)]
mod class_tests {
    use super::{is_overlay_occluder_class, never_tiled_class, place_batch_continue};
    use planor::Rect;

    #[test]
    fn settings_window_floats_and_sentinels_belong_to_their_test_daemon() {
        assert!(never_tiled_class("PlanorSettings", true));
        assert!(never_tiled_class("PlanorSettings", false));
        assert!(never_tiled_class("PlanorSentinel_p42", true));
        assert!(!never_tiled_class("PlanorSentinel_p42", false), "an isolated test daemon manages its own sentinels");
        assert!(!never_tiled_class("Notepad", true));
    }

    #[test]
    fn place_batch_continue_attempts_all_items_and_returns_first_error() {
        let items = vec![
            (1, Rect { x: 0, y: 0, w: 100, h: 100 }),
            (2, Rect { x: 100, y: 0, w: 100, h: 100 }),
            (3, Rect { x: 200, y: 0, w: 100, h: 100 }),
        ];
        let mut attempted = Vec::new();
        let res = place_batch_continue(&items, |id, _| {
            attempted.push(id);
            if id == 2 {
                Err("access denied on 2".into())
            } else {
                Ok(())
            }
        });
        assert_eq!(attempted, vec![1, 2, 3], "all items must be attempted after middle failure");
        assert_eq!(res.unwrap_err().to_string(), "access denied on 2");

        // All-succeed case returns Ok(()).
        let mut ok_attempted = Vec::new();
        let ok_res = place_batch_continue(&items, |id, _| {
            ok_attempted.push(id);
            Ok(())
        });
        assert!(ok_res.is_ok());
        assert_eq!(ok_attempted, vec![1, 2, 3]);
    }

    #[test]
    fn overlay_occluder_classes_cover_system_flyouts_and_planor_ui() {
        assert!(is_overlay_occluder_class("Windows.UI.Core.CoreWindow"));
        assert!(is_overlay_occluder_class("XamlExplorerHostIslandWindow"));
        assert!(is_overlay_occluder_class("TopLevelWindowForOverflowXamlIsland"));
        assert!(is_overlay_occluder_class("Planor.Note"));
        assert!(is_overlay_occluder_class("PlanorSettings"));
        assert!(is_overlay_occluder_class("Planor.Guide"));
        assert!(!is_overlay_occluder_class("Planor.Composition"));
        assert!(!is_overlay_occluder_class("Shell_TrayWnd"));
        assert!(!is_overlay_occluder_class("Progman"));
        assert!(!is_overlay_occluder_class("WorkerW"));
        assert!(!is_overlay_occluder_class("Notepad"));
        assert!(is_overlay_occluder_class("Planor.GestureGuide"), "the real gesture guide class");
    }

    #[test]
    fn topmost_occluders_are_floating_apps_not_shell_or_planor_or_click_through() {
        use super::topmost_overlay_occluder as occ;
        use windows_sys::Win32::UI::WindowsAndMessaging::{WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT};
        // Browser Picture-in-Picture and an ordinary always-on-top app (not filling a monitor).
        assert!(occ("Chrome_WidgetWin_1", WS_EX_TOPMOST | WS_EX_TOOLWINDOW, false));
        assert!(occ("MozillaDialogClass", WS_EX_TOPMOST, false));
        assert!(occ("Notepad", WS_EX_TOPMOST, false));
        // Not topmost: not this rule's business.
        assert!(!occ("Chrome_WidgetWin_1", WS_EX_TOOLWINDOW, false));
        // Shell surfaces (Shell_SecondaryTrayWnd really is WS_EX_TOPMOST | WS_EX_TOOLWINDOW).
        for shell in ["Shell_TrayWnd", "Shell_SecondaryTrayWnd", "Progman", "WorkerW"] {
            assert!(!occ(shell, WS_EX_TOPMOST | WS_EX_TOOLWINDOW, false), "{shell}");
        }
        // Planor's own overlay host: topmost, monitor-sized, layered, click-through. Holing it would
        // hide every border on the monitor.
        let host = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE;
        assert!(!occ("Planor.Composition", host, false));
        assert!(!occ("Planor.Composition", WS_EX_TOPMOST, false));
        assert!(!occ("PlanorOverlay", WS_EX_TOPMOST, false));
        // A click-through HUD (game/recording overlay) from another app.
        assert!(!occ("CEF-OSC-WIDGET", host, false));
        // (#102 review) Tooltips: transient hover popups, holing them flickers the overlay.
        assert!(!occ("tooltips_class32", WS_EX_TOPMOST | WS_EX_TOOLWINDOW, false));
        assert!(!occ("TOOLTIPS_CLASS32", WS_EX_TOPMOST, false), "class match is case-insensitive");
        // (#102 review) Monitor-filling topmost windows (borderless fullscreen games, the Snipping
        // Tool full-screen frame): holing them would blank the overlay for the whole monitor.
        assert!(!occ("Chrome_WidgetWin_1", WS_EX_TOPMOST, true), "fullscreen borderless game");
        assert!(!occ("Screen Sketch", WS_EX_TOPMOST | WS_EX_TOOLWINDOW, true), "Snipping Tool fullscreen");
        // A floating PiP that happens to be large but does not fill the monitor still holes.
        assert!(occ("Chrome_WidgetWin_1", WS_EX_TOPMOST | WS_EX_TOOLWINDOW, false));
    }
}

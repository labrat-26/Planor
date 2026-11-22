//! Notification-area icon owned by the daemon: Pause/Resume, Configure, Exit. Lives on its own thread with a
//! hidden window and drives the daemon through the same named pipe the command line uses, so it never touches
//! the engine and never blocks tiling.
use crate::{Command, Result, client, pipe_name, wide};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW},
        WindowsAndMessaging::*,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    TogglePause,
    Configure,
    Exit,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub id: u32,
    pub label: &'static str,
    /// `None` with an empty label is a separator.
    pub action: Option<TrayAction>,
}

/// The menu for the current state: Pause (or Resume), Configure, a separator, Exit.
pub fn menu_model(paused: bool) -> Vec<MenuItem> {
    vec![
        MenuItem { id: 1001, label: if paused { "Resume" } else { "Pause" }, action: Some(TrayAction::TogglePause) },
        MenuItem { id: 1002, label: "Configure...", action: Some(TrayAction::Configure) },
        MenuItem { id: 1000, label: "", action: None },
        MenuItem { id: 1003, label: "Exit", action: Some(TrayAction::Exit) },
    ]
}
pub fn tooltip(paused: bool) -> String {
    if paused { "Planor (paused)".into() } else { "Planor".into() }
}
/// `paused` out of `planor state --json`; anything unreadable counts as running.
pub fn paused_from_state_json(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json).ok().and_then(|v| v["paused"].as_bool()).unwrap_or(false)
}
/// Test daemons run beside the real one and must not grow a second icon.
pub fn should_start(test_mode: bool) -> bool {
    !test_mode
}

fn query_paused() -> bool {
    client(&pipe_name(), &Command::State { json: true }).map(|s| paused_from_state_json(&s)).unwrap_or(false)
}
pub fn run_action(action: TrayAction) -> Result<()> {
    match action {
        TrayAction::TogglePause => {
            let cmd = if query_paused() { Command::Resume } else { Command::Pause };
            client(&pipe_name(), &cmd).map(|_| ())
        }
        TrayAction::Configure => {
            use std::os::windows::process::CommandExt;
            std::process::Command::new(std::env::current_exe()?).arg("settings").creation_flags(0x0800_0000).spawn()?;
            Ok(())
        }
        TrayAction::Exit => client(&pipe_name(), &Command::Stop).map(|_| ()),
    }
}

const WM_TRAY: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;
static PAUSED: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = ICON_ID;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = crate::theme::app_icon(unsafe { GetSystemMetrics(SM_CXSMICON) });
    for (slot, c) in data.szTip.iter_mut().zip(tooltip(PAUSED.load(Ordering::Relaxed)).encode_utf16().take(127)) {
        *slot = c;
    }
    data
}
fn add_icon(hwnd: HWND) {
    let data = icon_data(hwnd);
    if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
        crate::diag::trace("tray: Shell_NotifyIcon add failed");
    }
}
fn update_tooltip(hwnd: HWND, paused: bool) {
    PAUSED.store(paused, Ordering::Relaxed);
    let data = icon_data(hwnd);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
}
fn remove_icon(hwnd: HWND) {
    let data = icon_data(hwnd);
    unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
}

fn show_menu(hwnd: HWND) {
    let paused = query_paused();
    update_tooltip(hwnd, paused);
    let model = menu_model(paused);
    unsafe {
        let menu = CreatePopupMenu();
        for item in &model {
            if item.action.is_none() {
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            } else {
                AppendMenuW(menu, MF_STRING, item.id as usize, wide(item.label).as_ptr());
            }
        }
        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        SetForegroundWindow(hwnd); // otherwise the menu does not close when clicking elsewhere
        let picked = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, at.x, at.y, 0, hwnd, std::ptr::null());
        DestroyMenu(menu);
        PostMessageW(hwnd, WM_NULL, 0, 0);
        if let Some(action) = model.iter().find(|i| i.id == picked as u32).and_then(|i| i.action) {
            act(hwnd, action);
        }
    }
}
fn act(hwnd: HWND, action: TrayAction) {
    if action == TrayAction::Exit {
        remove_icon(hwnd); // an icon left behind lingers until the mouse passes over it
    }
    if let Err(e) = run_action(action) {
        crate::diag::trace(format!("tray: {action:?} failed: {e}"));
    }
    if action == TrayAction::TogglePause {
        update_tooltip(hwnd, query_paused());
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAY => match lp as u32 {
                WM_LBUTTONUP | WM_RBUTTONUP => show_menu(hwnd),
                WM_LBUTTONDBLCLK => act(hwnd, TrayAction::Configure),
                _ => (),
            },
            WM_DESTROY => {
                remove_icon(hwnd);
                PostQuitMessage(0);
            }
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => add_icon(hwnd), // Explorer restarted
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
        0
    }
}

fn run() {
    crate::perf::register_thread("tray");
    crate::theme::enable_app_dark_mode();
    unsafe {
        TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Ordering::Relaxed);
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide("PlanorTray");
        let wc = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: instance, lpszClassName: class.as_ptr(), ..std::mem::zeroed() };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            wide("Planor").as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            crate::diag::trace("tray: cannot create the window");
            return;
        }
        add_icon(hwnd);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Start the tray icon on its own thread (not in test mode).
pub fn spawn() {
    if should_start(crate::test_mode()) {
        std::thread::spawn(run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_model_toggles_pause_label() {
        assert_eq!(menu_model(false)[0].label, "Pause");
        assert_eq!(menu_model(true)[0].label, "Resume");
        assert_eq!(menu_model(true)[0].action, Some(TrayAction::TogglePause));
    }

    #[test]
    fn menu_model_has_configure_and_exit_last() {
        let m = menu_model(false);
        let labels: Vec<_> = m.iter().map(|i| i.label).collect();
        assert_eq!(labels, ["Pause", "Configure...", "", "Exit"], "separator before Exit");
        assert_eq!(m.last().unwrap().action, Some(TrayAction::Exit));
        assert_eq!(m[1].action, Some(TrayAction::Configure));
        let mut ids: Vec<_> = m.iter().map(|i| i.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), m.len(), "menu ids are unique");
    }

    #[test]
    fn tooltip_reflects_pause() {
        assert_eq!(tooltip(false), "Planor");
        assert_eq!(tooltip(true), "Planor (paused)");
    }

    #[test]
    fn paused_from_state_json_reads_the_flag() {
        assert!(paused_from_state_json(r#"{"paused":true,"locked":false}"#));
        assert!(!paused_from_state_json(r#"{"paused":false}"#));
        assert!(!paused_from_state_json("not json"));
        assert!(!paused_from_state_json("{}"));
    }

    #[test]
    fn spawn_skipped_in_test_mode() {
        assert!(!should_start(true));
        assert!(should_start(false));
    }
}

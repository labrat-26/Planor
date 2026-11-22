//! Diagnostics note box: Win+W opens a small input window so what was just seen can be typed down.
//! The text goes into the trace log with the same timestamp clock as everything else, so a recording
//! or log can be lined up with the note ("portrait just went black", "window jumped").
use super::*;
use std::sync::atomic::{AtomicBool, Ordering::*};
use windows_sys::Win32::{
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};
static OPEN: AtomicBool = AtomicBool::new(false);
const SUBMIT: u32 = WM_APP + 1;
static OLD_EDIT: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static NOTE: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// Show the box where the cursor is, on top, and give it focus.
unsafe fn present(h: HWND) {
    unsafe {
        let mut at = POINT::default();
        GetCursorPos(&mut at);
        let monitor = MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as _, ..Default::default() };
        GetMonitorInfoW(monitor, &mut info);
        let mut r = RECT::default();
        GetWindowRect(h, &mut r);
        let (w, hgt) = (r.right - r.left, r.bottom - r.top);
        let x = info.rcMonitor.left + (info.rcMonitor.right - info.rcMonitor.left - w) / 2;
        let y = info.rcMonitor.top + (info.rcMonitor.bottom - info.rcMonitor.top - hgt) / 3;
        SetWindowPos(h, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_SHOWWINDOW);
        ShowWindow(h, SW_SHOW);
        win32::activate(h as isize);
        SetFocus(GetDlgItem(h, 1));
    }
}
/// Open the box on its own thread (the hook that calls this must return at once). If one is already
/// open, bring that one to the cursor instead of ignoring the key: a box that ended up hidden or off
/// screen used to make Win+W do nothing for the rest of the session.
pub fn open() {
    if OPEN.swap(true, AcqRel) {
        let h = NOTE.load(Acquire);
        if h != 0 && unsafe { IsWindow(h as _) } != 0 {
            unsafe { present(h as _) };
            diag::trace("NOTE box brought forward");
        } else {
            OPEN.store(false, Release);
        }
        return;
    }
    std::thread::spawn(|| {
        unsafe { run() };
        OPEN.store(false, Release);
    });
}
unsafe extern "system" fn edit_proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        if m == WM_KEYDOWN && w as u32 == VK_RETURN as u32 {
            PostMessageW(GetParent(h), SUBMIT, 0, 0);
            return 0;
        }
        if m == WM_KEYDOWN && w as u32 == VK_ESCAPE as u32 {
            PostMessageW(GetParent(h), WM_CLOSE, 0, 0);
            return 0;
        }
        // Swallow the beep a handled Enter/Esc would make.
        if m == WM_CHAR && (w == 13 || w == 27) {
            return 0;
        }
        CallWindowProcW(
            std::mem::transmute::<isize, WNDPROC>(OLD_EDIT.load(Acquire)),
            h,
            m,
            w,
            l,
        )
    }
}
unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        match m {
            SUBMIT => {
                let edit = GetDlgItem(h, 1);
                let mut buf = [0u16; 2048];
                let n = GetWindowTextW(edit, buf.as_mut_ptr(), 2048).max(0) as usize;
                let text = String::from_utf16_lossy(&buf[..n]);
                if !text.trim().is_empty() {
                    diag::trace(format!("NOTE {}", text.trim().replace('\n', " ")));
                }
                DestroyWindow(h);
                0
            }
            WM_CLOSE => {
                DestroyWindow(h);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            WM_SETFOCUS => {
                SetFocus(GetDlgItem(h, 1));
                0
            }
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}
unsafe fn run() {
    unsafe {
        let inst = GetModuleHandleW(null());
        let class = wide("Planor.Note");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: inst,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as _,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        });
        let mut at = POINT::default();
        GetCursorPos(&mut at);
        let monitor = MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as _,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut info);
        let dpi = GetDpiForSystem().max(96) as i32;
        let s = |v: i32| v * dpi / 96;
        let (w, hgt) = (s(560), s(112));
        let x = info.rcMonitor.left + (info.rcMonitor.right - info.rcMonitor.left - w) / 2;
        let y = info.rcMonitor.top + (info.rcMonitor.bottom - info.rcMonitor.top - hgt) / 3;
        let title = wide("Planor note");
        let h = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            x,
            y,
            w,
            hgt,
            null_mut(),
            null_mut(),
            inst,
            null(),
        );
        if h.is_null() {
            return;
        }
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let edit_class = wide("EDIT");
        let empty = wide("");
        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            edit_class.as_ptr(),
            empty.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            s(12),
            s(12),
            w - s(12) * 2 - s(16),
            s(26),
            h,
            1 as _,
            inst,
            null(),
        );
        let hint_class = wide("STATIC");
        let hint = wide("What did you see?   Enter = save to the trace log,  Esc = cancel");
        let label = CreateWindowExW(
            0,
            hint_class.as_ptr(),
            hint.as_ptr(),
            WS_CHILD | WS_VISIBLE,
            s(12),
            s(46),
            w - s(40),
            s(20),
            h,
            2 as _,
            inst,
            null(),
        );
        SendMessageW(edit, WM_SETFONT, font as usize, 1);
        SendMessageW(label, WM_SETFONT, font as usize, 1);
        OLD_EDIT.store(
            SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize),
            Release,
        );
        NOTE.store(h as isize, Release);
        present(h);
        diag::trace("NOTE box opened");
        let mut m = MSG::default();
        while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
            TranslateMessage(&m);
            DispatchMessageW(&m);
        }
        UnregisterClassW(class.as_ptr(), inst);
    }
}

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
    if crate::test_mode() {
        return;
    }
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
                let trimmed = text.trim().replace('\n', " ");
                if !trimmed.is_empty() {
                    diag::trace(format!("NOTE {trimmed}"));
                }
                DestroyWindow(h);
                if !trimmed.is_empty() && !crate::test_mode() {
                    std::thread::spawn(move || save_note_bundle(&trimmed));
                }
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

fn sanitize_slug(text: &str) -> String {
    let mut slug: String = text
        .chars()
        .take(36)
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "note".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Capture the virtual desktop as top-down RGBA pixels `(width, height, bytes)`.
pub fn capture_desktop_rgb() -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if vw <= 0 || vh <= 0 {
            return None;
        }
        let screen = GetDC(null_mut());
        if screen.is_null() {
            return None;
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, vw, vh);
        let mut rgba = None;
        if !mem.is_null() && !bmp.is_null() {
            let old = SelectObject(mem, bmp);
            if BitBlt(mem, 0, 0, vw, vh, screen, vx, vy, SRCCOPY) != 0 {
                let mut bi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: vw,
                        biHeight: -vh,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bgra = vec![0u8; (vw as usize) * (vh as usize) * 4];
                let rows = GetDIBits(
                    mem,
                    bmp,
                    0,
                    vh as u32,
                    bgra.as_mut_ptr().cast(),
                    &mut bi,
                    DIB_RGB_COLORS,
                );
                if rows > 0 {
                    for px in bgra.as_chunks_mut::<4>().0 {
                        px.swap(0, 2);
                        px[3] = 255;
                    }
                    rgba = Some((vw as u32, vh as u32, bgra));
                }
            }
            SelectObject(mem, old);
        }
        if !bmp.is_null() {
            DeleteObject(bmp);
        }
        if !mem.is_null() {
            DeleteDC(mem);
        }
        ReleaseDC(null_mut(), screen);
        rgba
    }
}

fn encode_png_rgba(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> io::Result<()> {
    let file = File::create(path)?;
    let mut encoder = png::Encoder::new(io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(io::Error::other)?;
    writer.write_image_data(rgba).map_err(io::Error::other)?;
    Ok(())
}

/// Write a self-contained diagnostic bundle (`note.txt`, `state.json`, `perf.json`,
/// `perf.trace.json`, `trace.tail.log`, and optional `screenshot.png`) under `base_dir/bundles/`.
pub fn write_bundle(
    base_dir: &std::path::Path,
    text: &str,
    state_json: &str,
    perf_json: &str,
    perf_trace_json: &str,
    screenshot_rgba: Option<(u32, u32, &[u8])>,
) -> io::Result<PathBuf> {
    let stamp = diag::stamp().replace([':', '.'], "");
    let bundle_dir = base_dir
        .join("bundles")
        .join(format!("{stamp}-{}", sanitize_slug(text)));
    fs::create_dir_all(&bundle_dir)?;
    fs::write(
        bundle_dir.join("note.txt"),
        format!("stamp={}\ntext={text}\nqpc_us={}\n", diag::stamp(), perf::qpc_us()),
    )?;
    fs::write(bundle_dir.join("state.json"), state_json)?;
    fs::write(bundle_dir.join("perf.json"), perf_json)?;
    fs::write(bundle_dir.join("perf.trace.json"), perf_trace_json)?;

    let trace_src = base_dir.join("trace.log");
    if let Ok(raw) = fs::read_to_string(&trace_src) {
        let lines: Vec<&str> = raw.lines().collect();
        // #56: 400 lines covered only ~4 s once touchpad PAD/SCENE lines flooded the trace, so the moment a note
        // describes ("clicking the bar made everything disappear") had already scrolled out. Keep 3000.
        let start = lines.len().saturating_sub(3000);
        fs::write(bundle_dir.join("trace.tail.log"), lines[start..].join("\n"))?;
    }

    if let Some((w, h, rgba)) = screenshot_rgba {
        encode_png_rgba(&bundle_dir.join("screenshot.png"), w, h, rgba)?;
    }
    Ok(bundle_dir)
}

fn save_note_bundle(text: &str) {
    // Allow DestroyWindow repaint to settle so the note input box does not occlude the desktop screenshot.
    std::thread::sleep(std::time::Duration::from_millis(80));
    let Ok(base_dir) = folder("LOCALAPPDATA") else {
        return;
    };
    let shot = capture_desktop_rgb();
    let state_json = client(&pipe_name(), &Command::State { json: true })
        .unwrap_or_else(|e| format!("{{\"error\":{e:?}}}"));
    let perf_json = serde_json::to_string_pretty(&perf::build_report(None))
        .unwrap_or_else(|_| "{}".to_string());
    let trace_json = serde_json::to_string(&perf::export_chrome_trace(Some(15)))
        .unwrap_or_else(|_| "{}".to_string());
    let shot_ref = shot.as_ref().map(|(w, h, b)| (*w, *h, b.as_slice()));
    let Ok(bundle_dir) = write_bundle(&base_dir, text, &state_json, &perf_json, &trace_json, shot_ref)
    else {
        return;
    };
    if let Some(rec) = watchdog::find_planor_rec() {
        let _ = std::process::Command::new(rec)
            .args(["dump", "--out"])
            .arg(bundle_dir.join("video"))
            .args(["--reason", "note", "--pipe", "ring"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    diag::trace(format!("NOTE_BUNDLE {}", bundle_dir.display()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_writes_note_state_perf_trace_and_png() {
        let dir = std::env::temp_dir().join(format!("planor-note-bundle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("trace.log"), "12:00:00.000 START test\n12:00:01.000 PLACE 1\n")
            .unwrap();

        let rgba = vec![0xffu8; 4 * 4 * 4];
        let bundle = write_bundle(
            &dir,
            "jumpy scroll right!!",
            "{\"schema_version\":1,\"tiles\":[]}",
            "{\"schema_version\":1,\"budgets\":{\"all_pass\":true}}",
            "{\"traceEvents\":[]}",
            Some((4, 4, &rgba)),
        )
        .unwrap();

        assert!(bundle.join("note.txt").is_file());
        assert!(fs::read_to_string(bundle.join("note.txt"))
            .unwrap()
            .contains("jumpy scroll right!!"));
        assert!(bundle.join("state.json").is_file());
        assert!(bundle.join("perf.json").is_file());
        assert!(bundle.join("perf.trace.json").is_file());
        let tail = fs::read_to_string(bundle.join("trace.tail.log")).unwrap();
        assert!(tail.contains("PLACE 1"));
        let png_bytes = fs::read(bundle.join("screenshot.png")).unwrap();
        assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        let _ = fs::remove_dir_all(&dir);
    }
}

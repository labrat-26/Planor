#![allow(dead_code, unused_imports)]
include!("common/helpers.rs");

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{KEYEVENTF_KEYUP, keybd_event};

const VK_LWIN_KEY: u8 = 0x5B;
const VK_ESCAPE_KEY: u8 = 0x1B;
const OVERLAY_CLASS: &str = "Planor.Composition";
/// Extended-style flags every Planor overlay host is created with (render.rs `Host::new`).
const OVERLAY_EX: [(u32, &str); 5] = [
    (WS_EX_LAYERED, "WS_EX_LAYERED"),
    // HTTRANSPARENT only passes clicks to windows of the same thread, so without this style the
    // topmost overlay swallowed clicks meant for explorer.exe's taskbar.
    (WS_EX_TRANSPARENT, "WS_EX_TRANSPARENT"),
    (WS_EX_TOOLWINDOW, "WS_EX_TOOLWINDOW"),
    (WS_EX_TOPMOST, "WS_EX_TOPMOST"),
    (WS_EX_NOACTIVATE, "WS_EX_NOACTIVATE"),
];

// ---- local helpers -----------------------------------------------------------------------------

fn rect_s(r: &RECT) -> String {
    format!("[l={} t={} r={} b={}]", r.left, r.top, r.right, r.bottom)
}

fn same_rect(a: &RECT, b: &RECT, tol: i32) -> bool {
    (a.left - b.left).abs() <= tol
        && (a.top - b.top).abs() <= tol
        && (a.right - b.right).abs() <= tol
        && (a.bottom - b.bottom).abs() <= tol
}

/// True when `inner` lies completely within `outer`.
fn inside(outer: &RECT, inner: &RECT) -> bool {
    inner.left >= outer.left
        && inner.top >= outer.top
        && inner.right <= outer.right
        && inner.bottom <= outer.bottom
}

fn pt_in(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn centre(r: &RECT) -> (i32, i32) {
    ((r.left + r.right) / 2, (r.top + r.bottom) / 2)
}

fn pid_of(h: HWND) -> u32 {
    let mut p = 0u32;
    unsafe { GetWindowThreadProcessId(h, &mut p) };
    p
}

fn ex_style(h: HWND) -> u32 {
    unsafe { GetWindowLongW(h, GWL_EXSTYLE) as u32 }
}

fn class_of(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

unsafe extern "system" fn collect_top_level(h: HWND, l: isize) -> i32 {
    unsafe { (*(l as *mut Vec<HWND>)).push(h) };
    1
}

fn all_top_level() -> Vec<HWND> {
    let mut v: Vec<HWND> = vec![];
    unsafe { EnumWindows(Some(collect_top_level), &mut v as *mut Vec<HWND> as isize) };
    v
}

/// Planor overlay hosts owned by the daemon process (recognised by class name and owner pid).
fn overlays_of(pid: u32) -> Vec<HWND> {
    all_top_level()
        .into_iter()
        .filter(|&h| pid_of(h) == pid && class_of(h) == OVERLAY_CLASS)
        .collect()
}

/// A Planor overlay owned by the daemon with all four overlay extended-style flags set.
fn is_overlay(h: HWND, pid: u32) -> bool {
    !h.is_null()
        && pid_of(h) == pid
        && OVERLAY_EX
            .iter()
            .all(|(flag, _)| (ex_style(h) & flag) == *flag)
}

/// Restores the real cursor position when dropped (probes move the cursor).
struct CursorRestore(POINT);
impl Drop for CursorRestore {
    fn drop(&mut self) {
        unsafe { SetCursorPos(self.0.x, self.0.y) };
    }
}
fn cursor_restore() -> CursorRestore {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p) };
    CursorRestore(p)
}

/// Hit-test the point. Planor's overlay answers WM_NCHITTEST from GetCursorPos, so the cursor
/// is moved onto the point first (exactly what a real click at that point would see). No click is sent.
fn probe(x: i32, y: i32) -> HWND {
    unsafe {
        SetCursorPos(x, y);
        WindowFromPoint(POINT { x, y })
    }
}

/// Top-level ancestor of a window (GA_ROOT), or the same null handle.
fn root_of(h: HWND) -> HWND {
    unsafe { GetAncestor(h, GA_ROOT) }
}

/// Taskbar strips of one monitor: the part of the monitor rect that the work area excludes.
fn taskbar_strips(mon: &(RECT, RECT)) -> Vec<(&'static str, RECT)> {
    let (m, w) = (&mon.0, &mon.1);
    let mut v = vec![];
    if w.bottom < m.bottom {
        v.push((
            "bottom",
            RECT {
                left: m.left,
                top: w.bottom,
                right: m.right,
                bottom: m.bottom,
            },
        ));
    }
    if w.top > m.top {
        v.push((
            "top",
            RECT {
                left: m.left,
                top: m.top,
                right: m.right,
                bottom: w.top,
            },
        ));
    }
    if w.left > m.left {
        v.push((
            "left",
            RECT {
                left: m.left,
                top: m.top,
                right: w.left,
                bottom: m.bottom,
            },
        ));
    }
    if w.right < m.right {
        v.push((
            "right",
            RECT {
                left: w.right,
                top: m.top,
                right: m.right,
                bottom: m.bottom,
            },
        ));
    }
    v
}

/// Every visible frame must lie inside the work area of each monitor it touches, i.e. it must not
/// extend vertically past the work area and must not cover any part of the taskbar strip.
/// Horizontal scroll clipping at the monitor edge is legitimate and not treated as a violation.
fn check_inside_work_area(ws: &[Sentinel], mons_v: &[(RECT, RECT)], label: &str) {
    for (i, w) in ws.iter().enumerate() {
        let v = w.visible();
        for (mi, (mon, work)) in mons_v.iter().enumerate() {
            if overlap(&v, mon) == 0 {
                continue;
            }
            assert!(
                v.top >= work.top && v.bottom <= work.bottom,
                "{label}: window {i} vertical extent expected inside work area {} of monitor {mi}, observed frame {}",
                rect_s(work),
                rect_s(&v)
            );
            assert!(
                overlap(&v, mon) == overlap(&v, work),
                "{label}: window {i} expected to avoid the taskbar strip of monitor {mi} (monitor {}, work area {}), observed frame {} covering {} px of the strip",
                rect_s(mon),
                rect_s(work),
                rect_s(&v),
                overlap(&v, mon) - overlap(&v, work)
            );
        }
    }
}

/// Every sentinel frame must match its recorded frame within 2 px.
fn assert_frames_unchanged(ws: &[Sentinel], before: &[RECT], when: &str) {
    let mut bad = vec![];
    for (i, (w, b)) in ws.iter().zip(before).enumerate() {
        let now = w.visible();
        if !same_rect(&now, b, 2) {
            bad.push(format!(
                "window {i}: expected frame {} unchanged (+-2px), observed {}",
                rect_s(b),
                rect_s(&now)
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{when}: sentinel frames moved: {}",
        bad.join("; ")
    );
}

/// Releases the Escape key on drop so the Start menu is always closed, even on panic.
struct StartMenuGuard;
impl Drop for StartMenuGuard {
    fn drop(&mut self) {
        unsafe {
            keybd_event(VK_ESCAPE_KEY, 0, 0, 0);
            keybd_event(VK_ESCAPE_KEY, 0, KEYEVENTF_KEYUP, 0);
        }
        thread::sleep(Duration::from_millis(400));
    }
}

fn tap_win_key() {
    unsafe {
        keybd_event(VK_LWIN_KEY, 0, 0, 0);
        keybd_event(VK_LWIN_KEY, 0, KEYEVENTF_KEYUP, 0);
    }
}

// ---- S6 tests ----------------------------------------------------------------------------------

#[test]
#[ignore]
fn tiles_stay_inside_work_area() {
    aware();
    let mons_v = mons();
    // With PLANOR_RECORD=1 the scenario is screen-recorded and the recording is checked against the pixels;
    // otherwise `rec` is inert and nothing changes.
    let rec = Recording::start("tiles_stay_inside_work_area", "s6-work");
    let ws = [
        Sentinel::new(0, 0),
        Sentinel::new(1, 0),
        Sentinel::new(2, 0),
    ];
    let mut d = Daemon::new("s6-work");
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "taskbar daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    check_inside_work_area(&ws, &mons_v, "after settle");
    for step in 1..=2 {
        press(&[ALT], RIGHT);
        rec.mark(&format!("Alt+Right step {step}"));
        check_inside_work_area(&ws, &mons_v, &format!("after Alt+Right step {step}"));
    }
    for step in 1..=2 {
        press(&[ALT], LEFT);
        rec.mark(&format!("Alt+Left step {step}"));
        check_inside_work_area(&ws, &mons_v, &format!("after Alt+Left step {step}"));
    }
    assert_recording_clean(&rec);
}

#[test]
#[ignore]
fn taskbar_hit_test_passes_through() {
    aware();
    let _ws = [Sentinel::new(0, 0), Sentinel::new(1, 0)];
    let mut d = Daemon::new("s6-hit");
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "taskbar daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    let pid = d.p.as_ref().unwrap().id();
    let _cursor = cursor_restore();

    let mons_v = mons();
    let mut probes: Vec<(usize, &'static str, RECT)> = vec![];
    for (mi, m) in mons_v.iter().enumerate() {
        for (side, r) in taskbar_strips(m) {
            probes.push((mi, side, r));
        }
    }
    if probes.is_empty() {
        println!("SKIPPED_NO_TASKBAR_STRIP: every monitor's work area equals its monitor rect");
        return;
    }

    let fg_before = fg();
    for (mi, side, r) in &probes {
        let (x, y) = centre(r);
        let root = root_of(probe(x, y));
        let cls = class_of(root);
        assert!(
            !root.is_null()
                && (cls == "Shell_TrayWnd" || cls == "Shell_SecondaryTrayWnd")
                && !is_overlay(root, pid),
            "monitor {mi} {side} taskbar strip centre ({x},{y}) in strip {}: expected WindowFromPoint root to be Shell_TrayWnd or Shell_SecondaryTrayWnd (not a Planor overlay), observed root hwnd {:#x} class {cls:?} planor_overlay={}",
            rect_s(r),
            root as isize,
            is_overlay(root, pid)
        );
    }

    let overlays = overlays_of(pid);
    assert!(
        !overlays.is_empty(),
        "expected at least one Planor overlay (class {OVERLAY_CLASS}) owned by daemon pid {pid}, found none; taskbar pass-through cannot be verified"
    );
    for h in &overlays {
        let ex = ex_style(*h);
        let missing: Vec<&str> = OVERLAY_EX
            .iter()
            .filter(|(flag, _)| (ex & flag) == 0)
            .map(|(_, name)| *name)
            .collect();
        assert!(
            missing.is_empty(),
            "overlay hwnd {:#x} (pid {pid}): expected extended style WS_EX_LAYERED|WS_EX_TRANSPARENT|WS_EX_TOOLWINDOW|WS_EX_TOPMOST|WS_EX_NOACTIVATE, observed missing {:?}",
            *h as isize,
            missing
        );
    }

    let fg_after = fg();
    assert_eq!(
        fg_after, fg_before,
        "hit-testing the taskbar must not change the foreground window: expected {fg_before:#x}, observed {fg_after:#x}"
    );
}

fn click_at(x: i32, y: i32) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, mouse_event,
    };
    unsafe {
        SetCursorPos(x, y);
        thread::sleep(Duration::from_millis(250));
        mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
        thread::sleep(Duration::from_millis(60));
        mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
    }
}

/// A real mouse click on a peek sliver must reveal and focus that window. The overlay is fully
/// click-through (WS_EX_TRANSPARENT), so this is done by Planor's low-level mouse hook. Real input is
/// used on purpose: WindowFromPoint skips transparent windows and cannot tell the two designs apart.
#[test]
#[ignore]
fn clicking_a_peek_reveals_that_window() {
    aware();
    let ws = [
        Sentinel::new(0, 0),
        Sentinel::new(1, 0),
        Sentinel::new(2, 0),
        Sentinel::new(3, 0),
    ];
    let mut d = Daemon::new_with_hooks("s6-peek");
    d.start();
    thread::sleep(Duration::from_millis(1800));
    let _cursor = cursor_restore();
    let mons_v = mons();
    let (_, work) = &mons_v[0];
    let y = (work.top + work.bottom) / 2;

    // Walk the focus along the strip until a stretch of the work area is not covered by any real
    // window. Whatever is drawn there is a peek (a DWM thumbnail of a parked window), never a real
    // window, so a click there can only focus something through Planor's hook.
    let mut log = vec![];
    let mut revealed = false;
    for step in 0..8 {
        thread::sleep(Duration::from_millis(900));
        let mut covered: Vec<(i32, i32)> = ws
            .iter()
            .filter_map(|w| {
                let v = w.visible();
                (v.right > work.left
                    && v.left < work.right
                    && v.bottom > work.top
                    && v.top < work.bottom)
                    .then_some((v.left.max(work.left), v.right.min(work.right)))
            })
            .collect();
        covered.sort();
        let mut gaps = vec![];
        let mut cursor = work.left;
        for (l, r) in covered {
            if l - cursor >= 16 {
                gaps.push((cursor, l));
            }
            cursor = cursor.max(r);
        }
        if work.right - cursor >= 16 {
            gaps.push((cursor, work.right));
        }
        if let Some(&(a, b)) = gaps.first() {
            let x = (a + b) / 2;
            let before = fg_index(&ws);
            click_at(x, y);
            thread::sleep(Duration::from_millis(1200));
            let after = fg_index(&ws);
            log.push(format!("step {step}: click in uncovered gap {a}..{b} at ({x},{y}): focus {before:?} -> {after:?}"));
            if let Some(i) = after.filter(|i| Some(*i) != before) {
                assert!(
                    reachable(&ws[i]),
                    "clicked peek at ({x},{y}) focused sentinel {i} but it is not on screen: rect {:?}",
                    ws[i].rect()
                );
                revealed = true;
            }
            break;
        }
        log.push(format!("step {step}: no uncovered stretch"));
        press(&[ALT], if step < 3 { RIGHT } else { LEFT });
    }
    assert!(
        revealed,
        "expected a real click in a peek (gap beside the focused window) in primary work area {} to focus a different sentinel; observed: {}",
        rect_s(work),
        log.join("; ")
    );
}

#[test]
#[ignore]
fn overlay_click_through_everywhere_but_peek() {
    aware();
    let ws = [
        Sentinel::new(0, 0),
        Sentinel::new(1, 0),
        Sentinel::new(2, 0),
    ];
    let mut d = Daemon::new("s6-clk");
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "taskbar daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    let pid = d.p.as_ref().unwrap().id();
    let _cursor = cursor_restore();

    let mons_v = mons();
    let prim = &mons_v[0].0;
    let dpi = unsafe { GetDpiForWindow(ws[0].hwnd as HWND) } as i32;
    let peek_w = 48 * dpi / 96;
    let span_x = prim.right - prim.left - 1;
    let span_y = prim.bottom - prim.top - 1;

    let mut bad: Vec<String> = vec![];
    let mut probes = 0;
    for j in 0..9 {
        for i in 0..9 {
            let x = prim.left + span_x * i / 8;
            let y = prim.top + span_y * j / 8;
            probes += 1;
            let hit = probe(x, y);
            if !is_overlay(hit, pid) {
                continue;
            }
            let on_peek_edge = x < prim.left + peek_w || x >= prim.right - peek_w;
            let on_clipped_tile = ws.iter().any(|t| {
                let v = t.visible();
                pt_in(&v, x, y) && !inside(prim, &v)
            });
            if !on_peek_edge && !on_clipped_tile {
                bad.push(format!(
                    "grid point ({i},{j}) = ({x},{y}) returned overlay hwnd {:#x}",
                    hit as isize
                ));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "expected WindowFromPoint never to return a Planor overlay except inside a peek sliver (edge strip {peek_w}px wide or a clipped tile), observed {} of {probes} grid points on the primary monitor {} hitting an overlay: {}",
        bad.len(),
        rect_s(prim),
        bad.join("; ")
    );
}

#[test]
#[ignore]
fn fullscreen_covers_taskbar_and_is_scrollable_out() {
    aware();
    let ws = [
        Sentinel::new(0, 0),
        Sentinel::new(1, 0),
        Sentinel::new(2, 0),
    ];
    let mut d = Daemon::new("s6-full");
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "taskbar daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    let _cursor = cursor_restore();

    let mons_v = mons();
    let mon = mons_v[0].0;
    let strips = taskbar_strips(&mons_v[0]);
    if strips.is_empty() {
        println!("SKIPPED_NO_TASKBAR_STRIP: primary work area equals its monitor rect");
        return;
    }

    // Focus can be stolen by other windows, so retry: Alt tap, then SetForegroundWindow, up to 4 times.
    let h0 = ws[0].hwnd as HWND;
    let mut focused = false;
    let mut last_fg: isize = 0;
    for attempt in 0..4 {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(400));
        }
        press(&[], ALT);
        unsafe { SetForegroundWindow(h0) };
        thread::sleep(Duration::from_millis(300));
        last_fg = fg();
        if last_fg == ws[0].hwnd {
            focused = true;
            break;
        }
    }
    assert!(
        focused,
        "precondition: expected window 0 (hwnd {:#x}) to take focus before going fullscreen (4 attempts of Alt tap + SetForegroundWindow, 400 ms apart), observed foreground hwnd {:#x} class {:?} process id {}",
        ws[0].hwnd,
        last_fg,
        class_of(last_fg as HWND),
        pid_of(last_fg as HWND)
    );

    unsafe {
        SetWindowLongPtrW(h0, GWL_STYLE, (WS_POPUP | WS_VISIBLE) as isize);
        SetWindowPos(
            h0,
            null_mut(),
            mon.left,
            mon.top,
            mon.right - mon.left,
            mon.bottom - mon.top,
            SWP_FRAMECHANGED | SWP_SHOWWINDOW | SWP_NOZORDER,
        );
    }
    thread::sleep(Duration::from_millis(1500));

    let v0 = ws[0].visible();
    assert!(
        same_rect(&v0, &mon, 2),
        "window 0 fullscreen: expected visible frame == monitor rect {} (covering the taskbar), observed {}",
        rect_s(&mon),
        rect_s(&v0)
    );
    for i in 1..3 {
        assert!(
            !reachable(&ws[i]),
            "window {i} while window 0 is fullscreen: expected it scrolled out of view (window 0 sits at the strip end), observed still reachable at {}",
            rect_s(&ws[i].visible())
        );
    }

    press(&[ALT], LEFT);
    assert_eq!(
        fg_index(&ws),
        Some(2),
        "after Alt+Left from fullscreen window 0 at the strip end: expected focus on window 2 (the previous tile), observed fg index {:?} (foreground hwnd {:#x})",
        fg_index(&ws),
        fg()
    );
    assert!(
        reachable(&ws[2]),
        "window 2 after Alt+Left: expected reachable on screen, observed frame {}",
        rect_s(&ws[2].visible())
    );
    let v0_after = ws[0].visible();
    assert!(
        !same_rect(&v0_after, &mon, 2),
        "after Alt+Left: expected window 0 no longer fullscreen over the taskbar (frame != monitor rect {}), observed frame {}",
        rect_s(&mon),
        rect_s(&v0_after)
    );
    let (x, y) = centre(&strips[0].1);
    let root = root_of(probe(x, y));
    let cls = class_of(root);
    assert!(
        cls == "Shell_TrayWnd" || cls == "Shell_SecondaryTrayWnd",
        "after Alt+Left: expected the taskbar visible at strip centre ({x},{y}) of strip {} ({}), observed WindowFromPoint root hwnd {:#x} class {cls:?}",
        strips[0].0,
        rect_s(&strips[0].1),
        root as isize
    );
}

#[test]
#[ignore]
fn start_menu_open_keeps_layout() {
    aware();
    let ws = [
        Sentinel::new(0, 0),
        Sentinel::new(1, 0),
        Sentinel::new(2, 0),
    ];
    let mut d = Daemon::new("s6-start");
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "taskbar daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    let before: Vec<RECT> = ws.iter().map(|w| w.visible()).collect();

    let _esc = StartMenuGuard;
    tap_win_key();
    thread::sleep(Duration::from_millis(1500));
    assert_frames_unchanged(&ws, &before, "after Win key (Start menu opened)");

    tap_win_key();
    thread::sleep(Duration::from_millis(1500));
    assert_frames_unchanged(&ws, &before, "after second Win key (Start menu closed)");
}

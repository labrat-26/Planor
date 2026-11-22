//! `planor gestures guide`: a window that stays on top and walks the owner through every touchpad gesture
//! (1 to 5 fingers, taps, physical clicks, every direction) while the daemon records them. Each step is
//! labelled in the recording with a `mark` line, so the tests can tell which gesture a frame belongs to.
//! By default the daemon only records (Planor does not act), so windows do not fly around meanwhile.
use super::*;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows_sys::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{HiDpi::GetDpiForSystem, WindowsAndMessaging::*},
};

/// (what to do, how) — read by the owner from the window.
pub const STEPS: &[(&str, &str)] = &[
    ("ONE finger: move the cursor around", "Slide one finger over the touchpad in circles and lines."),
    ("ONE finger: TAP 3 times", "Tap-to-click, on this window."),
    ("PRESS the pad: LEFT click 3 times", "Physically press the touchpad down, on this window."),
    ("TWO fingers: TAP 3 times", "Two-finger tap (right click), on this window."),
    ("PRESS bottom-right corner: RIGHT click 3 times", "Physical right click, on this window."),
    ("TWO fingers: scroll UP and DOWN", "A few times each way."),
    ("TWO fingers: scroll LEFT and RIGHT", "A few times each way."),
    ("TWO fingers: PINCH in and out", "Zoom gesture, a few times."),
    ("THREE fingers: TAP 3 times", "Short taps, no movement."),
    ("THREE fingers: swipe LEFT, 3 times", "Normal speed, lift between swipes."),
    ("THREE fingers: swipe RIGHT, 3 times", "Normal speed, lift between swipes."),
    ("THREE fingers: swipe UP, 3 times", "Normal speed, lift between swipes."),
    ("THREE fingers: swipe DOWN, 3 times", "Normal speed, lift between swipes."),
    ("THREE fingers: swipe LEFT very SLOWLY, once", "Take the whole time."),
    ("THREE fingers: FLICK right fast, 3 times", "Quick flicks."),
    ("THREE fingers: DIAGONAL swipes", "Any diagonal direction, a few times."),
    ("FOUR fingers: TAP 3 times", "Short taps, no movement."),
    ("FOUR fingers: drag LEFT, 3 times", "Lift between drags."),
    ("FOUR fingers: drag RIGHT, 3 times", "Lift between drags."),
    ("FOUR fingers: drag UP, 3 times", "Lift between drags."),
    ("FOUR fingers: drag DOWN, 3 times", "Lift between drags."),
    ("FOUR fingers: drag in a CIRCLE, then lift", "One slow circle."),
    ("FOUR fingers: drag, then lift ONE finger at a time", "Twice."),
    ("FIVE fingers: tap and swipe", "Anything with the whole hand."),
];

/// `--set topup`: what the first full session was missing. Planor acts (drops only mean something live).
pub const TOPUP: &[(&str, &str)] = &[
    ("ONE finger: TAP 3 times (tap to click)", "One finger only, on this window."),
    ("THREE fingers: swipe LEFT very SLOWLY, once", "Fingers moving to the LEFT, take the whole time."),
    ("FOUR fingers: drag a window onto the CENTRE of another, lift", "Lift inside the filled box in the middle of the other window."),
    ("FOUR fingers: drag a window onto the EDGE of another, lift", "Lift on the other window but away from the filled box."),
    ("FOUR fingers: FLICK a window fast towards another screen", "A quick throw, then lift."),
    ("FOUR fingers: drag, then lift ONE finger at a time", "Twice: drag, then raise your fingers one by one."),
    ("THREE fingers: swipe UP and DOWN on the group you made", "Fingers across the strip roll the group."),
    ("MOUSE: hold WIN + turn the wheel, cursor on the TOP screen", "Keep the Windows key down while turning the mouse wheel."),
    ("MOUSE: hold WIN + turn the wheel, cursor on the PORTRAIT screen", "Move the cursor to the tall screen first."),
    ("MOUSE: hold WIN + turn the wheel, cursor on the LAPTOP screen", "Move the cursor to the laptop screen first."),
];

/// `--set group`: grouping needs time to carry a window onto the small box, so these steps are long (20 s).
pub const GROUP: &[(&str, &str)] = &[
    ("FOUR fingers: drag a window onto the CENTRE of another, lift", "Aim for the filled box in the middle of the other window; lift inside it. Try until they group."),
    ("THREE fingers: swipe UP and DOWN on the group", "Three fingers, up and down a few times: the group rolls."),
    ("THREE fingers: swipe LEFT very SLOWLY, once", "Fingers moving to the LEFT, take your time."),
    ("FOUR fingers: drag a window onto the CENTRE of another, lift", "Once more, on another pair of windows."),
];

const READY: Duration = Duration::from_secs(2);

#[derive(Clone)]
enum Phase {
    Ready(usize, Instant),
    Go(usize, Instant),
    Done(String),
}

struct State {
    phase: Phase,
    step: Duration,
    out: PathBuf,
    steps: &'static [(&'static str, &'static str)],
    finished_at: Option<Instant>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn ipc(action: GestureAction) -> Result<String> {
    client(&pipe_name(), &Command::Gestures { action })
}

/// Advance the session; returns false once the window should close.
fn tick() -> bool {
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else { return false };
    let now = Instant::now();
    match s.phase.clone() {
        Phase::Ready(i, at) if now.duration_since(at) >= READY => {
            let _ = ipc(GestureAction::Mark { text: format!("step {}: {}", i + 1, s.steps[i].0) });
            s.phase = Phase::Go(i, now);
        }
        Phase::Go(i, at) if now.duration_since(at) >= s.step => {
            let _ = ipc(GestureAction::Mark { text: format!("end step {}", i + 1) });
            if i + 1 < s.steps.len() {
                s.phase = Phase::Ready(i + 1, now);
            } else {
                let reply = ipc(GestureAction::Stop).unwrap_or_else(|e| format!("stop failed: {e}"));
                s.phase = Phase::Done(format!("{reply}\n{}", s.out.display()));
                s.finished_at = Some(now);
            }
        }
        Phase::Done(_) if s.finished_at.is_some_and(|t| now.duration_since(t) > Duration::from_secs(6)) => return false,
        _ => (),
    }
    true
}

/// (headline, detail, countdown line, background colour as BGR)
fn screen() -> (String, String, String, u32) {
    let g = STATE.lock().unwrap();
    let Some(s) = g.as_ref() else { return Default::default() };
    let now = Instant::now();
    let n = s.steps.len();
    let steps = s.steps;
    match &s.phase {
        Phase::Ready(i, at) => {
            let left = READY.saturating_sub(now.duration_since(*at)).as_secs_f32().ceil();
            (
                format!("Next ({}/{n}): {}", i + 1, steps[*i].0),
                steps[*i].1.to_string(),
                format!("Get ready... starts in {left:.0}"),
                0x0040_6080, // amber-ish dark
            )
        }
        Phase::Go(i, at) => {
            let left = s.step.saturating_sub(now.duration_since(*at)).as_secs_f32().ceil();
            (
                format!("{}/{n}: {}", i + 1, steps[*i].0),
                steps[*i].1.to_string(),
                format!("GO  -  {left:.0} s"),
                0x0030_7020, // green
            )
        }
        Phase::Done(msg) => ("Done, thank you!".into(), msg.clone(), "This window closes by itself.".into(), 0x0050_3020),
    }
}

unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        match m {
            WM_TIMER => {
                if tick() {
                    InvalidateRect(h, std::ptr::null(), 1);
                } else {
                    DestroyWindow(h);
                }
                0
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(h, &mut ps);
                let mut rc = RECT::default();
                GetClientRect(h, &mut rc);
                let (head, detail, count, bg) = screen();
                let brush = CreateSolidBrush(bg);
                FillRect(dc, &rc, brush);
                DeleteObject(brush as _);
                SetBkMode(dc, TRANSPARENT as _);
                SetTextColor(dc, 0x00FF_FFFF);
                let dpi = GetDpiForSystem().max(96) as i32;
                let font = |px: i32, bold: bool| {
                    let face = wide("Segoe UI");
                    CreateFontW(-px * dpi / 96, 0, 0, 0, if bold { 700 } else { 400 }, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr())
                };
                let pad = 16 * dpi / 96;
                let mut y = pad;
                for (text, px, bold) in [(head, 26, true), (detail, 18, false), (count, 30, true)] {
                    let f = font(px, bold);
                    let old = SelectObject(dc, f as _);
                    let t = wide(&text);
                    let mut r = RECT { left: pad, top: y, right: rc.right - pad, bottom: rc.bottom };
                    let used = DrawTextW(dc, t.as_ptr(), -1, &mut r, DT_WORDBREAK | DT_NOPREFIX);
                    y += used + pad / 2;
                    SelectObject(dc, old);
                    DeleteObject(f as _);
                }
                EndPaint(h, &ps);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}

pub fn run(name: String, step_seconds: u64, live: bool, set: String) -> Result<()> {
    let (steps, live, step_seconds): (&'static [(&'static str, &'static str)], bool, u64) = match set.as_str() {
        "topup" => (TOPUP, true, step_seconds.max(8)),
        "group" => (GROUP, true, step_seconds.max(20)),
        _ => (STEPS, live, step_seconds),
    };
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let stamp = diag::stamp().replace([':', '.'], "");
    let out = folder("LOCALAPPDATA")?.join("gestures").join(format!("{stamp}-{slug}.jsonl"));
    fs::create_dir_all(out.parent().unwrap())?;
    let step = Duration::from_secs(step_seconds.max(2));
    let total = steps.len() as u64 * (step.as_secs() + READY.as_secs()) + 30;
    let reply = ipc(GestureAction::Record { name: slug, seconds: total, out: Some(out.clone()), passive: !live })
        .map_err(|e| format!("cannot reach the Planor daemon (is planor.exe running?): {e}"))?;
    println!("{reply}");
    println!("{} steps, about {} s, Planor {}. Follow the window on top.", steps.len(), steps.len() as u64 * (step.as_secs() + 2), if live { "live" } else { "recording only" });
    *STATE.lock().unwrap() = Some(State { phase: Phase::Ready(0, Instant::now()), step, out: out.clone(), steps, finished_at: None });
    unsafe {
        let inst = GetModuleHandleW(std::ptr::null());
        let class = wide("Planor.GestureGuide");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: inst,
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..Default::default()
        });
        let dpi = GetDpiForSystem().max(96) as i32;
        let (w, hgt) = (760 * dpi / 96, 230 * dpi / 96);
        let mut at = windows_sys::Win32::Foundation::POINT::default();
        GetCursorPos(&mut at);
        let mon = MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as _, ..Default::default() };
        GetMonitorInfoW(mon, &mut info);
        let x = info.rcWork.left + (info.rcWork.right - info.rcWork.left - w) / 2;
        let y = info.rcWork.top + 24 * dpi / 96;
        let title = wide("Planor gesture recording");
        // On top, never activated (gestures and clicks on it must not steal focus), and a tool window so
        // Planor does not tile it.
        let h = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            x,
            y,
            w,
            hgt,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            inst,
            std::ptr::null(),
        );
        if h.is_null() {
            let _ = ipc(GestureAction::Stop);
            return Err("could not create the guide window".into());
        }
        SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        SetTimer(h, 1, 100, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // Closed early with the X: finish the recording with what was done so far.
    let done = matches!(STATE.lock().unwrap().as_ref().map(|s| &s.phase), Some(Phase::Done(_)));
    if !done {
        println!("{}", ipc(GestureAction::Stop).unwrap_or_else(|e| format!("stop failed: {e}")));
    }
    std::thread::sleep(Duration::from_millis(300));
    match fs::read_to_string(&out) {
        Ok(text) => {
            let marks = text.lines().filter(|l| l.contains("\"kind\":\"mark\"") && l.contains("step ") && !l.contains("end step")).count();
            let frames = text.lines().filter(|l| l.contains("\"kind\":\"contacts\"")).count();
            let clicks = text.lines().filter(|l| l.contains("\"kind\":\"button\"") && l.contains("\"down\":true")).count();
            println!("{frames} touch frames, {clicks} clicks, {marks}/{} steps -> {}", steps.len(), out.display());
            println!("Give this path to Claude to add it under tests/fixtures/gestures/recorded/.");
        }
        Err(e) => println!("no recording at {}: {e}", out.display()),
    }
    Ok(())
}

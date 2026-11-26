#![allow(dead_code, unused_imports)]
include!("common/helpers.rs");

// S2 from docs/TEST_PLAN.md: size and position fidelity while scrolling.
// Every test asserts the behaviour the spec requires; a red test with a precise message is a real bug.
//
// Colours: helpers.rs passes COLORREF values (0x00BBGGRR) to CreateSolidBrush, so the RGB seen on screen
// for sentinel i is SENTINEL_RGB[i] (magenta, cyan, yellow). Boxes are indexed the same way.

/// Sentinels per test. Pixel colours collide at 4 (SENTINEL_RGB has 3 entries).
const N: usize = 3;
/// Tolerance in px for every geometry comparison (spec).
const TOL: i32 = 2;
/// Tolerance in px for the peek sliver width measured from pixel boxes (spec).
const PEEK_TOL: i32 = 3;
/// Width of the off-screen peek sliver in px (spec `peek_width_px`).
const PEEK_PX: i32 = 48;
/// gap_px from the config (planor.example.toml), in raw device pixels: the product scales only border width and
/// radius by DPI, not the gap.
const GAP_PX: i32 = 6;
/// Tolerance in px for the gap between adjacent tiles.
const GAP_TOL: i32 = 1;

/// (left, top, right, bottom) in physical pixels.
type Rect4 = (i32, i32, i32, i32);

fn rect4(r: RECT) -> Rect4 {
    (r.left, r.top, r.right, r.bottom)
}
/// Visible (DWM extended frame) bounds of a sentinel in desktop coordinates.
/// Only meaningful for a sentinel that is reachable(); parked windows report their park rect.
fn vis(w: &Sentinel) -> Rect4 {
    rect4(w.visible())
}
fn width(r: Rect4) -> i32 {
    r.2 - r.0
}
fn height(r: Rect4) -> i32 {
    r.3 - r.1
}
/// Colour string of sentinel `i` for messages (the on-screen RGB).
fn colour(i: usize) -> String {
    format!("0x{:06x}", SENTINEL_RGB[i])
}
/// Colour box of sentinel `i` out of a frame's box array.
fn box_of(b: &[BBox; N], i: usize) -> BBox {
    b[i]
}
fn fmt_box(b: BBox) -> String {
    match b {
        None => "absent".into(),
        Some(r) => format!("(l={} t={} r={} b={}, {}x{})", r.0, r.1, r.2, r.3, r.2 - r.0, r.3 - r.1),
    }
}
fn fmt_boxes(b: &[BBox; N]) -> String {
    (0..N)
        .map(|i| format!("sentinel {i} {}: {}", colour(i), fmt_box(box_of(b, i))))
        .collect::<Vec<_>>()
        .join("; ")
}
/// True if a colour box is clipped by the left or right output edge.
fn touches_edge(b: (i32, i32, i32, i32), out_w: i32) -> bool {
    b.0 <= 0 || b.2 >= out_w
}
/// Window handles in sentinel order.
fn hwnds(ws: &[Sentinel]) -> [isize; N] {
    std::array::from_fn(|i| ws[i].hwnd)
}
/// Index of the sentinel that currently has focus, None when another window (or none) does.
fn fg_slot(hw: &[isize; N]) -> Option<usize> {
    let f = fg();
    hw.iter().position(|&h| h == f)
}
/// True if the box size and vertical position match the reference within TOL.
fn size_ok(b: Rect4, r: Rect4) -> bool {
    (width(b) - width(r)).abs() <= TOL
        && (height(b) - height(r)).abs() <= TOL
        && (b.1 - r.1).abs() <= TOL
        && (b.3 - r.3).abs() <= TOL
}

/// (monitor, work area) of the primary monitor, physical pixels. mons() lists the primary first.
fn primary() -> (Rect4, Rect4) {
    let m = mons();
    (rect4(m[0].0), rect4(m[0].1))
}

/// Sentinels are created on the primary monitor; confirm that before the daemon moves them.
fn assert_on_primary(ws: &[Sentinel], mon: Rect4) {
    for (i, w) in ws.iter().enumerate() {
        let v = vis(w);
        let (cx, cy) = ((v.0 + v.2) / 2, (v.1 + v.3) / 2);
        assert!(
            cx >= mon.0 && cx < mon.2 && cy >= mon.1 && cy < mon.3,
            "sentinel {i} is not on the primary monitor {mon:?}: visible frame {v:?}"
        );
    }
}

/// Press a chord without the post-action wait of `press()`, so the caller can sample while it animates.
fn chord_now(mods: &'static [u8], vk: u8) {
    send_chord_raw(mods, vk);
}

/// One desktop frame with its output size and sentinel colour boxes. Retries until a frame arrives (5 s max).
fn frame_boxes(cap: &Capture) -> (i32, i32, [BBox; N]) {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match cap.boxes() {
            Ok((_, w, h, b)) => return (w, h, b),
            Err(e) => assert!(Instant::now() < until, "no desktop frame within 5 s: {e}"),
        }
    }
}

/// Foreground window description for failure messages: hwnd, window class and owning process image.
fn describe_window(h: isize) -> String {
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
    if h == 0 {
        return "hwnd 0x0 (no foreground window)".into();
    }
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h as _, buf.as_mut_ptr(), buf.len() as i32) };
    let class = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(h as _, &mut pid) };
    let mut process = String::from("?");
    unsafe {
        let ph = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) as isize;
        if ph != 0 {
            let mut pb = [0u16; 1024];
            let mut len = pb.len() as u32;
            if QueryFullProcessImageNameW(ph as _, 0, pb.as_mut_ptr(), &mut len) != 0 {
                process = String::from_utf16_lossy(&pb[..len as usize]);
            }
            CloseHandle(ph as _);
        }
    }
    format!("hwnd 0x{h:x} class '{class}' pid {pid} process '{process}'")
}

/// Make sentinel `target` the foreground window: tap Alt (so Windows lets this process take focus), then
/// SetForegroundWindow on it. Up to 4 tries, 400 ms apart. Panics with the foreground window's class/process
/// if every try fails.
fn claim_focus(ws: &[Sentinel], target: usize) {
    for attempt in 1..=4 {
        let _ = take_foreground(ws[target].hwnd);
        if fg_index(ws).is_some() {
            return;
        }
        if attempt < 4 {
            thread::sleep(Duration::from_millis(400));
        }
    }
    panic!(
        "focus_to({target}): the foreground window is not a sentinel after 4 tries to claim focus (Alt tap + SetForegroundWindow on sentinel {target}); foreground window: {}",
        describe_window(fg())
    );
}

/// Open the primary output and check it starts at the primary monitor origin.
/// Call this after the daemon has settled its last move: the first frame delivered is the current desktop.
fn open_primary(mon: Rect4) -> Capture {
    let cap = Capture::open(0).expect("DXGI primary output duplication");
    assert_eq!(
        cap.origin,
        (mon.0, mon.1),
        "DXGI output 0 does not start at the primary monitor origin"
    );
    cap
}

/// Press Alt+Left/Right until sentinel `target` has focus, then check it is on screen.
/// Sentinel 2 (the last window) is focused at start; sentinel 0 is parked until something scrolls it into view.
/// If another application holds the foreground (common right after the daemon starts), focus is claimed first.
fn focus_to(ws: &[Sentinel], target: usize) {
    let mut claimed = false;
    for _ in 0..2 * N {
        let cur = match fg_index(ws) {
            Some(c) => c,
            None => {
                claim_focus(ws, target);
                claimed = true;
                fg_index(ws).expect("claim_focus returned with a sentinel focused")
            }
        };
        if cur == target {
            break;
        }
        press(&[ALT], if cur < target { RIGHT } else { LEFT });
    }
    if claimed && fg_index(ws) == Some(target) {
        // Claiming focus changes the foreground directly; the daemon scrolls the strip to the new focus
        // asynchronously (#93), so give that scroll the same settle time `press` gives a hotkey step.
        let _ = eventually(2, || reachable(&ws[target]));
    }
    assert_eq!(
        fg_index(ws),
        Some(target),
        "focus_to: expected sentinel {target} focused after scrolling, observed {:?}",
        fg_index(ws)
    );
    assert!(
        reachable(&ws[target]),
        "focus_to: sentinel {target} is focused but not reachable (visible frame {:?})",
        vis(&ws[target])
    );
}

/// One desktop frame with its output size and the focused sentinel (None if the focus changed while grabbing).
/// Only call this right after a change of the desktop: an unchanged desktop delivers no new frame.
fn settled_frame(cap: &Capture, hw: &[isize; N]) -> (i32, Option<usize>, [BBox; N]) {
    let before = fg_slot(hw);
    let (w, _h, boxes) = frame_boxes(cap);
    let after = fg_slot(hw);
    (w, if before == after { before } else { None }, boxes)
}

/// Settled colour boxes per sentinel, separately for "focused" and "unfocused". The focused tile's
/// 2 px border shrinks its colour box, so each focus state has its own reference.
struct Refs {
    focused: [Option<Rect4>; N],
    unfocused: [Option<Rect4>; N],
}
impl Refs {
    fn new() -> Self {
        Self { focused: [None; N], unfocused: [None; N] }
    }
}

/// Compare one frame's colour boxes with the references of the same focus state.
/// `seed` true: a missing reference is recorded from this frame. `seed` false: a missing reference is skipped.
/// `counts[i]` is incremented for every box of sentinel i that was seeded or compared.
fn observe(
    refs: &mut Refs,
    out_w: i32,
    fg: Option<usize>,
    boxes: &[BBox; N],
    when: &str,
    seed: bool,
    counts: &mut [usize; N],
) {
    let Some(f) = fg else { return };
    for i in 0..N {
        let Some(b) = boxes[i] else { continue };
        if touches_edge(b, out_w) {
            continue;
        }
        let focused = f == i;
        let state = if focused { "focused" } else { "unfocused" };
        let slot = if focused { &mut refs.focused[i] } else { &mut refs.unfocused[i] };
        let cur = *slot;
        match cur {
            None if seed => {
                *slot = Some(b);
                counts[i] += 1;
            }
            None => {}
            Some(r) => {
                assert!(
                    size_ok(b, r),
                    "{when}: sentinel {i} ({}) while {state}: expected size {}x{} with top {} and bottom {} (settled box {}), observed size {}x{} with top {} and bottom {} (box {}); all boxes: {}",
                    colour(i),
                    width(r),
                    height(r),
                    r.1,
                    r.3,
                    fmt_box(Some(r)),
                    width(b),
                    height(b),
                    b.1,
                    b.3,
                    fmt_box(Some(b)),
                    fmt_boxes(boxes)
                );
                counts[i] += 1;
            }
        }
    }
}

/// One desktop frame taken while a chord animates: present time, focused sentinel
/// (None when the focus changed during the grab), and colour boxes.
struct Obs {
    t: i64,
    fg: Option<usize>,
    boxes: [BBox; N],
}

/// Start a chord on a helper thread and sample the primary output for `ms` milliseconds.
fn sample_with_focus(cap: &Capture, hw: &[isize; N], mods: &'static [u8], vk: u8, ms: u64) -> Vec<Obs> {
    let key = thread::spawn(move || chord_now(mods, vk));
    let until = Instant::now() + Duration::from_millis(ms);
    let mut out = Vec::new();
    while Instant::now() < until {
        let before = fg_slot(hw);
        let Ok((t, _w, _h, boxes)) = cap.boxes() else { continue };
        let after = fg_slot(hw);
        out.push(Obs { t, fg: if before == after { before } else { None }, boxes });
    }
    key.join().unwrap();
    out
}

/// Every sentinel's visible width must equal `expect[i]` within TOL, for the sentinels that are reachable.
/// Returns how many were measured.
fn assert_widths_reachable(ws: &[Sentinel], expect: [i32; N], skip: Option<usize>, label: &str) -> usize {
    let mut measured = 0;
    for i in 0..N {
        if Some(i) == skip || !reachable(&ws[i]) {
            continue;
        }
        measured += 1;
        let v = vis(&ws[i]);
        assert!(
            (width(v) - expect[i]).abs() <= TOL,
            "[{label}] sentinel {i}: expected visible width {} px, observed {} px (visible frame {v:?})",
            expect[i],
            width(v)
        );
    }
    measured
}

/// Visible frames of the strip, for the reachable sentinels only: each has the work-area height, and each pair of
/// adjacent reachable sentinels is separated by exactly `gap` px. Parked windows are not measured.
fn assert_geometry(ws: &[Sentinel], work: Rect4, gap: i32, label: &str) {
    let reach: Vec<bool> = ws.iter().map(reachable).collect();
    let n_reach = reach.iter().filter(|r| **r).count();
    let frames: Vec<Rect4> = ws.iter().map(vis).collect();
    assert!(
        n_reach >= 2,
        "[{label}] only {n_reach} sentinel(s) reachable, need at least 2 to check the gap; visible frames {frames:?}"
    );
    for i in 0..N {
        if reach[i] {
            assert!(
                height(frames[i]) == height(work),
                "[{label}] sentinel {i}: visible height {} px, expected work-area height {} px (visible {:?}, work area {work:?})",
                height(frames[i]),
                height(work),
                frames[i]
            );
        }
    }
    let mut pairs = 0;
    for i in 0..N - 1 {
        if !(reach[i] && reach[i + 1]) {
            continue;
        }
        pairs += 1;
        let observed = frames[i + 1].0 - frames[i].2;
        assert!(
            (observed - gap).abs() <= GAP_TOL,
            "[{label}] gap between sentinel {i} and sentinel {} expected {gap} px (+-{GAP_TOL}, gap_px {GAP_PX}), observed {observed} px (visible {:?} and {:?})",
            i + 1,
            frames[i],
            frames[i + 1]
        );
    }
    assert!(pairs > 0, "[{label}] no adjacent pair of reachable sentinels to check the gap (reachable {reach:?})");
}

/// Peek slivers for focus on sentinel `f`. Only a MIDDLE window has off-view neighbours on both sides, so only then
/// must the left neighbour leave a PEEK_PX sliver on the left output edge and the right neighbour one on the right
/// edge. At the first or last window the neighbour on the other side is on screen (not off-view), so no sliver is
/// required there; instead the two on-screen tiles must be fully visible on the monitor.
fn check_peeks(ws: &[Sentinel], cap: &Capture, hw: &[isize; N], mon: Rect4, f: usize) {
    let (out_w, fg, boxes) = settled_frame(cap, hw);
    assert_eq!(fg, Some(f), "peek: expected sentinel {f} focused while measuring, foreground is {fg:?}");
    let all = fmt_boxes(&boxes);
    // A middle window that is already fully on screen does not move the strip (core test
    // focusing_a_visible_window_never_slides_the_strip), so an off-view neighbour is parked, not peeking.
    // No sliver is owed then. A neighbour cut by an edge still has to show its exact sliver below.
    let shown = |b: BBox| b.is_some_and(|b| b.0 > 0 && b.2 < out_w);
    let parked_or_shown = |k: usize| boxes[k].is_none() || shown(boxes[k]);
    if f > 0 && f + 1 < N && shown(boxes[f]) && parked_or_shown(f - 1) && parked_or_shown(f + 1) {
        return;
    }
    if f == 0 {
        assert!(
            boxes.iter().flatten().all(|b| b.0 > 0),
            "peek (focus 0): no window should leave a sliver on the left edge (it is the first window), but a colour touches it; all boxes: {all}"
        );
    } else if f + 1 < N {
        let n = f - 1;
        let Some(b) = boxes[n] else {
            panic!(
                "peek (focus {f}): sentinel {n} ({}) shows no colour at the left output edge; expected its {PEEK_PX} px sliver; all boxes: {all}",
                colour(n)
            )
        };
        assert!(
            b.0 <= 0,
            "peek (focus {f}): sentinel {n} ({}) should leave a {PEEK_PX} px sliver on the left edge, but its colour starts at x={} (box {}); all boxes: {all}",
            colour(n),
            b.0,
            fmt_box(Some(b))
        );
        let observed = b.2 - b.0;
        assert!(
            (observed - PEEK_PX).abs() <= PEEK_TOL,
            "peek (focus {f}): sentinel {n} ({}) left sliver expected {PEEK_PX} px (+-{PEEK_TOL}), observed {observed} px (box {}); all boxes: {all}",
            colour(n),
            fmt_box(Some(b))
        );
    }
    if f + 1 == N {
        assert!(
            boxes.iter().flatten().all(|b| b.2 < out_w),
            "peek (focus {f}, last window): no window should leave a sliver on the right edge, but a colour touches it; all boxes: {all}"
        );
    } else if f > 0 {
        let n = f + 1;
        let Some(b) = boxes[n] else {
            panic!(
                "peek (focus {f}): sentinel {n} ({}) shows no colour at the right output edge (x={out_w}); expected its {PEEK_PX} px sliver; all boxes: {all}",
                colour(n)
            )
        };
        assert!(
            b.2 >= out_w,
            "peek (focus {f}): sentinel {n} ({}) should leave a {PEEK_PX} px sliver on the right edge, but its colour ends at x={} (box {}); all boxes: {all}",
            colour(n),
            b.2,
            fmt_box(Some(b))
        );
        let observed = out_w - b.0;
        assert!(
            (observed - PEEK_PX).abs() <= PEEK_TOL,
            "peek (focus {f}): sentinel {n} ({}) right sliver expected {PEEK_PX} px (+-{PEEK_TOL}), observed {observed} px (box {}); all boxes: {all}",
            colour(n),
            fmt_box(Some(b))
        );
    }
    if f == 0 || f + 1 == N {
        // Strip end: the two on-screen tiles fill the monitor and must be fully visible.
        let pair = if f == 0 { [0, 1] } else { [N - 2, N - 1] };
        for k in pair {
            let v = vis(&ws[k]);
            assert!(
                v.0 >= mon.0 - TOL && v.2 <= mon.2 + TOL,
                "peek (focus {f}): on-screen tile {k} ({}) should be fully visible on the primary monitor {mon:?} at this strip end, observed visible frame {v:?}; all boxes: {all}",
                colour(k)
            );
        }
    }
}

#[test]
#[ignore]
fn scroll_boxes_constant_size() {
    aware();
    let ws: Vec<Sentinel> = (0..N).map(|i| Sentinel::new(i, 0)).collect();
    let (mon, _work) = primary();
    assert_on_primary(&ws, mon);
    let hw = hwnds(&ws);
    let mut d = Daemon::new_animated("scroll-size");
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "scroll daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    // Sentinel 0 is parked at start: bring it on screen first, so every sentinel has a settled reference.
    focus_to(&ws, 0);
    let cap = open_primary(mon);
    let mut refs = Refs::new();
    let mut seeded = [0usize; N];
    let (out_w, fg, boxes) = settled_frame(&cap, &hw);
    assert_eq!(fg, Some(0), "settled before Alt+Right: expected sentinel 0 focused, observed {fg:?}");
    observe(&mut refs, out_w, fg, &boxes, "settled before Alt+Right", true, &mut seeded);
    assert!(
        refs.unfocused[1].is_some(),
        "precondition: sentinel 1 has no settled reference while unfocused; boxes: {}",
        fmt_boxes(&boxes)
    );

    let samples = sample_with_focus(&cap, &hw, &[ALT], RIGHT, 900);
    eprintln!("scroll_boxes_constant_size: {} frames sampled during Alt+Right", samples.len());
    // The last frame of the 900 ms window is the settled desktop after the scroll.
    let last = samples.last().expect("no desktop frame during the scroll");
    assert_eq!(last.fg, Some(1), "after Alt+Right: expected sentinel 1 focused, observed {:?}", last.fg);
    observe(&mut refs, out_w, last.fg, &last.boxes, "settled after Alt+Right", true, &mut seeded);

    let mut checked = [0usize; N];
    let mut skipped = 0;
    for (k, o) in samples.iter().enumerate() {
        if o.fg.is_none() {
            skipped += 1;
            continue;
        }
        observe(
            &mut refs,
            out_w,
            o.fg,
            &o.boxes,
            &format!("scroll sample #{k} (present time {})", o.t),
            false,
            &mut checked,
        );
    }
    eprintln!(
        "scroll_boxes_constant_size: checks per sentinel {checked:?}, samples skipped (focus changing) {skipped}"
    );
    assert!(
        checked.iter().sum::<usize>() > 0,
        "no sentinel stayed off the output edge with a settled reference during the scroll (skipped {skipped} samples)"
    );
}

#[test]
#[ignore]
fn scroll_monotonic_and_gapped() {
    aware();
    let ws: Vec<Sentinel> = (0..N).map(|i| Sentinel::new(i, 0)).collect();
    let (mon, _work) = primary();
    assert_on_primary(&ws, mon);
    let hw = hwnds(&ws);
    let mut d = Daemon::new_animated("scroll-mono");
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "scroll daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    // Sentinels 0 and 1 fit the monitor side by side, so stepping from 0 to 1 never moves the strip. The scroll
    // that has to be measured is the step from 1 into sentinel 2, which is parked until the strip slides.
    focus_to(&ws, 0);
    press(&[ALT], RIGHT);
    assert_eq!(
        fg_index(&ws),
        Some(1),
        "setup: expected sentinel 1 focused before the measured scroll, observed {:?}",
        fg_index(&ws)
    );
    let cap = open_primary(mon);
    let samples = sample_with_focus(&cap, &hw, &[ALT], RIGHT, 900);
    eprintln!("scroll_monotonic_and_gapped: {} frames sampled during Alt+Right", samples.len());
    assert_eq!(
        fg_index(&ws),
        Some(2),
        "after Alt+Right: expected sentinel 2 focused, observed {:?}",
        fg_index(&ws)
    );

    // Left edges must move monotonically toward the destination: no backward jump beyond TOL.
    let mut moved = 0;
    for i in 0..N {
        let lefts: Vec<(usize, i32)> = samples
            .iter()
            .enumerate()
            .filter_map(|(k, o)| box_of(&o.boxes, i).map(|r| (k, r.0)))
            .collect();
        let (Some(&(_, first)), Some(&(_, last))) = (lefts.first(), lefts.last()) else {
            eprintln!("scroll_monotonic_and_gapped: sentinel {i} was never on the desktop during the scroll");
            continue;
        };
        let total = last - first;
        if total.abs() <= TOL {
            continue;
        }
        moved += 1;
        let dir = total.signum();
        for w in lefts.windows(2) {
            let ((_, pl), (ck, cl)) = (w[0], w[1]);
            let toward = (cl - pl) * dir;
            assert!(
                toward >= -TOL,
                "scroll sample #{ck} (present time {}), sentinel {i} colour {}: left edge jumped backwards by {}px ({pl} -> {cl}) while moving toward destination x={last} from x={first}; all boxes: {}",
                samples[ck].t,
                colour(i),
                -toward,
                fmt_boxes(&samples[ck].boxes)
            );
        }
    }
    assert!(moved > 0, "no sentinel visibly moved during Alt+Right, so no motion was checked");

    // After settle: adjacent reachable sentinels are separated by gap_px scaled by DPI.
    let gap = GAP_PX;
    let reach: Vec<bool> = ws.iter().map(reachable).collect();
    let mut pairs = 0;
    for i in 0..N - 1 {
        if !(reach[i] && reach[i + 1]) {
            continue;
        }
        pairs += 1;
        let (a, b) = (vis(&ws[i]), vis(&ws[i + 1]));
        let observed = b.0 - a.2;
        assert!(
            (observed - gap).abs() <= GAP_TOL,
            "after Alt+Right settled: gap between sentinel {i} and sentinel {} expected {gap} px (+-{GAP_TOL}, gap_px {GAP_PX}), observed {observed} px (visible {a:?} and {b:?})",
            i + 1
        );
    }
    assert!(pairs > 0, "after Alt+Right settled: no adjacent pair of reachable sentinels to check the gap (reachable {reach:?})");
}

#[test]
#[ignore]
fn settled_layout_geometry() {
    aware();
    let ws: Vec<Sentinel> = (0..N).map(|i| Sentinel::new(i, 0)).collect();
    let (mon, work) = primary();
    assert_on_primary(&ws, mon);
    let mut d = Daemon::new("scroll-geom");
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "scroll daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    let gap = GAP_PX;
    // Start is focus on the last window (sentinel 2), so Alt+Left moves the strip; checkpoints follow the focus.
    assert_geometry(&ws, work, gap, "settled at start");
    for (label, key, expect) in [
        ("after Alt+Left #1", LEFT, 1),
        ("after Alt+Left #2", LEFT, 0),
        ("after Alt+Right #1", RIGHT, 1),
    ] {
        press(&[ALT], key);
        assert_eq!(
            fg_index(&ws),
            Some(expect),
            "[{label}] expected sentinel {expect} focused, observed {:?}",
            fg_index(&ws)
        );
        assert_geometry(&ws, work, gap, label);
    }
}

#[test]
#[ignore]
fn resize_then_scroll() {
    aware();
    let ws: Vec<Sentinel> = (0..N).map(|i| Sentinel::new(i, 0)).collect();
    let (mon, work) = primary();
    assert_on_primary(&ws, mon);
    let mut d = Daemon::new("scroll-resize");
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "scroll daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));

    // Original lengths: each window is measured while it is on screen.
    let mut orig = [0i32; N];
    for i in 0..N {
        focus_to(&ws, i);
        orig[i] = width(vis(&ws[i]));
    }
    eprintln!("resize_then_scroll: original visible widths {orig:?}");

    // Resize the middle sentinel (on screen, so the resize is visible to the user).
    focus_to(&ws, 1);
    let work_w = width(work);
    let target = (work_w as f64 * 0.7).round() as i32;
    let r = ws[1].rect();
    let extra_w = (r[2] - r[0]) - width(vis(&ws[1]));
    let ok = unsafe {
        SetWindowPos(
            ws[1].hwnd as _,
            null_mut(),
            r[0],
            r[1],
            target + extra_w,
            r[3] - r[1],
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
    assert_ne!(ok, 0, "SetWindowPos failed on sentinel 1");
    thread::sleep(Duration::from_millis(1500));
    let w1 = width(vis(&ws[1]));
    assert!(
        (w1 - target).abs() <= TOL,
        "resize did not stick: sentinel 1 expected visible width {target} px (70% of work width {work_w} px), observed {w1} px"
    );
    // Only the untouched windows are checked after the scroll. A SetWindowPos resize is not a user resize (only
    // an edge drag is, see resize_by_edge_drag): the next layout restores the strip's length for the resized
    // window, so its width is not part of the push contract (docs/TEST_PLAN.md S2.4).
    let expect = [orig[0], target, orig[2]];
    let skip = Some(1);

    // Push semantics: every window on screen keeps its length while scrolling through all windows.
    focus_to(&ws, 0);
    assert!(
        assert_widths_reachable(&ws, expect, skip, "after resize, focus 0") > 0,
        "after resize: no sentinel reachable to measure"
    );
    for (label, key) in [
        ("after Alt+Right #1", RIGHT),
        ("after Alt+Right #2", RIGHT),
        ("after Alt+Left #1", LEFT),
        ("after Alt+Left #2", LEFT),
    ] {
        press(&[ALT], key);
        let measured = assert_widths_reachable(&ws, expect, skip, label);
        assert!(measured > 0, "[{label}] no sentinel reachable to measure");
    }
}

#[test]
#[ignore]
fn peek_visible() {
    aware();
    let ws: Vec<Sentinel> = (0..N).map(|i| Sentinel::new(i, 0)).collect();
    let (mon, _work) = primary();
    assert_on_primary(&ws, mon);
    let hw = hwnds(&ws);
    let mut d = Daemon::new("scroll-peek");
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "scroll daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    // Focus on the last window (the start state), then the middle, then the first window.
    focus_to(&ws, N - 1);
    let cap = open_primary(mon);
    check_peeks(&ws, &cap, &hw, mon, N - 1);
    focus_to(&ws, 1);
    check_peeks(&ws, &cap, &hw, mon, 1);
    focus_to(&ws, 0);
    check_peeks(&ws, &cap, &hw, mon, 0);
}

#[test]
#[ignore]
fn scroll_no_shrink_or_offset_6_windows() {
    aware();
    let ws: Vec<Sentinel> = (0..N).map(|i| Sentinel::new(i, 0)).collect();
    let (mon, _work) = primary();
    assert_on_primary(&ws, mon);
    let hw = hwnds(&ws);
    let mut d = Daemon::new_animated("scroll-six");
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "scroll daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    focus_to(&ws, N - 1);
    let cap = open_primary(mon);

    let mut refs = Refs::new();
    let mut counts = [0usize; N];
    let (out_w, fg, boxes) = settled_frame(&cap, &hw);
    observe(&mut refs, out_w, fg, &boxes, "settled at start (focus last)", true, &mut counts);

    // Six presses across the strip and back: Alt+Left x3 (the third is clamped at window 0),
    // then Alt+Right x3 (the third is clamped at the last window). Expected focus after each press:
    let steps: [(u8, usize); 6] = [(LEFT, 1), (LEFT, 0), (LEFT, 0), (RIGHT, 1), (RIGHT, 2), (RIGHT, 2)];
    for (k, (key, expect)) in steps.into_iter().enumerate() {
        let name = if key == LEFT { "Alt+Left" } else { "Alt+Right" };
        let samples = sample_with_focus(&cap, &hw, &[ALT], key, 900);
        assert_eq!(
            fg_index(&ws),
            Some(expect),
            "press #{} ({name}): expected sentinel {expect} focused afterwards, observed {:?}",
            k + 1,
            fg_index(&ws)
        );
        let mut skipped = 0;
        for o in &samples {
            if o.fg.is_none() {
                skipped += 1;
                continue;
            }
            observe(
                &mut refs,
                out_w,
                o.fg,
                &o.boxes,
                &format!("press #{} ({name}), sample at present time {}", k + 1, o.t),
                true,
                &mut counts,
            );
        }
        eprintln!(
            "scroll_no_shrink_or_offset_6_windows: press #{} ({name}): {} frames, {skipped} skipped",
            k + 1,
            samples.len()
        );
    }
    assert!(
        counts.iter().all(|&c| c > 0),
        "some sentinel was never on the output away from its edges, so its size was never checked: checks per sentinel {counts:?}"
    );
}

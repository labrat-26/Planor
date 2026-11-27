#![allow(dead_code, unused_imports)]
include!("common/helpers.rs");

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    KEYEVENTF_KEYUP, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, VK_ESCAPE, VK_SHIFT, keybd_event,
    mouse_event,
};

/// Stacked-column border colour (planor.example.toml [borders].stacked_color = #A855F7), as 0x00RRGGBB.
const STACKED_RGB: u32 = 0x00a855f7;
/// Time the cursor rests over a drop target before release. Above stack_hover_ms (350) and the plan's 450 ms,
/// and it also keeps the cursor still long enough that the drop-time velocity is below drag_slow_velocity_px_s.
const HOLD: Duration = Duration::from_millis(600);
/// Hover time for the stacking drop: well above stack_hover_ms (350). The cursor keeps moving by +-2 px
/// every 50 ms during the hover (80 px/s) so the daemon keeps receiving input while the velocity stays
/// below drag_slow_velocity_px_s (300).
const HOVER: Duration = Duration::from_millis(900);
/// gap_px from the config (planor.example.toml), before DPI scaling.
const GAP_PX: f64 = 6.0;
const SETTLE: Duration = Duration::from_millis(1500);
const STEP: Duration = Duration::from_millis(15);
const STEPS: i32 = 20;

fn rect4(r: RECT) -> [i32; 4] {
    [r.left, r.top, r.right, r.bottom]
}
/// DWM visible frame of a sentinel as [left, top, right, bottom].
fn vis(w: &Sentinel) -> [i32; 4] {
    rect4(w.visible())
}
fn width(r: [i32; 4]) -> i32 {
    r[2] - r[0]
}
fn height(r: [i32; 4]) -> i32 {
    r[3] - r[1]
}
fn centre(r: [i32; 4]) -> (i32, i32) {
    ((r[0] + r[2]) / 2, (r[1] + r[3]) / 2)
}
fn inside(p: (i32, i32), r: &RECT) -> bool {
    p.0 >= r.left && p.0 < r.right && p.1 >= r.top && p.1 < r.bottom
}
/// What `w` answers to WM_NCHITTEST at screen point `p` (HTLEFT, HTCLIENT, ...): which part of it a press grabs.
/// `None` when its thread does not answer within 500 ms: the sentinel lives on another thread, and an unbounded
/// cross-thread SendMessage to a hung window would block the test forever.
fn nc_hit(w: &Sentinel, p: (i32, i32)) -> Option<isize> {
    let l = (((p.1 as u16 as u32) << 16) | p.0 as u16 as u32) as isize;
    let mut out = 0usize;
    let ok = unsafe {
        SendMessageTimeoutW(
            w.hwnd as _,
            WM_NCHITTEST,
            0,
            l,
            SMTO_ABORTIFHUNG,
            500,
            &mut out,
        )
    };
    (ok != 0).then_some(out as isize)
}
/// The top-level window a real mouse press at `p` lands on. `WindowFromPoint` skips click-through windows
/// (`WS_EX_LAYERED | WS_EX_TRANSPARENT`, as Planor's overlay is), the way real mouse input does.
fn root_at(p: (i32, i32)) -> isize {
    unsafe { GetAncestor(WindowFromPoint(POINT { x: p.0, y: p.1 }), GA_ROOT) as isize }
}
/// A real press at `p` grabs the leading (left) resize border of `w`: `p` is on a monitor, `w` is the top-level window
/// the press lands on (no other window covers the point), and `w` answers HTLEFT there.
fn grabs_leading_border(w: &Sentinel, p: (i32, i32)) -> bool {
    on_any_monitor(p) && root_at(p) == w.hwnd && nc_hit(w, p) == Some(HTLEFT as isize)
}
fn on_any_monitor(p: (i32, i32)) -> bool {
    mons().iter().any(|(m, _)| inside(p, m))
}
/// Point at `frac` of the window's visible width measured from its left edge (inside the window, vertical centre).
fn left_point(w: &Sentinel, frac: f64) -> (i32, i32) {
    let v = vis(w);
    (
        (v[0] as f64 + width(v) as f64 * frac) as i32,
        (v[1] + v[3]) / 2,
    )
}
/// Point at `frac` of the window's visible width measured from its right edge (inside the window).
fn right_point(w: &Sentinel, frac: f64) -> (i32, i32) {
    let v = vis(w);
    (
        (v[2] as f64 - width(v) as f64 * frac) as i32,
        (v[1] + v[3]) / 2,
    )
}
fn describe(ws: &[Sentinel]) -> String {
    ws.iter()
        .enumerate()
        .map(|(i, w)| format!("w{i}={:?}", vis(w)))
        .collect::<Vec<_>>()
        .join(" ")
}
/// Window indices sorted by strip position (visible left edge).
fn order(ws: &[Sentinel]) -> Vec<usize> {
    let mut o: Vec<usize> = (0..ws.len()).collect();
    o.sort_by_key(|&i| vis(&ws[i])[0]);
    o
}
/// Point to grab a window by: x = centre of its on-screen part, y = hwnd rect top + 12 px (caption).
/// None when fewer than 16 px of the window are on screen.
fn grab(w: &Sentinel) -> Option<(i32, i32)> {
    let v = w.visible();
    let (mr, _) = mons().into_iter().max_by_key(|(m, _)| overlap(&v, m))?;
    if overlap(&v, &mr) == 0 {
        return None;
    }
    let left = v.left.max(mr.left);
    let right = v.right.min(mr.right);
    if right - left < 16 {
        return None;
    }
    Some(((left + right) / 2, w.rect()[1] + 12))
}
/// `grab`, but only when the window is the one under the point after raising it (None when another window
/// covers the caption, which the callers report as having no caption to grab).
fn grab_raised(w: &Sentinel) -> Option<(i32, i32)> {
    let p = grab(w)?;
    raise_until_hit(w, p).then_some(p)
}
/// The window the mouse would hit at screen point `p` (0 when none).
fn window_at(p: (i32, i32)) -> isize {
    unsafe { WindowFromPoint(POINT { x: p.0, y: p.1 }) as isize }
}
/// Bring a sentinel to the front and wait until it is the window under `p`. Closing a window makes Windows
/// activate the next one in z-order, which can be an unrelated window (the user's) left covering the caption
/// that a drag starts on, so earlier scenarios would change the outcome of this one.
fn raise_until_hit(w: &Sentinel, p: (i32, i32)) -> bool {
    eventually(3, || {
        unsafe {
            SetForegroundWindow(w.hwnd as _);
            BringWindowToTop(w.hwnd as _);
        }
        window_at(p) == w.hwnd
    })
}

/// Real mouse input. Releases the left button, the Shift key and restores the cursor on drop, even on panic.
struct Mouse {
    saved: POINT,
    left: bool,
    shift: bool,
}
impl Mouse {
    fn begin() -> Self {
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p) };
        Self {
            saved: p,
            left: false,
            shift: false,
        }
    }
    fn put(&self, p: (i32, i32)) {
        unsafe { SetCursorPos(p.0, p.1) };
    }
    fn down(&mut self, at: (i32, i32)) {
        self.put(at);
        thread::sleep(Duration::from_millis(60));
        unsafe { mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0) };
        self.left = true;
        thread::sleep(Duration::from_millis(60));
    }
    /// 20 steps of 15 ms from `from` to `to`.
    fn glide(&self, from: (i32, i32), to: (i32, i32)) {
        for i in 1..=STEPS {
            self.put((
                from.0 + (to.0 - from.0) * i / STEPS,
                from.1 + (to.1 - from.1) * i / STEPS,
            ));
            thread::sleep(STEP);
        }
    }
    fn up(&mut self) {
        if self.left {
            unsafe { mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0) };
            self.left = false;
        }
    }
    /// Press at `from`, glide to `to`, rest there (HOLD), release. Fails as a precondition when the cursor is no
    /// longer at `to`: outside input (a real mouse, another process) moved it, so the drop would land somewhere the
    /// test never asked for and any assertion about the result would blame Planor for it. That failure path first
    /// cancels the move (Escape snaps the window back, so Planor applies no drop) and only then lets the button
    /// go, so neither this test nor `Drop` releases the window at the moved cursor and changes the next test's layout.
    fn drag(&mut self, from: (i32, i32), to: (i32, i32)) {
        self.down(from);
        self.glide(from, to);
        thread::sleep(HOLD);
        let mut at = POINT::default();
        unsafe { GetCursorPos(&mut at) };
        if (at.x, at.y) != to {
            self.escape();
            self.up();
        }
        assert!(
            (at.x, at.y) == to,
            "precondition: outside input moved the cursor during the drag (expected it at {to:?} before release, \
             observed ({}, {})); do not use the mouse while live tests run",
            at.x,
            at.y
        );
        self.up();
    }
    fn shift(&mut self, on: bool) {
        unsafe { keybd_event(VK_SHIFT as u8, 0, if on { 0 } else { KEYEVENTF_KEYUP }, 0) };
        self.shift = on;
        thread::sleep(Duration::from_millis(60));
    }
    fn escape(&self) {
        unsafe {
            keybd_event(VK_ESCAPE as u8, 0, 0, 0);
            keybd_event(VK_ESCAPE as u8, 0, KEYEVENTF_KEYUP, 0);
        }
        thread::sleep(Duration::from_millis(100));
    }
}
impl Drop for Mouse {
    fn drop(&mut self) {
        unsafe {
            if self.shift {
                keybd_event(VK_SHIFT as u8, 0, KEYEVENTF_KEYUP, 0);
            }
            if self.left {
                mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
            }
            SetCursorPos(self.saved.x, self.saved.y);
        }
    }
}

/// Number of pixels in the captured desktop frame equal to `rgb`. 0 when no new frame arrives within 250 ms.
fn count_rgb(cap: &Capture, rgb: u32) -> usize {
    unsafe {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res = None;
        if cap.dupl.AcquireNextFrame(250, &mut info, &mut res).is_err() {
            return 0;
        }
        let result = (|| -> windows::core::Result<usize> {
            let tex: ID3D11Texture2D = res.unwrap().cast()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as _;
            desc.BindFlags = 0;
            desc.MiscFlags = 0;
            let mut copy = None;
            cap.dev.CreateTexture2D(&desc, None, Some(&mut copy))?;
            let copy = copy.unwrap();
            cap.ctx.CopyResource(&copy, &tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            cap.ctx
                .Map(&copy, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let mut n = 0usize;
            for y in 0..desc.Height as usize {
                let row = std::slice::from_raw_parts(
                    (mapped.pData as *const u8)
                        .add(y * mapped.RowPitch as usize)
                        .cast::<u32>(),
                    desc.Width as usize,
                );
                n += row.iter().filter(|px| **px & 0xffffff == rgb).count();
            }
            cap.ctx.Unmap(&copy, 0);
            Ok(n)
        })();
        let _ = cap.dupl.ReleaseFrame();
        result.unwrap_or(0)
    }
}

/// Settled colour box of sentinel colour `k` (SENTINEL_RGB index) on the primary output: (output width, box).
/// Retries while desktop duplication times out (it delivers no frame while nothing on screen changes).
fn colour_box(cap: &Capture, k: usize) -> (i32, BBox) {
    for _ in 0..8 {
        if let Ok((_, w, _, boxes)) = cap.boxes() {
            return (w, boxes[k]);
        }
    }
    panic!("colour_box: desktop duplication delivered no frame in 2 s");
}

/// Asserts each window is `reachable` (>= 90% on a monitor).
fn assert_all_reachable(ws: &[(usize, &Sentinel)], ctx: &str) {
    for &(i, w) in ws {
        let f = on_screen_fraction(w);
        assert!(
            f >= 0.9,
            "{ctx}: window {i} must be reachable: expected >= 0.90 of its frame on a monitor, observed {f:.2} (visible rect {:?})",
            vis(w)
        );
    }
}
/// Asserts tiled geometry of `ws` on a monitor with work rect `wr`. Vertical: each window spans the work width and
/// the windows differ in y. Horizontal: each spans the work height and the windows differ in x. Tolerance 3 px.
fn check_layout(ws: &[(usize, &Sentinel)], wr: RECT, vertical: bool, ctx: &str) {
    let work_w = wr.right - wr.left;
    let work_h = wr.bottom - wr.top;
    for &(i, w) in ws {
        let v = vis(w);
        if vertical {
            assert!(
                (width(v) - work_w).abs() <= 3,
                "{ctx}: window {i} should span the portrait work-area width: expected width {work_w} (+-3px), observed {} (visible rect {v:?})",
                width(v)
            );
        } else {
            assert!(
                (height(v) - work_h).abs() <= 3,
                "{ctx}: window {i} should span the work-area height: expected height {work_h} (+-3px), observed {} (visible rect {v:?})",
                height(v)
            );
        }
    }
    // Parked off-screen tiles are intentionally anchored at (park_x, bound.y) (#46) so they share
    // `y` with the top on-screen portrait tile. Verify axis separation across all on-screen tiles
    // on `wr` and ensure at least min(ws.len(), 2) tiles are actually on-screen.
    let on_screen: Vec<(usize, [i32; 4])> = ws
        .iter()
        .map(|&(i, w)| (i, vis(w)))
        .filter(|&(_, v)| {
            overlap(
                &RECT {
                    left: v[0],
                    top: v[1],
                    right: v[2],
                    bottom: v[3],
                },
                &wr,
            ) > 0
        })
        .collect();
    let min_on_screen = ws.len().min(2);
    assert!(
        on_screen.len() >= min_on_screen,
        "{ctx}: expected at least {min_on_screen} windows on monitor rect {:?}, observed {} on-screen ({on_screen:?})",
        [wr.left, wr.top, wr.right, wr.bottom],
        on_screen.len()
    );
    for a in 0..on_screen.len() {
        for b in a + 1..on_screen.len() {
            let (ia, va) = on_screen[a];
            let (ib, vb) = on_screen[b];
            let (pa, pb, axis) = if vertical {
                (va[1], vb[1], "y")
            } else {
                (va[0], vb[0], "x")
            };
            assert!(
                (pa - pb).abs() > 3,
                "{ctx}: windows {ia} and {ib} should be laid out along {axis}: expected different {axis} positions, observed both at {pa} (rects {va:?} / {vb:?})"
            );
        }
    }
}

/// Drag on the primary monitor with real mouse input. Shared body of the monitor tests.
fn monitor_roundtrip(mon: usize, vertical: bool, label: &str) {
    aware();
    let kind = if vertical { "portrait" } else { "landscape" };
    let ms = mons();
    let (_, wr) = ms[mon];
    let (_, pw) = ms[0];
    // Two residents on the target monitor, created before the daemon so they are tiled at startup.
    let res_a = Sentinel::at(0, 0, wr.left + 100, wr.top + 100);
    let res_b = Sentinel::at(1, 0, wr.left + 500, wr.top + 500);
    // The mover and one more window on the primary monitor.
    let mover = Sentinel::new(2, 0);
    let other = Sentinel::new(3, 0);
    let mut d = Daemon::new(label);
    let mut m = Mouse::begin();
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "drag daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));

    let res: [(usize, &Sentinel); 2] = [(0, &res_a), (1, &res_b)];
    check_layout(
        &res,
        wr,
        vertical,
        &format!("{kind} pre-drag residents on monitor {mon}"),
    );
    assert_all_reachable(&res, &format!("{kind} pre-drag residents on monitor {mon}"));
    check_layout(
        &[(2, &mover), (3, &other)],
        pw,
        false,
        "primary pre-drag (horizontal)",
    );

    // Drag window 2 from the primary onto the target monitor (drop at its outer edge => Before, no stacking).
    let from = grab_raised(&mover).unwrap_or_else(|| {
        panic!(
            "{kind}: window 2 has no on-screen caption on the primary (visible rect {:?})",
            vis(&mover)
        )
    });
    let to = if vertical {
        ((wr.left + wr.right) / 2, wr.top + 5)
    } else {
        (wr.left + 5, (wr.top + wr.bottom) / 2)
    };
    assert!(
        on_any_monitor(to),
        "{kind}: drop point {to:?} on monitor {mon} is off-screen"
    );
    m.drag(from, to);
    thread::sleep(SETTLE);
    let landed = eventually(4, || monitor_of(&mover) == Some(mon));
    assert!(
        landed,
        "drag window 2 from primary to {kind} monitor {mon}: expected window 2 on monitor {mon}, observed monitor {:?} (visible rect {:?})",
        monitor_of(&mover),
        vis(&mover)
    );
    let all: [(usize, &Sentinel); 3] = [(0, &res_a), (1, &res_b), (2, &mover)];
    check_layout(
        &all,
        wr,
        vertical,
        &format!("after drop of window 2 onto {kind} monitor {mon}"),
    );
    assert!(
        reachable(&mover),
        "after drop onto {kind} monitor {mon}: window 2 expected reachable (revealed), observed {:.2} on screen (visible rect {:?})",
        on_screen_fraction(&mover),
        vis(&mover)
    );

    // Drag it back to the primary: the primary now holds only window 3, so drop on its left edge.
    let from = grab_raised(&mover).unwrap_or_else(|| {
        panic!(
            "{kind}: window 2 has no on-screen caption on monitor {mon} before the return drag (visible rect {:?})",
            vis(&mover)
        )
    });
    let to = (pw.left + 5, (pw.top + pw.bottom) / 2);
    m.drag(from, to);
    thread::sleep(SETTLE);
    let back = eventually(4, || monitor_of(&mover) == Some(0));
    assert!(
        back,
        "drag window 2 from {kind} monitor {mon} back to primary: expected monitor 0, observed {:?} (visible rect {:?})",
        monitor_of(&mover),
        vis(&mover)
    );
    check_layout(
        &[(2, &mover), (3, &other)],
        pw,
        false,
        "after drag back to primary (horizontal again)",
    );
    check_layout(
        &res,
        wr,
        vertical,
        &format!("residents of {kind} monitor {mon} after window 2 left"),
    );
    assert_all_reachable(
        &[(0, &res_a), (1, &res_b), (2, &mover), (3, &other)],
        "after drag back to primary",
    );
    d.stop();
}

#[test]
#[ignore]
fn reorder_before_after() {
    aware();
    // Two windows, not three: after Alt+Left the strip shows exactly two tiles, so both can be grabbed.
    // A window outside the viewport is parked at the park rect and has no caption on screen.
    let ws: Vec<Sentinel> = (0..2).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("s4-reorder");
    let mut m = Mouse::begin();
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "drag daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    // Focus window 0 so the strip scrolls to the start; a second press is a no-op at the strip start.
    for _ in 0..2 {
        press(&[ALT], LEFT);
    }
    let both: [(usize, &Sentinel); 2] = [(0, &ws[0]), (1, &ws[1])];
    assert_all_reachable(&both, "precondition (reorder, start)");
    let start = order(&ws);
    assert_eq!(
        start,
        vec![0, 1],
        "precondition: strip order must follow creation order; expected [0, 1], observed {start:?} ({})",
        describe(&ws)
    );

    // Phase A (Before): window 1 dropped on the outer 25% left edge of window 0 => [1, 0].
    let from = grab_raised(&ws[1]).unwrap_or_else(|| {
        panic!(
            "phase A precondition: window 1 has no on-screen caption to grab; {}",
            describe(&ws)
        )
    });
    let to = left_point(&ws[0], 0.10);
    assert!(
        on_any_monitor(to),
        "phase A precondition: left 10% of window 0 at {to:?} is off-screen, cannot hover it; {}",
        describe(&ws)
    );
    m.drag(from, to);
    thread::sleep(SETTLE);
    let o = order(&ws);
    assert_eq!(
        o,
        vec![1, 0],
        "phase A: drag window 1 over the outer 25% left edge of window 0 and drop (Before); expected strip order [1, 0], observed {o:?}; {}",
        describe(&ws)
    );

    // Phase B (After): window 1 (now the left tile) dropped on the outer 25% right edge of window 0 => [0, 1].
    assert_all_reachable(&both, "precondition (reorder, phase B)");
    let from = grab_raised(&ws[1]).unwrap_or_else(|| {
        panic!(
            "phase B precondition: window 1 has no on-screen caption to grab; {}",
            describe(&ws)
        )
    });
    let to = right_point(&ws[0], 0.10);
    assert!(
        on_any_monitor(to),
        "phase B precondition: right 10% of window 0 at {to:?} is off-screen, cannot hover it; {}",
        describe(&ws)
    );
    m.drag(from, to);
    thread::sleep(SETTLE);
    let o = order(&ws);
    assert_eq!(
        o,
        vec![0, 1],
        "phase B: drag window 1 over the outer 25% right edge of window 0 and drop (After); expected strip order [0, 1], observed {o:?}; {}",
        describe(&ws)
    );
    d.stop();
}

#[test]
#[ignore]
fn drop_on_center_stacks() {
    aware();
    let ws: Vec<Sentinel> = (0..2).map(|i| Sentinel::new(i, 0)).collect();
    let cap = Capture::open(0).expect("desktop duplication for the stacked border colour check");
    let mut d = Daemon::new("s4-stack");
    let mut m = Mouse::begin();
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "drag daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    let from = grab_raised(&ws[1]).unwrap_or_else(|| {
        panic!(
            "precondition: window 1 has no on-screen caption to grab; {}",
            describe(&ws)
        )
    });
    // Centre of window 0 is the middle 50% of its width: hover there for HOVER, then drop.
    let to = centre(vis(&ws[0]));
    // Instrumentation: the dragged window must follow the cursor before the drop. If it does not, the drag
    // never reached the window/daemon and the stack failure below says nothing about stacking.
    let start = vis(&ws[1]);
    m.down(from);
    m.glide(from, to);
    // The Windows 11 caption-drag proxy updates the real window a moment after the cursor settles.
    thread::sleep(Duration::from_millis(250));
    let mid = vis(&ws[1]);
    let moved = (mid[0] - start[0]).abs().max((mid[1] - start[1]).abs());
    println!(
        "drop_on_center_stacks: window 1 rect at press {start:?}, mid-drag {mid:?}, cursor {to:?}, moved {moved} px"
    );
    assert!(
        moved > 100,
        "drop_on_center_stacks: dragged window 1 must follow the cursor before the drop: expected a move > 100 px toward window 0, observed {moved} px (rect at press {start:?}, mid-drag {mid:?}, cursor {to:?}); the drag did not reach the window"
    );
    // Hover for HOVER with +-2 px jitter every 50 ms: input keeps flowing, velocity stays under 300 px/s.
    for i in 0..HOVER.as_millis() as u64 / 50 {
        let dx: i32 = if i % 2 == 0 { 2 } else { -2 };
        m.put((to.0 + dx, to.1));
        thread::sleep(Duration::from_millis(50));
    }
    let held = vis(&ws[1]);
    println!(
        "drop_on_center_stacks: after {} ms hover window 1 rect {held:?}, window 0 rect {:?}",
        HOVER.as_millis(),
        vis(&ws[0])
    );
    // #113: after the dwell, pressing the configured group modifier (Shift) activates group mode; a plain
    // release would tile beside the neighbour instead of stacking.
    m.shift(true);
    thread::sleep(Duration::from_millis(150));
    m.up();
    m.shift(false);
    thread::sleep(SETTLE);
    println!("drop_on_center_stacks: after drop {}", describe(&ws));
    let (r0, r1) = (reachable(&ws[0]), reachable(&ws[1]));
    assert!(
        r0 ^ r1,
        "drop of window 1 on the centre of window 0 held {} ms: expected one stack (exactly one of windows 0/1 reachable, the other a stack peek); observed reachable w0={r0} w1={r1}; {}",
        HOVER.as_millis(),
        describe(&ws)
    );
    let seen = eventually(4, || count_rgb(&cap, STACKED_RGB) > 0);
    assert!(
        seen,
        "stacked column: expected border colour #A855F7 on screen; not found in 4 s of captured frames; {}",
        describe(&ws)
    );
    d.stop();
}

#[test]
#[ignore]
fn shift_freezes_reflow() {
    aware();
    let ws: Vec<Sentinel> = (0..3).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("s4-shift");
    let mut m = Mouse::begin();
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "drag daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    for _ in 0..3 {
        press(&[ALT], LEFT);
    }
    let from = grab_raised(&ws[1]).unwrap_or_else(|| {
        panic!(
            "precondition: window 1 has no on-screen caption to grab; {}",
            describe(&ws)
        )
    });
    let to = left_point(&ws[0], 0.10);
    assert!(
        on_any_monitor(to),
        "precondition: left 10% of window 0 at {to:?} is off-screen; {}",
        describe(&ws)
    );
    let neighbours = [0usize, 2];
    let before: Vec<[i32; 4]> = neighbours.iter().map(|&i| vis(&ws[i])).collect();
    // Shift held for the whole drag: reflow must stay frozen until the drop.
    m.shift(true);
    m.down(from);
    m.glide(from, to);
    for sample in 0..3 {
        thread::sleep(Duration::from_millis(200));
        for (k, &i) in neighbours.iter().enumerate() {
            let now = vis(&ws[i]);
            let moved = (0..4).any(|c| (now[c] - before[k][c]).abs() > 2);
            assert!(
                !moved,
                "shift_freezes_reflow: Shift held while dragging window 1 over the left edge of window 0; neighbour window {i} moved before the drop (sample {sample}, ~{} ms into the hold): expected {:?} (+-2px), observed {:?}",
                (sample + 1) * 200,
                before[k],
                now
            );
        }
    }
    m.up();
    m.shift(false);
    thread::sleep(SETTLE);
    d.stop();
}

#[test]
#[ignore]
fn drag_out_of_stack() {
    aware();
    let ws: Vec<Sentinel> = (0..3).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("s4-unstack");
    let mut m = Mouse::begin();
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "drag daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    for _ in 0..3 {
        press(&[ALT], LEFT);
    }
    // Setup: window 1 dropped on the centre of window 0 to form a stack.
    let from = grab_raised(&ws[1]).unwrap_or_else(|| {
        panic!(
            "setup precondition: window 1 has no on-screen caption to grab; {}",
            describe(&ws)
        )
    });
    // Setup (#113): drag window 1 onto the middle of window 0, dwell, then press the group modifier (Shift).
    let to_centre = centre(vis(&ws[0]));
    m.down(from);
    m.glide(from, to_centre);
    thread::sleep(HOVER);
    m.shift(true);
    thread::sleep(Duration::from_millis(150));
    m.up();
    m.shift(false);
    thread::sleep(SETTLE);
    let r = [reachable(&ws[0]), reachable(&ws[1])];
    assert!(
        r[0] ^ r[1],
        "setup: expected windows 0 and 1 to form one stack (exactly one reachable); observed reachable w0={} w1={}; {}",
        r[0],
        r[1],
        describe(&ws)
    );
    let active = if r[0] { 0 } else { 1 };

    // Drag the stack's active member out, dropping it on the outer 25% left edge of window 2 (Before).
    let from = grab_raised(&ws[active]).unwrap_or_else(|| {
        panic!(
            "precondition: stack member {active} has no on-screen caption; {}",
            describe(&ws)
        )
    });
    // #115: pull the active member sideways by more than 1/4 of the neighbour span: it leaves the stack.
    let r = vis(&ws[active]);
    // Threshold is 1/4 of the neighbour's visible span: pull 60% of the widest visible window (never marginal).
    let widest = (0..3)
        .map(|i| {
            let v = vis(&ws[i]);
            v[2] - v[0]
        })
        .max()
        .unwrap_or(r[2] - r[0]);
    let pull = (widest * 3 / 5).max(1);
    let to = (from.0 + pull, from.1);
    assert!(
        on_any_monitor(to),
        "precondition: pull target {to:?} is off-screen; {}",
        describe(&ws)
    );
    m.drag(from, to);
    thread::sleep(SETTLE);

    let (_, work) = mons()[0];
    let wh = work.bottom - work.top;
    for i in 0..3 {
        let h = height(vis(&ws[i]));
        assert!(
            (h - wh).abs() <= 3,
            "drag_out_of_stack: after auto-unwrap window {i} should be a normal full-height tile: expected height {wh} (+-3px), observed {h} (visible rect {:?}); {}",
            vis(&ws[i]),
            describe(&ws)
        );
    }
    let focused = eventually(3, || fg_index(&ws) == Some(active));
    assert!(
        focused,
        "drag_out_of_stack: focus should follow the dragged window {active}: expected foreground window {active}, observed {:?}; {}",
        fg_index(&ws),
        describe(&ws)
    );
    d.stop();
}

#[test]
#[ignore]
fn drag_to_portrait_monitor() {
    aware();
    let ms = mons();
    let Some(mon) = (1..ms.len()).find(|&i| is_portrait(&ms[i].0)) else {
        println!("SKIPPED_NO_PORTRAIT_MONITOR");
        return;
    };
    monitor_roundtrip(mon, true, "s4-portrait");
}

#[test]
#[ignore]
fn drag_to_other_landscape_monitor() {
    aware();
    let ms = mons();
    let Some(mon) = (1..ms.len()).find(|&i| !is_portrait(&ms[i].0)) else {
        println!("SKIPPED_NO_SECOND_LANDSCAPE_MONITOR");
        return;
    };
    // With PLANOR_RECORD=1 the scenario is screen-recorded and the recording is checked against the pixels;
    // otherwise `rec` is inert and nothing changes.
    let rec = Recording::start("drag_to_other_landscape_monitor", "s4-landscape");
    monitor_roundtrip(mon, false, "s4-landscape");
    assert_recording_clean(&rec);
}

#[test]
#[ignore]
fn drag_cancel_escape() {
    aware();
    let ws: Vec<Sentinel> = (0..3).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("s4-escape");
    let mut m = Mouse::begin();
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "drag daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    for _ in 0..3 {
        press(&[ALT], LEFT);
    }
    let before: Vec<[i32; 4]> = (0..3).map(|i| vis(&ws[i])).collect();
    let before_order = order(&ws);
    let from = grab_raised(&ws[1]).unwrap_or_else(|| {
        panic!(
            "precondition: window 1 has no on-screen caption to grab; {}",
            describe(&ws)
        )
    });
    let to = left_point(&ws[0], 0.10);
    assert!(
        on_any_monitor(to),
        "precondition: left 10% of window 0 at {to:?} is off-screen; {}",
        describe(&ws)
    );
    // Reorder preview is active over window 0, then Escape mid-drag must cancel it.
    m.down(from);
    m.glide(from, to);
    thread::sleep(HOLD);
    m.escape();
    thread::sleep(Duration::from_millis(300));
    m.up();
    thread::sleep(SETTLE);
    for i in 0..3 {
        let now = vis(&ws[i]);
        let moved = (0..4).any(|c| (now[c] - before[i][c]).abs() > 3);
        assert!(
            !moved,
            "drag_cancel_escape: Escape mid-drag must return window {i} to its original column: expected {:?} (+-3px), observed {:?}; {}",
            before[i],
            now,
            describe(&ws)
        );
    }
    let after = order(&ws);
    assert_eq!(
        after, before_order,
        "drag_cancel_escape: strip order after Escape should be unchanged: expected {before_order:?}, observed {after:?}"
    );
    d.stop();
}

#[test]
#[ignore]
fn resize_by_edge_drag() {
    aware();
    // Two windows: both are on screen after Alt+Left, so their geometry can be measured. A parked window
    // reports the park rect, so offsets against it would be meaningless.
    let ws: Vec<Sentinel> = (0..2).map(|i| Sentinel::new(i, 0)).collect();
    let cap = Capture::open(0).expect("desktop duplication for the pixel push check");
    let mut d = Daemon::new("s4-resize");
    let mut m = Mouse::begin();
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "drag daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    for _ in 0..2 {
        press(&[ALT], LEFT);
    }
    let both: [(usize, &Sentinel); 2] = [(0, &ws[0]), (1, &ws[1])];
    assert_all_reachable(&both, "precondition (resize, start)");

    // Part 1: drag the right edge of window 0 by +200 px; window 1 must be pushed by 200 px.
    // The right resize border of window 0 is the grab point (rect right - 4).
    let r_idx = 0usize;
    let later = 1usize;
    let edge_of = |i: usize| {
        let r = ws[i].rect();
        (r[2] - 4, (r[1] + r[3]) / 2)
    };
    let edge = edge_of(r_idx);
    assert!(
        on_any_monitor(edge),
        "precondition: right resize edge of window {r_idx} at {edge:?} is off-screen; {}",
        describe(&ws)
    );
    let before: Vec<[i32; 4]> = (0..2).map(|i| vis(&ws[i])).collect();
    // Calibration, before the drag while window 1 is fully visible and unfocused: the unfocused client inset is the
    // colour box's left edge minus the window's visible left edge.
    let (_, pre_cyan) = colour_box(&cap, later);
    let pre_left = pre_cyan
        .unwrap_or_else(|| {
            panic!(
                "calibration: window {later} colour absent before the drag (visible rect {:?}); {}",
                before[later],
                describe(&ws)
            )
        })
        .0;
    let inset = pre_left - before[later][0];
    assert!(
        (1..=3).contains(&inset),
        "calibration: window {later} unfocused client inset expected 1..=3 px (colour box left {pre_left} minus visible left {}), observed {inset}; {}",
        before[later][0],
        describe(&ws)
    );
    m.drag(edge, (edge.0 + 200, edge.1));
    thread::sleep(SETTLE);
    let grew = width(vis(&ws[r_idx])) - width(before[r_idx]);
    assert!(
        (grew - 200).abs() <= 3,
        "resize_by_edge_drag: right-edge drag +200 px on window {r_idx}: expected width {} (+-3px), observed {} (+{grew}); {}",
        width(before[r_idx]) + 200,
        width(vis(&ws[r_idx])),
        describe(&ws)
    );

    // Push check, measured in one frame: reveal the later window (Alt+Right moves focus to it and scrolls it
    // into view), then read the resized window's right edge and the later window's left edge together.
    // Pushed means the gap between them is the normal gap; an unpushed neighbour would overlap by ~200 px.
    // Push check WITHOUT revealing: window 0 keeps focus, so window 1 stays the next column. It is only a clipped
    // DWM-thumbnail peek on the right output edge and its real rect is parked, so it is measured in pixels.
    assert!(
        fg_index(&ws) == Some(r_idx),
        "resize_by_edge_drag: window {r_idx} must keep focus after the resize: expected foreground window {r_idx}, observed {:?}; {}",
        fg_index(&ws),
        describe(&ws)
    );
    let a = vis(&ws[r_idx]);
    assert!(
        on_any_monitor(((a[2] - 1), (a[1] + a[3]) / 2)),
        "resize_by_edge_drag: right edge of window {r_idx} is off-screen, cannot measure the push; rect {a:?}"
    );
    let gap_expected = GAP_PX as i32; // product applies gap_px as raw device pixels
    let (out_w, cyan) = colour_box(&cap, later);
    let (cl, _, cr, _) = cyan.unwrap_or_else(|| {
        panic!(
            "resize_by_edge_drag: window {later} (sentinel colour absent from captured frames; expected its peek at the right output edge); rect {a:?}; {}",
            describe(&ws)
        )
    });
    let expected_left = a[2] + gap_expected + inset;
    assert!(
        (cl - expected_left).abs() <= 3,
        "resize_by_edge_drag: later window {later} should be pushed by the +200 px resize of window {r_idx}: expected colour box left {expected_left} px (window {r_idx} right edge {} + gap {gap_expected} + inset {inset}, +-3px), observed {cl} px (box {cyan:?}); a window that was not pushed would overlap by ~200 px",
        a[2]
    );
    assert!(
        cr >= out_w - 3,
        "resize_by_edge_drag: window {later} must be clipped by the right output edge (pushed, not overlapped): expected colour box right >= {} (+-3px), observed {cr} (output width {out_w}, box {cyan:?})",
        out_w - 3
    );

    // Part 2: the leading (left) edge drag on the first column must be ignored. Window 0 is the first column, so its
    // leading border is the grab point. That border is invisible and starts at the window rect's left (8 px left of
    // the visible edge at 100 %, 16 px at 200 %), so `rect left + 3` lies on whatever is left of the window: another
    // monitor on the three-monitor rig, nothing at all on a single monitor. Take the first column from there through
    // two columns past the visible edge (the range follows the real border width, whatever the DPI) where a real
    // press lands on window 0 itself and grabs its HTLEFT border: on a single monitor the visible edge column, x = 0.
    // "Ignored" then cannot pass on a client-area drag, nor on a press another window intercepts.
    let lead_of = |i: usize| {
        let r = ws[i].rect();
        let v = vis(&ws[i]);
        let y = (r[1] + r[3]) / 2;
        ((r[0] + 3).min(v[0])..=v[0] + 2)
            .map(|x| (x, y))
            .find(|&p| grabs_leading_border(&ws[i], p))
    };
    let l_idx = r_idx;
    let edge = lead_of(l_idx).unwrap_or_else(|| {
        panic!(
            "precondition: no on-screen point where a press lands on window {l_idx} and grabs its leading (HTLEFT) resize border; rect {:?}; {}",
            ws[l_idx].rect(),
            describe(&ws)
        )
    });
    eprintln!(
        "resize_by_edge_drag: leading-edge grab at {edge:?} (window rect {:?})",
        ws[l_idx].rect()
    );
    let mid: Vec<[i32; 4]> = (0..2).map(|i| vis(&ws[i])).collect();
    assert!(
        grabs_leading_border(&ws[l_idx], edge),
        "precondition: right before the press, {edge:?} no longer lands on window {l_idx}'s leading (HTLEFT) border (window under it: {:#x}); {}",
        root_at(edge),
        describe(&ws)
    );
    m.drag(edge, (edge.0 + 100, edge.1));
    thread::sleep(SETTLE);
    for i in 0..2 {
        let now = vis(&ws[i]);
        let moved = (0..4).any(|c| (now[c] - mid[i][c]).abs() > 3);
        assert!(
            !moved,
            "resize_by_edge_drag: leading (left) edge drag of window {l_idx} must be ignored: window {i} expected {:?} (+-3px), observed {:?}; {}",
            mid[i],
            now,
            describe(&ws)
        );
    }
    d.stop();
}

/// #116 (landscape): holding a dragged window in the trailing edge zone scrolls the strip.
#[test]
#[ignore]
fn drag_to_edge_scrolls_landscape() {
    aware();
    let ws: Vec<Sentinel> = (0..6).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("s4-edge");
    let mut m = Mouse::begin();
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "drag daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    for _ in 0..6 {
        press(&[ALT], LEFT);
    }
    let sum = |ws: &[Sentinel]| ws[1..].iter().map(|w| vis(w)[0] as i64).sum::<i64>();
    let before = sum(&ws);
    let from = grab_raised(&ws[0]).expect("window 0 caption");
    let (_, work) = mons()[0];
    let to = (work.right - 4, from.1);
    m.down(from);
    m.glide(from, to);
    thread::sleep(Duration::from_millis(1200));
    let during = sum(&ws);
    m.escape();
    m.up();
    assert!(
        during != before,
        "drag_to_edge_scrolls_landscape: strip did not scroll while the cursor rested in the edge zone; before={before} during={during}; {}",
        describe(&ws)
    );
}

/// Owner item 4: a mouse pull ACROSS the strip (down, in landscape) of a quarter of the column depth takes a
/// window out of its vertical group.
#[test]
#[ignore]
fn drag_out_of_vertical_group_by_pulling_down() {
    aware();
    let ws: Vec<Sentinel> = (0..3).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("s4-vexit");
    let mut m = Mouse::begin();
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "drag daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    for _ in 0..3 {
        press(&[ALT], LEFT);
    }
    // Group windows 0 and 1 with the modifier, as in drag_out_of_stack.
    let from = grab_raised(&ws[1]).expect("window 1 caption");
    let to_centre = centre(vis(&ws[0]));
    m.down(from);
    m.glide(from, to_centre);
    thread::sleep(HOVER);
    m.shift(true);
    thread::sleep(Duration::from_millis(150));
    m.up();
    m.shift(false);
    thread::sleep(SETTLE);
    let reach = |w: &Sentinel| reachable(w);
    let grouped = reach(&ws[0]) ^ reach(&ws[1]);
    assert!(
        grouped,
        "setup: windows 0 and 1 must form a stack; {}",
        describe(&ws)
    );
    let active = if reach(&ws[0]) { 0 } else { 1 };
    let from = grab_raised(&ws[active]).expect("stack member caption");
    let (_, work) = mons()[0];
    let pull = (work.bottom - work.top) * 2 / 5;
    let to = (from.0, from.1 + pull);
    assert!(
        on_any_monitor(to),
        "pull target {to:?} off-screen; {}",
        describe(&ws)
    );
    m.drag(from, to);
    thread::sleep(SETTLE);
    let wh = work.bottom - work.top;
    for i in 0..3 {
        let h = height(vis(&ws[i]));
        assert!(
            (h - wh).abs() <= 3,
            "drag_out_of_vertical_group: after the exit window {i} should be a full-height tile: expected {wh} (+-3), observed {h}; {}",
            describe(&ws)
        );
    }
}

/// Owner item 3: while a window is resized by its edge it stays above its neighbours: the pixel at the middle
/// of the grown window shows its own colour, not a neighbour's stand-in.
#[test]
#[ignore]
fn resized_window_stays_above_neighbours() {
    aware();
    let ws: Vec<Sentinel> = (0..2).map(|i| Sentinel::new(i, 0)).collect();
    let cap = Capture::open(0).expect("desktop duplication");
    let mut d = Daemon::new("s4-resize-top");
    let mut m = Mouse::begin();
    d.start();
    assert!(
        wait_settled(&d, 1, Duration::from_millis(2500)),
        "drag daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(120));
    for _ in 0..2 {
        press(&[ALT], LEFT);
    }
    let r = ws[0].rect();
    let edge = (r[2] - 4, (r[1] + r[3]) / 2);
    m.down(edge);
    m.glide(edge, (edge.0 + 250, edge.1));
    thread::sleep(Duration::from_millis(300));
    // Mid-resize (button still down): window 0's own colour must still dominate its grown area.
    let own = count_rgb(&cap, SENTINEL_RGB[0]);
    let other = count_rgb(&cap, SENTINEL_RGB[1]);
    m.up();
    assert!(
        own > other,
        "resized_window_stays_above_neighbours: during the resize window 0's colour ({own} px) must exceed window 1's ({other} px)"
    );
}

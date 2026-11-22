#![allow(dead_code, unused_imports)]
include!("common/helpers.rs");

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, mouse_event,
};

/// Tolerance used for "restored" rect comparisons after a round trip (spec: +-3 px).
const ROUND_TRIP_TOL: i32 = 3;
/// Tolerance used for "restored" rect comparisons after kill/restart (S3 semantics: +-1 px).
const RESTART_TOL: i32 = 1;

/// Minimized (iconic) state of a sentinel.
fn iconic(w: &Sentinel) -> bool {
    unsafe { IsIconic(w.hwnd as _) != 0 }
}

/// True while the daemon child process is still running.
fn is_alive(d: &mut Daemon) -> bool {
    d.p.as_mut().map_or(false, |p| p.try_wait().unwrap().is_none())
}

/// One sentinel per monitor (physical coords inside each monitor rect) plus two extras on the primary.
/// Indices 0..n are the per-monitor sentinels, n and n+1 are the extras (creation order = strip order).
fn build_sentinels(m: &[(RECT, RECT)]) -> Vec<Sentinel> {
    let mut ws = Vec::new();
    for (i, (rc, _)) in m.iter().enumerate() {
        ws.push(Sentinel::at(i, 0, rc.left + 100, rc.top + 100));
    }
    let p = m[0].0;
    let n = m.len();
    for k in 0..2usize {
        ws.push(Sentinel::at(n + k, 0, p.left + 600 + 400 * k as i32, p.top + 100));
    }
    ws
}

/// The monitor a sentinel was created on (its "original" monitor): index i for the per-monitor
/// sentinels, the primary (0) for the extras. Indices refer to the pre-outage `mons()` order.
fn origin_of(i: usize, n: usize) -> usize {
    if i < n { i } else { 0 }
}

fn rect_list(m: &[(RECT, RECT)]) -> Vec<[i32; 4]> {
    m.iter()
        .map(|(r, _)| [r.left, r.top, r.right, r.bottom])
        .collect()
}

/// True when the monitor that was `origin` before the outage is no longer attached.
fn home_gone(m0: &[(RECT, RECT)], origin: usize) -> bool {
    let want = rect_list(m0)[origin];
    !rect_list(&mons()).contains(&want)
}

/// The user-centric "can be brought on screen" invariant.
/// A minimized window is NOT revealable (restoring it by hand is what the user must not need to do;
/// only `planor pull-orphans` may restore it). Otherwise a bare Alt tap plus SetForegroundWindow must
/// make the window reachable within 2 s, which the daemon does by scrolling it into view.
fn can_reveal(w: &Sentinel) -> bool {
    if iconic(w) {
        return false;
    }
    press(&[], ALT);
    unsafe { SetForegroundWindow(w.hwnd as _) };
    eventually(2, || reachable(w))
}

/// Assert the monitor layout equals the recorded one, waiting briefly for the display to settle.
fn assert_layout_restored(before: &[(RECT, RECT)]) {
    let want = rect_list(before);
    let settled = eventually(10, || rect_list(&mons()) == want);
    let got = rect_list(&mons());
    println!("display layout before: {want:?}");
    println!("display layout after restore: {got:?}");
    assert!(
        settled && got == want,
        "display layout after /extend: expected identical monitor rects {want:?}, observed {got:?}"
    );
}

/// Switch to internal-only and wait for the topology to show a single monitor.
/// The guard is created before the assertion so /extend runs on unwind.
fn go_internal() -> DisplayGuard {
    let guard = DisplayGuard::internal_only();
    assert!(
        eventually(10, || mons().len() == 1),
        "after DisplaySwitch /internal: expected 1 monitor, observed {}",
        mons().len()
    );
    guard
}

/// Restore the extended layout and wait until the original monitor count is back, then settle 6 s.
fn restore_displays(guard: DisplayGuard, count: usize) {
    drop(guard);
    assert!(
        eventually(10, || mons().len() == count),
        "after DisplaySwitch /extend: expected {count} monitors, observed {}",
        mons().len()
    );
    thread::sleep(Duration::from_secs(6));
}

/// Shared body for offline_15s_round_trip and short_blip_3s.
fn offline_round_trip(label: &str, offline_secs: u64) {
    let m0 = mons();
    if m0.len() < 2 {
        println!("SKIPPED_SINGLE_MONITOR");
        return;
    }
    let n = m0.len();
    let ws = build_sentinels(&m0);
    let mut d = Daemon::new(label);
    d.start();
    thread::sleep(Duration::from_millis(1500));
    let rects: Vec<[i32; 4]> = ws.iter().map(|w| w.rect()).collect();
    let home: Vec<Option<usize>> = ws.iter().map(monitor_of).collect();
    let reach0: Vec<bool> = ws.iter().map(reachable).collect();
    let origin: Vec<usize> = (0..ws.len()).map(|i| origin_of(i, n)).collect();
    println!("settled rects: {rects:?}");
    println!("home monitors: {home:?}");
    println!("settled reachable: {reach0:?}");
    println!("original monitors: {origin:?}");

    let guard = go_internal();
    thread::sleep(Duration::from_secs(offline_secs));

    // Windows whose home monitor is gone: minimized or reachable on the remaining monitor.
    let gone: Vec<bool> = origin.iter().map(|&o| home_gone(&m0, o)).collect();
    for (i, w) in ws.iter().enumerate().filter(|(i, _)| gone[*i]) {
        let (ic, re) = (iconic(w), reachable(w));
        assert!(
            ic || re,
            "window {i}: home monitor {} is gone; expected minimized or reachable on the remaining monitor after {offline_secs}s offline; observed iconic={ic} reachable={re} rect={:?}",
            origin[i],
            w.rect()
        );
    }
    // Windows whose home monitor remains: must be revealable (Alt tap + focus brings them on screen).
    for (i, w) in ws.iter().enumerate().filter(|(i, _)| !gone[*i]) {
        let ok = can_reveal(w);
        assert!(
            ok,
            "window {i}: home monitor {} remains attached during the {offline_secs}s outage; expected revealable (not minimized, reachable within 2 s of Alt tap + focus); observed iconic={} reachable={} rect={:?}",
            origin[i],
            iconic(w),
            reachable(w),
            w.rect()
        );
    }

    // Windows of the primary stay tiled and focusable.
    let prim: Vec<usize> = (0..ws.len()).filter(|&i| home[i] == Some(0)).collect();
    assert!(
        prim.len() >= 2,
        "expected at least 2 windows on the primary monitor, home monitors {home:?}"
    );
    unsafe { SetForegroundWindow(ws[prim[0]].hwnd as _) };
    assert!(
        eventually(3, || fg_index(&ws) == Some(prim[0])),
        "focus during outage: expected window {} foreground, observed fg index {:?}",
        prim[0],
        fg_index(&ws)
    );
    press(&[ALT], RIGHT);
    let f = fg_index(&ws);
    assert!(
        f == Some(prim[1]) && reachable(&ws[prim[1]]),
        "Alt+Right on primary during outage: expected window {} focused and reachable, observed fg index {:?}",
        prim[1],
        f
    );
    press(&[ALT], LEFT);
    let f = fg_index(&ws);
    assert!(
        f == Some(prim[0]) && reachable(&ws[prim[0]]),
        "Alt+Left on primary during outage: expected window {} focused and reachable, observed fg index {:?}",
        prim[0],
        f
    );

    // Daemon still responds.
    assert!(
        is_alive(&mut d) && d.call("reload"),
        "daemon during outage: expected to respond to reload, observed no response (process alive: {})",
        is_alive(&mut d)
    );

    // Manual rescue: pull-orphans moves every orphan onto the active (remaining) monitor.
    assert!(
        d.call("pull-orphans"),
        "planor pull-orphans during outage: expected exit 0, observed failure"
    );
    // Only windows whose home monitor is gone are orphans. Windows of a remaining monitor that are merely
    // scrolled out of view are parked by design, so they are checked by can_reveal below, not here.
    eventually(5, || {
        ws.iter()
            .enumerate()
            .filter(|(i, _)| gone[*i])
            .all(|(_, w)| !iconic(w) && reachable(w) && monitor_of(w) == Some(0))
    });
    for (i, w) in ws.iter().enumerate().filter(|(i, _)| gone[*i]) {
        let (ic, mo) = (iconic(w), monitor_of(w));
        assert!(
            !ic && mo == Some(0),
            "after pull-orphans window {i} (home monitor {} is gone): expected not minimized and on the remaining monitor (monitor 0); observed iconic={ic} monitor={mo:?} rect={:?}",
            origin[i],
            w.rect()
        );
    }
    for (i, w) in ws.iter().enumerate() {
        let ok = can_reveal(w);
        assert!(
            ok,
            "after pull-orphans window {i}: expected can_reveal (not minimized, reachable within 2 s of Alt tap + focus); observed iconic={} reachable={} rect={:?}",
            iconic(w),
            reachable(w),
            w.rect()
        );
    }

    // Restore the extended layout and check every window came back.
    restore_displays(guard, n);
    assert_layout_restored(&m0);
    // Pass 1 (no focus changes): sentinels reachable before the outage are back in place on their
    // original monitor, at their settled rect.
    for (i, w) in ws.iter().enumerate().filter(|(i, _)| reach0[*i]) {
        let r = w.rect();
        let (re, mo) = (reachable(w), monitor_of(w));
        assert!(
            re && mo == Some(origin[i]),
            "window {i} after /extend: expected reachable on original monitor {} (reachable before the outage), observed reachable={re} monitor={mo:?} rect={r:?}",
            origin[i]
        );
        for k in 0..4 {
            assert!(
                (r[k] - rects[i][k]).abs() <= ROUND_TRIP_TOL,
                "window {i} after /extend: expected settled rect {:?} (+-{ROUND_TRIP_TOL}px), observed {r:?}",
                rects[i]
            );
        }
    }
    // Pass 2: every other sentinel can be revealed and ends up on its original monitor.
    for (i, w) in ws.iter().enumerate().filter(|(i, _)| !reach0[*i]) {
        let ok = can_reveal(w);
        let mo = monitor_of(w);
        assert!(
            ok && mo == Some(origin[i]),
            "window {i} after /extend: expected can_reveal and on original monitor {}, observed can_reveal={ok} monitor={mo:?} iconic={} rect={:?}",
            origin[i],
            iconic(w),
            w.rect()
        );
    }
    d.stop();
}

#[test]
#[ignore]
fn offline_15s_round_trip() {
    aware();
    offline_round_trip("s5-15s", 15);
}

#[test]
#[ignore]
fn short_blip_3s() {
    aware();
    offline_round_trip("s5-3s", 3);
}

#[test]
#[ignore]
fn offline_then_stop() {
    aware();
    let m0 = mons();
    if m0.len() < 2 {
        println!("SKIPPED_SINGLE_MONITOR");
        return;
    }
    let ws = build_sentinels(&m0);
    let before: Vec<[i32; 4]> = ws.iter().map(|w| w.rect()).collect();
    let mut d = Daemon::new("s5-offstop");
    d.start();
    thread::sleep(Duration::from_millis(1500));

    let guard = go_internal();
    thread::sleep(Duration::from_secs(3));
    d.stop();
    restore_displays(guard, m0.len());
    assert_layout_restored(&m0);

    // Windows can be moved back by Windows itself when their monitor reconnects (the daemon has already
    // exited), so the exact rect is reported as a NOTE only. The strict contract is reachability, not
    // minimized, and the original monitor.
    let origin: Vec<usize> = (0..ws.len()).map(|i| origin_of(i, m0.len())).collect();
    for (i, w) in ws.iter().enumerate() {
        let r = w.rect();
        if r != before[i] {
            println!(
                "NOTE exact rect differs for window {i}: expected {:?} observed {r:?}",
                before[i]
            );
        }
        let (re, ic, mo) = (reachable(w), iconic(w), monitor_of(w));
        assert!(
            re && !ic && mo == Some(origin[i]),
            "window {i} after stop while offline and /extend: expected reachable, not minimized and on original monitor {}, observed reachable={re} iconic={ic} monitor={mo:?} rect={r:?}",
            origin[i]
        );
    }
}

#[test]
#[ignore]
fn offline_then_kill_recover() {
    aware();
    let m0 = mons();
    if m0.len() < 2 {
        println!("SKIPPED_SINGLE_MONITOR");
        return;
    }
    let ws = build_sentinels(&m0);
    let before: Vec<[i32; 4]> = ws.iter().map(|w| w.rect()).collect();
    let mut d = Daemon::new("s5-offkill");
    d.start();
    thread::sleep(Duration::from_millis(1500));

    let guard = go_internal();
    thread::sleep(Duration::from_secs(3));
    d.kill();
    restore_displays(guard, m0.len());
    assert_layout_restored(&m0);

    // Restart without re-tiling so the check measures recovery only (same as S3).
    let path = d.dir.join("planor/planor.toml");
    fs::write(
        &path,
        fs::read_to_string(&path).unwrap().replace(
            "tile_existing_on_startup = true",
            "tile_existing_on_startup = false",
        ),
    )
    .unwrap();
    d.start();
    thread::sleep(Duration::from_millis(1500));

    let restored = eventually(5, || {
        ws.iter().enumerate().all(|(i, w)| {
            let r = w.rect();
            reachable(w)
                && (0..4).all(|k| (r[k] - before[i][k]).abs() <= RESTART_TOL)
        })
    });
    for (i, w) in ws.iter().enumerate() {
        let r = w.rect();
        let re = reachable(w);
        assert!(
            restored && re && (0..4).all(|k| (r[k] - before[i][k]).abs() <= RESTART_TOL),
            "window {i} after kill while offline, /extend and restart: expected rect {:?} (+-{RESTART_TOL}px) and reachable within 5 s, observed {r:?} reachable={re}",
            before[i]
        );
    }
    d.stop();
}

/// Left button held down on a caption; releases the button and restores the cursor on drop,
/// so a panic mid-drag never leaves the mouse stuck down.
struct Drag {
    down: bool,
    cursor: POINT,
}
impl Drag {
    fn up(&mut self) {
        if self.down {
            unsafe { mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0) };
            self.down = false;
        }
    }
}
impl Drop for Drag {
    fn drop(&mut self) {
        self.up();
        unsafe { SetCursorPos(self.cursor.x, self.cursor.y) };
    }
}

#[test]
#[ignore]
fn topology_events_while_dragging() {
    aware();
    let m0 = mons();
    if m0.len() < 2 {
        println!("SKIPPED_SINGLE_MONITOR");
        return;
    }
    let n = m0.len();
    let ws = build_sentinels(&m0);
    let mut d = Daemon::new("s5-drag");
    d.start();
    thread::sleep(Duration::from_millis(1500));
    let home: Vec<Option<usize>> = ws.iter().map(monitor_of).collect();
    let origin: Vec<usize> = (0..ws.len()).map(|i| origin_of(i, n)).collect();
    let prim = (0..ws.len())
        .find(|&i| home[i] == Some(0))
        .expect("expected a window on the primary monitor");
    let r = ws[prim].rect();
    let start = POINT {
        x: (r[0] + r[2]) / 2,
        y: r[1] + 12,
    };

    let mut cursor = POINT::default();
    unsafe { GetCursorPos(&mut cursor) };
    let mut drag = Drag {
        down: false,
        cursor,
    };
    unsafe { SetCursorPos(start.x, start.y) };
    thread::sleep(Duration::from_millis(60));
    unsafe { mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0) };
    drag.down = true;

    let mut display: Option<DisplayGuard> = None;
    for step in 0..20 {
        unsafe { SetCursorPos(start.x + 10 * (step + 1), start.y) };
        thread::sleep(Duration::from_millis(15));
        if step == 10 {
            // Topology change while the button is held.
            display = Some(go_internal());
        }
    }
    thread::sleep(Duration::from_millis(300));

    drag.up();
    assert!(
        is_alive(&mut d),
        "daemon died during drag with topology change: expected process alive, observed exited"
    );
    // Windows whose home monitor is gone: minimized or reachable on the remaining monitor.
    let gone: Vec<bool> = origin.iter().map(|&o| home_gone(&m0, o)).collect();
    for (i, w) in ws.iter().enumerate().filter(|(i, _)| gone[*i]) {
        let (ic, re) = (iconic(w), reachable(w));
        assert!(
            ic || re,
            "window {i} after drag with topology change: home monitor {} is gone; expected minimized or reachable on the remaining monitor, observed iconic={ic} reachable={re} rect={:?}",
            origin[i],
            w.rect()
        );
    }
    // Windows whose home monitor remains: must be revealable.
    for (i, w) in ws.iter().enumerate().filter(|(i, _)| !gone[*i]) {
        let ok = can_reveal(w);
        assert!(
            ok,
            "window {i} after drag with topology change: home monitor {} remains; expected revealable (not minimized, reachable within 2 s of Alt tap + focus), observed iconic={} reachable={} rect={:?}",
            origin[i],
            iconic(w),
            reachable(w),
            w.rect()
        );
    }

    restore_displays(display.take().unwrap(), n);
    drop(drag);
    assert_layout_restored(&m0);
    assert!(
        is_alive(&mut d),
        "daemon died after display restore: expected process alive, observed exited"
    );
    for (i, w) in ws.iter().enumerate() {
        let ok = can_reveal(w);
        let mo = monitor_of(w);
        assert!(
            ok && mo == Some(origin[i]),
            "window {i} after drag, topology change and /extend: expected can_reveal and on original monitor {}, observed can_reveal={ok} monitor={mo:?} iconic={} rect={:?}",
            origin[i],
            iconic(w),
            w.rect()
        );
    }
    d.stop();
}

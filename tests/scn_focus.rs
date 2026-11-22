#![allow(dead_code, unused_imports)]
include!("common/helpers.rs");

// Scenario S1: keyboard focus and shortcuts. Every test is live (moves real windows) and #[ignore]d.
// Run with: cargo test --test scn_focus -- --ignored --test-threads=1 --nocapture

const KEY_S: u8 = 0x53;
const KEY_P: u8 = 0x50;
const KEY_LBRACKET: u8 = 0xDB; // VK_OEM_4 ('[')
const KEY_RBRACKET: u8 = 0xDD; // VK_OEM_6 (']')

/// Shortcut table used by the random-sequence test: (label, modifiers, key).
const SEQ_KEYS: &[(&str, &[u8], u8)] = &[
    ("Alt+Left", &[ALT], LEFT),
    ("Alt+Right", &[ALT], RIGHT),
    ("Alt+Up", &[ALT], UP),
    ("Alt+Down", &[ALT], DOWN),
    ("Alt+S", &[ALT], KEY_S),
    ("Alt+Shift+Left", &[ALT, SHIFT], LEFT),
    ("Alt+Shift+Right", &[ALT, SHIFT], RIGHT),
];

fn settle(d: &Daemon) {
    assert!(
        wait_settled(d, 1, Duration::from_millis(2500)),
        "focus test daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(80));
}

/// Give `w` the foreground. Alt is tapped first so Windows allows SetForegroundWindow from this process.
fn focus_window(w: &Sentinel) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{KEYEVENTF_KEYUP, VK_MENU, keybd_event};
    unsafe {
        keybd_event(VK_MENU as u8, 0, 0, 0);
        keybd_event(VK_MENU as u8, 0, KEYEVENTF_KEYUP, 0);
        let _ = SetForegroundWindow(w.hwnd as _);
    }
    eventually(3, || fg() == w.hwnd)
}

/// Assert that window `want` holds the foreground and is reachable (>=90% on a monitor).
fn expect_focus(ws: &[Sentinel], want: usize, ctx: &str) {
    let got = fg_index(ws);
    assert_eq!(
        got,
        Some(want),
        "{ctx}: expected foreground = window {want}, observed {got:?} (foreground hwnd {:#x}; None means no sentinel has focus)",
        fg()
    );
    let frac = on_screen_fraction(&ws[want]);
    assert!(
        frac >= 0.9,
        "{ctx}: expected window {want} reachable (>=90% on a monitor), observed {:.0}% on screen, rect {:?}",
        frac * 100.0,
        ws[want].rect()
    );
}

/// Press Alt+Left/Right until window `target` has focus, then check it with `expect_focus`.
fn walk_to(ws: &[Sentinel], target: usize, ctx: &str) {
    for _ in 0..(2 * ws.len() + 2) {
        match fg_index(ws) {
            Some(cur) if cur == target => break,
            Some(cur) => press(&[ALT], if target > cur { RIGHT } else { LEFT }),
            None => break,
        }
    }
    expect_focus(ws, target, ctx);
}

/// Start-state precondition: window 0 holds the foreground and is reachable.
/// Daemon start focuses the LAST created window and scrolls the strip to it, so with >=3 windows
/// window 0 is legitimately parked. Walk focus left with Alt+Left (bounded by N+2 presses) and only
/// then assert reachability.
fn start_at_window_0(ws: &[Sentinel]) {
    if fg_index(ws).is_none() {
        assert!(
            focus_window(&ws[ws.len() - 1]),
            "setup: could not give the last window ({}) the foreground",
            ws.len() - 1
        );
    }
    for _ in 0..ws.len() + 2 {
        if fg_index(ws) == Some(0) {
            break;
        }
        press(&[ALT], LEFT);
    }
    expect_focus(ws, 0, "start (walked focus to window 0 with Alt+Left)");
}

fn is_topmost(w: &Sentinel) -> bool {
    unsafe { (GetWindowLongPtrW(w.hwnd as _, GWL_EXSTYLE) as u32) & WS_EX_TOPMOST != 0 }
}

fn size_of_window(w: &Sentinel) -> (i32, i32) {
    let v = w.visible();
    (v.right - v.left, v.bottom - v.top)
}

/// Minimal seeded xorshift64 PRNG (no external crate available to tests). Seed must be non-zero.
fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Seeded Fisher-Yates permutation of 0..n.
fn shuffled(n: usize, seed: u64) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    let mut s = seed;
    for k in (1..n).rev() {
        let j = (next_rand(&mut s) % (k as u64 + 1)) as usize;
        v.swap(k, j);
    }
    v
}

/// Poll `f` for up to `ms` milliseconds (`eventually` only counts whole seconds). Never panics.
fn eventually_ms(ms: u64, mut f: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + Duration::from_millis(ms);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Focus window `i` the way the OS does it: tap Alt, then SetForegroundWindow. Within 1500 ms the
/// window must be the foreground AND reachable (>=90% on a monitor). Err carries the diagnostics.
fn system_focus(ws: &[Sentinel], i: usize) -> Result<(), String> {
    press(&[], ALT);
    let ret = unsafe { SetForegroundWindow(ws[i].hwnd as _) };
    if eventually_ms(1500, || fg_index(ws) == Some(i) && reachable(&ws[i])) {
        return Ok(());
    }
    Err(format!(
        "expected window {i} to become foreground and reachable (>=90% on a monitor) within 1500 ms after SetForegroundWindow, observed fg_index {:?}, on-screen {:.0}%, rect {:?} (SetForegroundWindow returned {ret})",
        fg_index(ws),
        on_screen_fraction(&ws[i]) * 100.0,
        ws[i].rect()
    ))
}

// ---- 1. focus_walk_right_then_left (N = 3..=6) ----------------------------------------------------

fn focus_walk(n: usize) {
    let ws: Vec<Sentinel> = (0..n).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new(&format!("focuswalk{n}"));
    d.start();
    settle(&d);
    start_at_window_0(&ws);
    for step in 0..n - 1 {
        press(&[ALT], RIGHT);
        expect_focus(&ws, step + 1, &format!("N={n} Alt+Right press {}", step + 1));
    }
    press(&[ALT], RIGHT);
    expect_focus(&ws, n - 1, &format!("N={n} Alt+Right past strip end (press {n})"));
    for k in 1..n {
        press(&[ALT], LEFT);
        expect_focus(&ws, n - 1 - k, &format!("N={n} Alt+Left press {k} (returning)"));
    }
    drop(d);
}

#[test]
#[ignore]
fn focus_walk_right_then_left_n3() {
    aware();
    focus_walk(3);
}

#[test]
#[ignore]
fn focus_walk_right_then_left_n4() {
    aware();
    focus_walk(4);
}

#[test]
#[ignore]
fn focus_walk_right_then_left_n5() {
    aware();
    focus_walk(5);
}

#[test]
#[ignore]
fn focus_walk_right_then_left_n6() {
    aware();
    focus_walk(6);
}

// ---- 2. focused_window_never_parked --------------------------------------------------------------

#[test]
#[ignore]
fn focused_window_never_parked() {
    aware();
    let n = 5;
    let ws: Vec<Sentinel> = (0..n).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("neverparked");
    d.start();
    settle(&d);
    assert!(focus_window(&ws[0]), "setup: could not give window 0 the foreground");
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut seq: Vec<&str> = Vec::new();
    for step in 0..40 {
        let k = (next_rand(&mut rng) % SEQ_KEYS.len() as u64) as usize;
        let (label, mods, vk) = SEQ_KEYS[k];
        seq.push(label);
        press(mods, vk);
        let fgw = fg_index(&ws);
        let ctx = format!(
            "press {} ({label}) after {} keys; sequence so far: [{}]",
            step + 1,
            step + 1,
            seq.join(" ")
        );
        match fgw {
            None => panic!(
                "{ctx}: expected a sentinel to hold the foreground, observed foreground hwnd {:#x} which is not a sentinel",
                fg()
            ),
            Some(i) => {
                let frac = on_screen_fraction(&ws[i]);
                assert!(
                    frac >= 0.9,
                    "{ctx}: expected foreground window {i} reachable (>=90% on a monitor), observed {:.0}% on screen, rect {:?}",
                    frac * 100.0,
                    ws[i].rect()
                );
            }
        }
    }
    drop(d);
}

/// Variant: a system focus (SetForegroundWindow) to a seeded-random window, then 10 random Alt+arrow
/// presses. The foreground sentinel must stay reachable after every press.
#[test]
#[ignore]
fn system_focus_then_keys() {
    aware();
    let n = 5;
    let ws: Vec<Sentinel> = (0..n).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("sysfocuskeys");
    d.start();
    settle(&d);
    let mut rng: u64 = 0x2545_F491_4F6C_DD1D;
    let target = (next_rand(&mut rng) % n as u64) as usize;
    if let Err(msg) = system_focus(&ws, target) {
        panic!("system focus to random window {target}: {msg}");
    }
    let arrows: [(&str, u8); 4] = [
        ("Alt+Left", LEFT),
        ("Alt+Right", RIGHT),
        ("Alt+Up", UP),
        ("Alt+Down", DOWN),
    ];
    let mut seq: Vec<&str> = Vec::new();
    for step in 0..10 {
        let k = (next_rand(&mut rng) % arrows.len() as u64) as usize;
        let (label, vk) = arrows[k];
        seq.push(label);
        press(&[ALT], vk);
        let ctx = format!(
            "after system focus to window {target} and {} key(s); sequence so far: [{}]",
            step + 1,
            seq.join(" ")
        );
        match fg_index(&ws) {
            None => panic!(
                "{ctx}: expected a sentinel to hold the foreground, observed foreground hwnd {:#x} which is not a sentinel",
                fg()
            ),
            Some(i) => {
                let frac = on_screen_fraction(&ws[i]);
                assert!(
                    frac >= 0.9,
                    "{ctx}: press {} ({label}): expected foreground window {i} reachable (>=90% on a monitor), observed {:.0}% on screen, rect {:?}",
                    step + 1,
                    frac * 100.0,
                    ws[i].rect()
                );
            }
        }
    }
    drop(d);
}

// ---- 3. window_sizes_stable_while_scrolling ------------------------------------------------------

#[test]
#[ignore]
fn window_sizes_stable_while_scrolling() {
    aware();
    let n = 5;
    let ws: Vec<Sentinel> = (0..n).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("sizes");
    d.start();
    settle(&d);
    let start: Vec<(i32, i32)> = ws.iter().map(size_of_window).collect();
    let script: [(u8, &str); 8] = [
        (RIGHT, "Alt+Right"),
        (RIGHT, "Alt+Right"),
        (RIGHT, "Alt+Right"),
        (RIGHT, "Alt+Right"),
        (LEFT, "Alt+Left"),
        (LEFT, "Alt+Left"),
        (LEFT, "Alt+Left"),
        (LEFT, "Alt+Left"),
    ];
    for (step, (key, label)) in script.iter().enumerate() {
        press(&[ALT], *key);
        for i in 0..n {
            if on_screen_fraction(&ws[i]) <= 0.0 {
                continue;
            }
            let (w, h) = size_of_window(&ws[i]);
            let (sw, sh) = start[i];
            assert!(
                (w - sw).abs() <= 2 && (h - sh).abs() <= 2,
                "scroll press {} ({label}): on-screen window {i} expected size {sw}x{sh} (as at start, +-2px), observed {w}x{h}",
                step + 1
            );
        }
    }
    drop(d);
}

// ---- 4. stack_cycle -------------------------------------------------------------------------------

#[test]
#[ignore]
fn stack_cycle() {
    aware();
    let n = 4;
    let ws: Vec<Sentinel> = (0..n).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("stack");
    d.start();
    settle(&d);
    start_at_window_0(&ws);
    press(&[ALT], RIGHT);
    expect_focus(&ws, 1, "Alt+Right to window 1 before grouping");

    // Alt+S groups window 1 with its left neighbour (window 0).
    press(&[ALT], KEY_S);
    let stack = [0usize, 1usize];
    let fgw = fg_index(&ws);
    assert!(
        matches!(fgw, Some(0) | Some(1)),
        "Alt+S (group): expected focus to stay inside stack {{0,1}}, observed {fgw:?}"
    );
    let reach: Vec<usize> = stack.iter().copied().filter(|&i| reachable(&ws[i])).collect();
    assert_eq!(
        reach,
        vec![fgw.unwrap()],
        "Alt+S (group): expected exactly one stack member reachable (the focused one {fgw:?}), observed reachable members {reach:?}"
    );

    // Alt+Up / Alt+Down cycle the members; focus must stay in the stack with exactly one reachable.
    let mut prev = fgw.unwrap();
    let cyc = [UP, DOWN, UP, DOWN];
    for (step, &key) in cyc.iter().enumerate() {
        let label = if key == UP { "Alt+Up" } else { "Alt+Down" };
        press(&[ALT], key);
        let cur = fg_index(&ws);
        let cur_i = match cur {
            Some(i) if i == 0 || i == 1 => i,
            _ => panic!(
                "{label} cycle step {}: expected focus inside stack {{0,1}}, observed {cur:?}",
                step + 1
            ),
        };
        assert_ne!(
            cur_i, prev,
            "{label} cycle step {}: expected focus to move to the other stack member than {prev}, observed still {cur_i}",
            step + 1
        );
        let reach: Vec<usize> = stack.iter().copied().filter(|&i| reachable(&ws[i])).collect();
        assert_eq!(
            reach,
            vec![cur_i],
            "{label} cycle step {}: expected exactly one stack member reachable (window {cur_i}), observed {reach:?}",
            step + 1
        );
        prev = cur_i;
    }

    // Alt+S ungroups: every window must become a reachable column again (scrolling allowed).
    press(&[ALT], KEY_S);
    assert!(fg_index(&ws).is_some(), "Alt+S (ungroup): expected a sentinel to keep focus, observed none");
    for target in 0..n {
        walk_to(&ws, target, &format!("after ungroup, reaching window {target}"));
    }
    drop(d);
}

// ---- 5. move_reorders -----------------------------------------------------------------------------

// Parked windows all sit at the same park x, so their left edges cannot order them. The order is
// therefore checked through focus navigation, which is what the user actually experiences: Alt+Left and
// Alt+Right must reach the logical neighbours.
#[test]
#[ignore]
fn move_reorders() {
    aware();
    let n = 4;
    let ws: Vec<Sentinel> = (0..n).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("moveorder");
    d.start();
    settle(&d);
    start_at_window_0(&ws);
    press(&[ALT], RIGHT);
    expect_focus(&ws, 1, "Alt+Right to window 1 before moving");
    // Strip is [0,1,2,3]: window 1 has neighbours 0 (left) and 2 (right).

    press(&[ALT, SHIFT], RIGHT);
    expect_focus(&ws, 1, "Alt+Shift+Right on window 1: focus must follow the moved window 1");
    // Expected strip [0,2,1,3]: window 1 must now have left neighbour 2 and right neighbour 3.

    press(&[ALT], LEFT);
    expect_focus(&ws, 2, "after Alt+Shift+Right: Alt+Left from window 1 must reach its new left neighbour 2");
    press(&[ALT], RIGHT);
    expect_focus(&ws, 1, "after Alt+Shift+Right: Alt+Right from window 2 must return to window 1");
    press(&[ALT], RIGHT);
    expect_focus(&ws, 3, "after Alt+Shift+Right: Alt+Right from window 1 must reach its new right neighbour 3");
    press(&[ALT], LEFT);
    expect_focus(&ws, 1, "after Alt+Shift+Right: Alt+Left from window 3 must return to window 1");

    // Alt+Shift+Left moves window 1 back past its left neighbour 2: strip [0,1,2,3] again.
    press(&[ALT, SHIFT], LEFT);
    expect_focus(&ws, 1, "Alt+Shift+Left on window 1: focus must follow the moved window 1");
    press(&[ALT], LEFT);
    expect_focus(&ws, 0, "after Alt+Shift+Left: Alt+Left from window 1 must reach window 0 (order restored)");
    press(&[ALT], RIGHT);
    expect_focus(&ws, 1, "after Alt+Shift+Left: Alt+Right from window 0 must return to window 1");
    press(&[ALT], RIGHT);
    expect_focus(&ws, 2, "after Alt+Shift+Left: Alt+Right from window 1 must reach window 2 (order restored)");
    drop(d);
}

// ---- 6. system_focus_reveals_every_window ---------------------------------------------------------

/// Every window, reached by the OS (taskbar / Alt+Tab / click style SetForegroundWindow), must scroll into
/// view and become the reachable foreground. Covers each index once in a seeded shuffled order, then
/// once more in reverse.
#[test]
#[ignore]
fn system_focus_reveals_every_window() {
    aware();
    let n = 5;
    let ws: Vec<Sentinel> = (0..n).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("sysfocus");
    d.start();
    settle(&d);
    let parked = ws.iter().filter(|w| !reachable(w)).count();
    assert!(
        parked > 0,
        "precondition: expected at least one window parked at start so system focus exercises the scroll-into-view path, observed all {n} windows reachable"
    );
    let order = shuffled(n, 0xC0FF_EE15_2026_0001);
    let mut reversed = order.clone();
    reversed.reverse();
    println!("system focus order (shuffled pass): {order:?}");
    for (pass, seq) in [("shuffled", &order), ("reverse", &reversed)] {
        for (step, &i) in seq.iter().enumerate() {
            if let Err(msg) = system_focus(&ws, i) {
                panic!("{pass} pass, step {} (window {i}): {msg}", step + 1);
            }
        }
    }
    drop(d);
}

// ---- 7. pin_toggle --------------------------------------------------------------------------------

#[test]
#[ignore]
fn pin_toggle() {
    aware();
    let n = 4;
    let ws: Vec<Sentinel> = (0..n).map(|i| Sentinel::new(i, 0)).collect();
    let mut d = Daemon::new("pin");
    d.start();
    settle(&d);
    start_at_window_0(&ws);
    press(&[ALT], RIGHT);
    expect_focus(&ws, 1, "Alt+Right to window 1 before pinning");

    press(&[ALT], KEY_P);
    assert!(
        is_topmost(&ws[1]),
        "Alt+P (pin): expected window 1 to have WS_EX_TOPMOST, observed not topmost; rect {:?}",
        ws[1].rect()
    );
    assert!(
        reachable(&ws[1]),
        "Alt+P (pin): expected pinned window 1 reachable (>=90% on a monitor), observed {:.0}% on screen, rect {:?}",
        on_screen_fraction(&ws[1]) * 100.0,
        ws[1].rect()
    );

    press(&[ALT], KEY_P);
    assert!(
        !is_topmost(&ws[1]),
        "Alt+P (unpin): expected window 1 to lose WS_EX_TOPMOST, observed still topmost"
    );
    expect_focus(&ws, 1, "Alt+P (unpin): focus should stay on window 1");

    // Unpin re-inserts the window after the focused tile, so the strip order may legitimately change.
    // Every window must therefore be reachable by OS focus, independent of index order.
    for i in 0..n {
        if let Err(msg) = system_focus(&ws, i) {
            panic!("after unpin, system focus to window {i}: {msg}");
        }
    }

    // Order-independent keyboard check: walk to the first column (Alt+Left until stable), then press
    // Alt+Right n+2 times. Every window index must be seen as foreground at some point.
    let mut prev = fg_index(&ws);
    for _ in 0..n + 2 {
        press(&[ALT], LEFT);
        let cur = fg_index(&ws);
        if cur == prev {
            break;
        }
        prev = cur;
    }
    // The walk ends on the first column, which already holds the foreground: that window is reached too
    // (the presses below only ever visit the windows to its right). Check it like every other window.
    let mut seen = vec![false; n];
    let Some(first) = fg_index(&ws) else {
        panic!("after unpin, Alt+Left to the first column: expected a sentinel in the foreground, observed foreground hwnd {:#x}", fg());
    };
    let frac = on_screen_fraction(&ws[first]);
    assert!(
        frac >= 0.9,
        "after unpin, first column window {first}: expected reachable (>=90% on a monitor), observed {:.0}% on screen, rect {:?}",
        frac * 100.0,
        ws[first].rect()
    );
    seen[first] = true;
    for press_no in 1..=n + 2 {
        press(&[ALT], RIGHT);
        match fg_index(&ws) {
            None => panic!(
                "after unpin, Alt+Right press {press_no}: expected a sentinel to hold the foreground, observed foreground hwnd {:#x} which is not a sentinel",
                fg()
            ),
            Some(i) => {
                seen[i] = true;
                let frac = on_screen_fraction(&ws[i]);
                assert!(
                    frac >= 0.9,
                    "after unpin, Alt+Right press {press_no}: expected foreground window {i} reachable (>=90% on a monitor), observed {:.0}% on screen, rect {:?}",
                    frac * 100.0,
                    ws[i].rect()
                );
            }
        }
    }
    let seen_list: Vec<usize> = (0..n).filter(|&i| seen[i]).collect();
    let missing: Vec<usize> = (0..n).filter(|&i| !seen[i]).collect();
    assert!(
        missing.is_empty(),
        "after unpin, Alt+Right x{} from the first column: expected focus to reach every window 0..{n}, observed foreground set {seen_list:?}, missing indices {missing:?}",
        n + 2
    );
    drop(d);
}

// ---- 8. monitor_hotkeys ---------------------------------------------------------------------------

#[test]
#[ignore]
fn monitor_hotkeys() {
    aware();
    let m = mons();
    if m.len() < 2 {
        println!("SKIPPED_SINGLE_MONITOR");
        return;
    }
    // One sentinel per monitor, placed inside each monitor's work area (physical pixels).
    let ws: Vec<Sentinel> = m
        .iter()
        .enumerate()
        .map(|(i, (_, work))| Sentinel::at(i, 0, work.left + 100, work.top + 100))
        .collect();
    let mut d = Daemon::new("monitors");
    d.start();
    settle(&d);
    assert!(focus_window(&ws[0]), "setup: could not give window 0 (primary) the foreground");
    assert_eq!(
        monitor_of(&ws[0]),
        Some(0),
        "start: expected window 0 on the primary monitor (index 0), observed monitor {:?}",
        monitor_of(&ws[0])
    );
    expect_focus(&ws, 0, "start on primary");

    for target in 1..m.len() {
        press(&[ALT], KEY_RBRACKET);
        expect_focus(&ws, target, &format!("Alt+] to monitor {target}"));
        assert_eq!(
            monitor_of(&ws[target]),
            Some(target),
            "Alt+] to monitor {target}: expected focused window {target} on monitor {target}, observed monitor {:?}",
            monitor_of(&ws[target])
        );
    }
    for target in (0..m.len() - 1).rev() {
        press(&[ALT], KEY_LBRACKET);
        expect_focus(&ws, target, &format!("Alt+[ back to monitor {target}"));
        assert_eq!(
            monitor_of(&ws[target]),
            Some(target),
            "Alt+[ back to monitor {target}: expected focused window {target} on monitor {target}, observed monitor {:?}",
            monitor_of(&ws[target])
        );
    }
    drop(d);
}

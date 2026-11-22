#![allow(dead_code, unused_imports)]
include!("common/helpers.rs");

/// Pre-start snapshot of one window: (GetWindowRect, Sentinel::placement()).
type Snap = ([i32; 4], [i32; 10]);

const BACKSPACE: u8 = 0x08;
const RECOVERY_TAG: &str = "Planor.Recovery.Identity";

fn snapshot(ws: &[Sentinel]) -> Vec<Snap> {
    ws.iter().map(|w| (w.rect(), w.placement())).collect()
}

fn make_windows(n: usize) -> Vec<Sentinel> {
    (0..n).map(|i| Sentinel::new(i, 0)).collect()
}

fn journal_path(d: &Daemon) -> std::path::PathBuf {
    d.dir.join("planor/recovery.json")
}

/// The user-visible part of a placement: [flags, showCmd, rcNormalPosition l, t, r, b].
/// ptMinPosition/ptMaxPosition are rewritten by Windows whenever a window is minimized, so they are excluded.
fn placement_key(p: &[i32; 10]) -> [i32; 6] {
    [p[0], p[1], p[6], p[7], p[8], p[9]]
}

/// Checks one window against its pre-start snapshot. `label` is the window index used in messages.
fn check_window(w: &Sentinel, label: usize, before: &Snap) -> Result<(), String> {
    check_window_mode(w, label, before, false)
}

/// `loose` compares only flags, showCmd and rcNormalPosition of the placement (see `placement_key`).
fn check_window_mode(w: &Sentinel, label: usize, before: &Snap, loose: bool) -> Result<(), String> {
    let rect = w.rect();
    for j in 0..4 {
        if (rect[j] - before.0[j]).abs() > 1 {
            return Err(format!(
                "window {label}: expected rect {:?} (+-1px) as before Planor started, observed {:?}",
                before.0, rect
            ));
        }
    }
    let place = w.placement();
    let same = if loose {
        placement_key(&place) == placement_key(&before.1)
    } else {
        place == before.1
    };
    if !same {
        let (exp, obs, what) = if loose {
            (
                format!("{:?}", placement_key(&before.1)),
                format!("{:?}", placement_key(&place)),
                "flags, showCmd, rcNormalPosition",
            )
        } else {
            (format!("{:?}", before.1), format!("{place:?}"), "full placement")
        };
        return Err(format!(
            "window {label}: expected {what} {exp} as before Planor started, observed {obs}"
        ));
    }
    if !reachable(w) {
        let v = w.visible();
        return Err(format!(
            "window {label}: expected reachable (>=90% on a monitor), observed visible frame [{}, {}, {}, {}] (rect {:?})",
            v.left, v.top, v.right, v.bottom, rect
        ));
    }
    Ok(())
}

fn all_restored(ws: &[Sentinel], before: &[Snap], labels: &[usize]) -> bool {
    ws.iter()
        .enumerate()
        .all(|(k, w)| check_window(w, labels[k], &before[k]).is_ok())
}

/// Asserts every window in `ws` is back at its pre-start rect and placement and reachable.
/// `labels[k]` is the original window index reported for `ws[k]`.
fn restored_labeled(ws: &[Sentinel], before: &[Snap], labels: &[usize]) {
    assert_eq!(ws.len(), before.len(), "test bug: snapshot length mismatch");
    assert_eq!(ws.len(), labels.len(), "test bug: label length mismatch");
    for (k, w) in ws.iter().enumerate() {
        if let Err(e) = check_window(w, labels[k], &before[k]) {
            panic!("{e}");
        }
    }
}

fn restored(ws: &[Sentinel], before: &[Snap]) {
    let labels: Vec<usize> = (0..ws.len()).collect();
    restored_labeled(ws, before, &labels);
}

/// Like `restored`, but compares only rect, flags, showCmd and rcNormalPosition.
fn restored_loose(ws: &[Sentinel], before: &[Snap]) {
    assert_eq!(ws.len(), before.len(), "test bug: snapshot length mismatch");
    for (k, w) in ws.iter().enumerate() {
        if let Err(e) = check_window_mode(w, k, &before[k], true) {
            panic!("{e}");
        }
    }
}

fn tasklist_planor() -> String {
    Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq planor.exe"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_else(|e| format!("(tasklist failed: {e})"))
}

fn planor_running() -> bool {
    tasklist_planor().to_ascii_lowercase().contains("planor.exe")
}

/// Id and executable path of every planor process, for assertion messages.
fn planor_processes() -> String {
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-Process -Name planor -ErrorAction SilentlyContinue | Select-Object Id,Path | Format-Table -AutoSize | Out-String",
        ])
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { "(none)".to_string() } else { s }
        })
        .unwrap_or_else(|e| format!("(powershell failed: {e})"))
}

/// Panics with tasklist and process details if a planor.exe is running (the user's own, or a leftover).
fn assert_no_real_planor(context: &str) {
    assert!(
        !planor_running(),
        "{context}: expected no planor.exe running, observed one:\ntasklist /FI \"IMAGENAME eq planor.exe\":\n{}\nGet-Process planor (Id, Path):\n{}",
        tasklist_planor(),
        planor_processes()
    );
}

/// Waits (up to 5 s) for the killed daemon's planor.exe to disappear; panics with details otherwise.
fn wait_no_planor(context: &str) {
    if !eventually(5, || !planor_running()) {
        panic!(
            "{context}: expected no planor.exe within 5 s of Daemon::kill(), observed one still running:\ntasklist /FI \"IMAGENAME eq planor.exe\":\n{}\nGet-Process planor (Id, Path):\n{}",
            tasklist_planor(),
            planor_processes()
        );
    }
}

/// A `planor` command with the same environment `Daemon` uses, so it talks to the test daemon only.
fn planor_cmd(inst: &str, dir: &Path, arg: &str) -> Command {
    let mut c = Command::new(EXE);
    c.env("PLANOR_INSTANCE", inst)
        .env("APPDATA", dir)
        .env("LOCALAPPDATA", dir)
        .arg(arg);
    c
}

/// Runs `planor stop` for the test instance when a journal remains, so a failing test never leaves windows parked.
struct Rescue {
    inst: String,
    dir: PathBuf,
}
impl Drop for Rescue {
    fn drop(&mut self) {
        if self.dir.join("planor/recovery.json").exists() {
            let _ = planor_cmd(&self.inst, &self.dir, "stop")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Polls the daemon child process until it exits or `secs` elapse. None means still running.
fn wait_exit(d: &mut Daemon, secs: u64) -> Option<std::process::ExitStatus> {
    let until = Instant::now() + Duration::from_secs(secs);
    loop {
        let status = d
            .p
            .as_mut()
            .expect("test bug: daemon not running")
            .try_wait()
            .unwrap();
        if status.is_some() || Instant::now() >= until {
            return status;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// `planor stop` with explicit messages: the command must succeed and the daemon must exit with 0.
fn stop_checked(d: &mut Daemon) {
    assert!(
        d.call("stop"),
        "expected `planor stop` to succeed, observed the command failing"
    );
    let status = wait_exit(d, 10).unwrap_or_else(|| {
        panic!("expected daemon to exit within 10 s of `planor stop`, observed it still running")
    });
    assert!(
        status.success(),
        "expected daemon exit code 0 after `planor stop`, observed {status:?}"
    );
    d.p = None;
}

/// Rewrites `tile_existing_on_startup` in the test daemon's config.
fn set_tile_on_startup(d: &Daemon, on: bool) {
    let path = d.dir.join("planor/planor.toml");
    let text = fs::read_to_string(&path).unwrap();
    let (from, to) = if on { ("false", "true") } else { ("true", "false") };
    let updated = text.replace(
        &format!("tile_existing_on_startup = {from}"),
        &format!("tile_existing_on_startup = {to}"),
    );
    assert!(
        updated.contains(&format!("tile_existing_on_startup = {to}")),
        "test bug: config has no `tile_existing_on_startup` key to set to {to}"
    );
    fs::write(path, updated).unwrap();
}

/// Sends a chord without the 700 ms settle that `press` adds.
fn send_chord(mods: &[u8], vk: u8) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{KEYEVENTF_KEYUP, keybd_event};
    unsafe {
        for m in mods {
            keybd_event(*m, 0, 0, 0);
        }
        keybd_event(vk, 0, 0, 0);
        keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
        for m in mods.iter().rev() {
            keybd_event(*m, 0, KEYEVENTF_KEYUP, 0);
        }
    }
}

/// Scrolls the strip right `n` times so that several windows are parked off-screen.
fn scroll_right(n: usize) {
    for _ in 0..n {
        press(&[ALT], RIGHT);
    }
}

fn count_parked(ws: &[Sentinel]) -> usize {
    ws.iter().filter(|w| !reachable(w)).count()
}

#[test]
#[ignore]
fn stop_with_many_windows() {
    aware();
    let ws = make_windows(6);
    let before = snapshot(&ws);
    let mut d = Daemon::new("scn-exit-stop-many");
    d.start();
    thread::sleep(Duration::from_millis(1500));
    scroll_right(3);
    stop_checked(&mut d);
    restored(&ws, &before);
    assert!(
        !journal_path(&d).exists(),
        "expected recovery.json removed after stop, observed it still present"
    );
}

#[test]
#[ignore]
fn panic_hotkey() {
    aware();
    let ws = make_windows(6);
    let before = snapshot(&ws);
    let mut d = Daemon::new("scn-exit-panic");
    d.start();
    thread::sleep(Duration::from_millis(1500));
    press(&[CTRL, ALT, SHIFT], BACKSPACE);
    let status = wait_exit(&mut d, 5);
    assert!(
        status.is_some(),
        "expected daemon to exit within 5 s of Ctrl+Alt+Shift+Backspace, observed it still running"
    );
    restored(&ws, &before);
    assert!(
        !journal_path(&d).exists(),
        "expected recovery.json removed after panic hotkey, observed it still present"
    );
}

#[test]
#[ignore]
fn kill_then_restart_recovers() {
    aware();
    let ws = make_windows(6);
    let before = snapshot(&ws);
    let labels: Vec<usize> = (0..ws.len()).collect();
    let mut d = Daemon::new("scn-exit-kill-restart");
    d.start();
    thread::sleep(Duration::from_millis(1500));
    scroll_right(3);
    d.kill();
    assert!(
        journal_path(&d).exists(),
        "expected recovery.json present after kill, observed it missing"
    );
    let parked = count_parked(&ws);
    assert!(
        parked > 0,
        "precondition: expected at least one window parked off-screen after kill, observed all {} reachable",
        ws.len()
    );
    set_tile_on_startup(&d, false);
    d.start();
    // Bounded wait (5 s); `restored` then reports the first window that is still wrong.
    eventually(5, || all_restored(&ws, &before, &labels));
    restored(&ws, &before);
    stop_checked(&mut d);
    restored(&ws, &before);
}

#[test]
#[ignore]
fn kill_twice() {
    aware();
    let ws = make_windows(6);
    let before = snapshot(&ws);
    let labels: Vec<usize> = (0..ws.len()).collect();
    let mut d = Daemon::new("scn-exit-kill-twice");

    // First kill: tiled and parked, then restart without tiling. Restart must restore everything.
    d.start();
    thread::sleep(Duration::from_millis(1500));
    scroll_right(3);
    d.kill();
    wait_no_planor("after kill 1");
    let parked = count_parked(&ws);
    assert!(
        parked > 0,
        "precondition (kill 1): expected at least one window parked off-screen, observed all {} reachable",
        ws.len()
    );
    set_tile_on_startup(&d, false);
    d.start();
    eventually(5, || all_restored(&ws, &before, &labels));
    restored(&ws, &before);
    stop_checked(&mut d);

    // Second kill: re-tile so windows are parked again, then kill before any stop.
    set_tile_on_startup(&d, true);
    d.start();
    thread::sleep(Duration::from_millis(1500));
    scroll_right(3);
    d.kill();
    wait_no_planor("after kill 2");
    let parked = count_parked(&ws);
    assert!(
        parked > 0,
        "precondition (kill 2): expected at least one window parked off-screen, observed all {} reachable",
        ws.len()
    );

    // Restart after the second kill: all windows must be restored again.
    set_tile_on_startup(&d, false);
    d.start();
    eventually(5, || all_restored(&ws, &before, &labels));
    restored(&ws, &before);
    stop_checked(&mut d);
    restored(&ws, &before);
}

#[test]
#[ignore]
fn stop_during_animation() {
    aware();
    let ws = make_windows(6);
    let before = snapshot(&ws);
    let mut d = Daemon::new("scn-exit-animation");
    d.start();
    thread::sleep(Duration::from_millis(1500));
    send_chord(&[ALT], RIGHT);
    thread::sleep(Duration::from_millis(100));
    stop_checked(&mut d);
    restored(&ws, &before);
    assert!(
        !journal_path(&d).exists(),
        "expected recovery.json removed after stop during animation, observed it still present"
    );
}

#[test]
#[ignore]
fn close_some_windows_then_stop() {
    aware();
    let ws = make_windows(6);
    let before_all = snapshot(&ws);
    let mut d = Daemon::new("scn-exit-close");
    d.start();
    thread::sleep(Duration::from_millis(1500));

    // Close windows 1 and 4 while tiled; keep the other four with their original indices.
    let mut kept = Vec::new();
    let mut kept_before = Vec::new();
    let mut labels = Vec::new();
    for (i, w) in ws.into_iter().enumerate() {
        if i == 1 || i == 4 {
            drop(w);
        } else {
            kept.push(w);
            kept_before.push(before_all[i]);
            labels.push(i);
        }
    }
    thread::sleep(Duration::from_millis(800));
    assert!(
        wait_exit(&mut d, 0).is_none(),
        "expected daemon still running after windows 1 and 4 were closed, observed it exited"
    );
    assert!(
        d.call("reload"),
        "expected daemon to respond to `reload` after windows 1 and 4 were closed, observed no response"
    );

    stop_checked(&mut d);
    restored_labeled(&kept, &kept_before, &labels);
    assert!(
        !journal_path(&d).exists(),
        "expected recovery.json removed after stop, observed it still present"
    );
}

#[test]
#[ignore]
fn minimized_window_stop() {
    aware();
    let ws = make_windows(6);
    let before = snapshot(&ws);
    let mut d = Daemon::new("scn-exit-minimized");
    d.start();
    thread::sleep(Duration::from_millis(1500));
    unsafe { ShowWindow(ws[2].hwnd as _, SW_MINIMIZE) };
    thread::sleep(Duration::from_millis(300));
    assert!(
        unsafe { IsIconic(ws[2].hwnd as _) } != 0,
        "precondition: expected window 2 minimized before stop, observed it not minimized"
    );
    stop_checked(&mut d);
    // The journal recorded window 2 as normal (pre-Planor placement), so it must come back normal at its rect.
    // ptMin/ptMaxPosition are not compared: Windows rewrites them on minimize and they are not user-visible.
    restored_loose(&ws, &before);
    unsafe { ShowWindow(ws[2].hwnd as _, SW_RESTORE) };
    assert!(
        eventually(3, || reachable(&ws[2])),
        "window 2: expected reachable after ShowWindow(SW_RESTORE), observed visible frame not on a monitor"
    );
}

#[test]
#[ignore]
fn maximized_before_start() {
    aware();
    let ws = make_windows(6);
    unsafe { ShowWindow(ws[0].hwnd as _, SW_MAXIMIZE) };
    assert!(
        eventually(3, || unsafe { IsZoomed(ws[0].hwnd as _) } != 0),
        "precondition: expected window 0 maximized before start, observed not maximized"
    );
    thread::sleep(Duration::from_millis(300));
    let before = snapshot(&ws);
    let mut d = Daemon::new("scn-exit-maximized");
    d.start();
    thread::sleep(Duration::from_millis(1500));
    stop_checked(&mut d);
    restored(&ws, &before);
    assert!(
        unsafe { IsZoomed(ws[0].hwnd as _) } != 0,
        "window 0: expected maximized after stop (as before start), observed not maximized"
    );
}

#[test]
#[ignore]
fn stale_journal_entry() {
    aware();
    let w = Sentinel::new(0, 0);
    let before = snapshot(std::slice::from_ref(&w))[0];
    let mut d = Daemon::new("scn-exit-stale");
    set_tile_on_startup(&d, false);

    // Move the live window away from its original place so only the journal can bring it back.
    unsafe {
        SetWindowPos(
            w.hwnd as _,
            null_mut(),
            700,
            500,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
    thread::sleep(Duration::from_millis(300));
    let cookie: usize = 0x1234_5679;
    unsafe { SetPropW(w.hwnd as _, wide(RECOVERY_TAG).as_ptr(), cookie as _) };

    // Hand-written journal: one dead hwnd (no such window, pid that does not exist) plus the live window.
    let pl = w.placement();
    let dead_id: isize = 0x5EAD_BEE8;
    let dead = serde_json::json!({
        "pid": 0xFFFF_FFF0u32,
        "cookie": 0x2222usize,
        "flags": 0,
        "show": 1,
        "min": [-1, -1],
        "max": [-1, -1],
        "normal": [0, 0, 10, 10],
        "rect": [0, 0, 10, 10],
        "topmost": false
    });
    let live = serde_json::json!({
        "pid": std::process::id(),
        "cookie": cookie,
        "flags": pl[0],
        "show": pl[1],
        "min": [pl[2], pl[3]],
        "max": [pl[4], pl[5]],
        "normal": [pl[6], pl[7], pl[8], pl[9]],
        "rect": before.0,
        "topmost": false
    });
    let mut map = serde_json::Map::new();
    map.insert(dead_id.to_string(), dead);
    map.insert(w.hwnd.to_string(), live);
    fs::write(journal_path(&d), serde_json::to_vec(&map).unwrap()).unwrap();

    d.start();
    assert!(
        wait_exit(&mut d, 0).is_none(),
        "expected daemon running after start with a stale journal entry, observed it exited"
    );
    eventually(5, || check_window(&w, 0, &before).is_ok());
    restored(std::slice::from_ref(&w), &[before]);
    assert!(
        !journal_path(&d).exists(),
        "expected recovery.json removed once stale and live entries were handled, observed it still present"
    );
    stop_checked(&mut d);
}

#[test]
#[ignore]
fn stop_without_daemon_rescues_windows() {
    aware();
    let ws = make_windows(6);
    let before = snapshot(&ws);
    let mut d = Daemon::new("scn-exit-stop-nodaemon");
    d.start();
    thread::sleep(Duration::from_millis(1500));
    scroll_right(3);
    d.kill();
    wait_no_planor("stop without daemon (after kill)");
    let _rescue = Rescue {
        inst: d.inst.clone(),
        dir: d.dir.clone(),
    };
    let parked = count_parked(&ws);
    assert!(
        parked > 0,
        "precondition: expected at least one window parked off-screen after kill, observed all {} reachable",
        ws.len()
    );

    let out = planor_cmd(&d.inst, &d.dir, "stop")
        .output()
        .expect("test bug: could not run planor binary");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "expected `planor stop` with no daemon running to exit 0, observed {:?} (stdout {stdout:?}, stderr {stderr:?})",
        out.status
    );
    assert!(
        stdout.contains("restored"),
        "expected `planor stop` with no daemon to print 'restored', observed stdout {stdout:?}"
    );
    restored(&ws, &before);
    assert!(
        !journal_path(&d).exists(),
        "expected recovery.json deleted by `planor stop` with no daemon, observed it still present"
    );
}

#[test]
#[ignore]
fn ctrl_break_restores_before_exit() {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    use windows_sys::Win32::Foundation::GetLastError;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    aware();
    let ws = make_windows(6);
    let before = snapshot(&ws);
    let mut d = Daemon::new("scn-exit-ctrl-break");
    assert_no_real_planor("ctrl_break_restores_before_exit");

    // Spawn the daemon directly (same environment as Daemon::start) in its own process group,
    // so CTRL_BREAK can be aimed at it alone.
    let child = planor_cmd(&d.inst, &d.dir, "start")
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .expect("test bug: could not spawn planor start");
    let pid = child.id();
    d.p = Some(child);
    let _rescue = Rescue {
        inst: d.inst.clone(),
        dir: d.dir.clone(),
    };
    let until = Instant::now() + Duration::from_secs(8);
    while !planor_cmd(&d.inst, &d.dir, "reload")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
    {
        assert!(
            d.p.as_mut().unwrap().try_wait().unwrap().is_none(),
            "daemon exited during startup (ctrl-break test)"
        );
        assert!(Instant::now() < until, "pipe startup timeout (ctrl-break test)");
        thread::sleep(Duration::from_millis(80));
    }
    thread::sleep(Duration::from_millis(1500));
    scroll_right(3);
    let parked = count_parked(&ws);
    assert!(
        parked > 0,
        "precondition: expected at least one window parked off-screen before CTRL_BREAK, observed all {} reachable",
        ws.len()
    );

    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    if sent == 0 {
        let code = unsafe { GetLastError() };
        println!("SKIPPED_NO_CONSOLE: GenerateConsoleCtrlEvent failed (GetLastError {code})");
        return;
    }

    // The process must exit within 6 s. Windows are checked immediately after the exit is observed,
    // so they can only be restored if restoration finished before the process went away.
    let status = wait_exit(&mut d, 6);
    assert!(
        status.is_some(),
        "expected daemon to exit within 6 s of CTRL_BREAK_EVENT, observed it still running"
    );
    restored(&ws, &before);
    assert!(
        !journal_path(&d).exists(),
        "expected recovery.json removed before the process exited after CTRL_BREAK, observed it still present"
    );
}

#[test]
#[ignore]
fn exit_restores_windows_on_many_monitors() {
    aware();
    let m = mons();
    if m.len() < 2 {
        println!("SKIPPED_SINGLE_MONITOR: {} monitor(s) attached, need at least 2", m.len());
        return;
    }
    // One sentinel per monitor (index i on monitor i, primary first), plus 3 more on the primary.
    let mut ws: Vec<Sentinel> = Vec::new();
    for (i, (_, work)) in m.iter().enumerate() {
        ws.push(Sentinel::at(i, 0, work.left + 100, work.top + 100));
    }
    let primary_work = m[0].1;
    for k in 0..3 {
        ws.push(Sentinel::at(
            m.len() + k,
            0,
            primary_work.left + 100 + 400 * k as i32,
            primary_work.top + 400,
        ));
    }
    thread::sleep(Duration::from_millis(500));
    for (i, w) in ws.iter().take(m.len()).enumerate() {
        assert_eq!(
            monitor_of(w),
            Some(i),
            "precondition: expected sentinel {i} on monitor {i}, observed monitor {:?}",
            monitor_of(w)
        );
    }
    let before = snapshot(&ws);
    let mut d = Daemon::new("scn-exit-many-monitors");
    d.start();
    thread::sleep(Duration::from_millis(1500));
    let moved = ws
        .iter()
        .zip(before.iter())
        .filter(|(w, b)| w.rect() != b.0)
        .count();
    assert!(
        moved > 0,
        "precondition: expected tiling to move at least one of {} windows after start, observed none moved",
        ws.len()
    );
    stop_checked(&mut d);
    restored(&ws, &before);
    assert!(
        !journal_path(&d).exists(),
        "expected recovery.json removed after stop on multiple monitors, observed it still present"
    );
}

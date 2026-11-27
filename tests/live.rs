#![allow(dead_code, unused_imports)]
include!("common/helpers.rs");
#[test]
#[ignore]
fn stop_restores_exact_rect() {
    let w = Sentinel::new(0, 0);
    let before = w.rect();
    let saved = w.placement();
    let mut d = Daemon::new("stop");
    d.start();
    wait(|| w.rect() != before);
    d.stop();
    assert_eq!(w.rect(), before);
    assert_eq!(w.placement(), saved);
    assert!(!d.dir.join("planor/recovery.json").exists())
}
#[test]
#[ignore]
fn forced_kill_recovery() {
    let w = Sentinel::new(1, 0);
    let before = w.rect();
    let _second = Sentinel::new(2, 0);
    let _third = Sentinel::new(3, 0);
    let mut d = Daemon::new("kill");
    d.start();
    let right =
        unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    wait(|| w.rect()[0] >= right);
    d.kill();
    assert!(d.dir.join("planor/recovery.json").exists());
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
    wait(|| w.rect() == before);
    d.stop();
    assert_eq!(w.rect(), before)
}
#[test]
#[ignore]
fn owned_dialog_excluded() {
    let w = Sentinel::new(0, 0);
    let dlg = Sentinel::new(2, w.hwnd);
    let before = dlg.rect();
    let owner = w.rect();
    let mut d = Daemon::new("owned");
    d.start();
    wait(|| w.rect() != owner);
    assert_eq!(dlg.rect(), before);
    d.stop()
}
#[test]
#[ignore]
fn compute_visual() {
    let first = Sentinel::new(0, 0);
    let w = Sentinel::new(1, 0);
    let _third = Sentinel::new(2, 0);
    let mut d = Daemon::new_animated("visual");
    let output = Path::new("build/live");
    fs::create_dir_all(output).unwrap();
    let mut report = serde_json::json!({"bleed_events":null,"border_delta_px":null,"input_to_first_pixel_ms":null,"median_dt_ms":null,"p99_dt_ms":null,"frozen_frames":null,"non_monotonic_jumps":null});
    let save = |r: &serde_json::Value| {
        fs::write(
            output.join("visual_report.json"),
            serde_json::to_vec_pretty(r).unwrap(),
        )
        .unwrap()
    };
    save(&report);
    let capture = Capture::open(0).expect("DXGI primary output duplication");
    let count = unsafe { GetSystemMetrics(SM_CMONITORS) };
    let secondary = if count > 1 {
        Some(Capture::open(1).expect("DXGI second output"))
    } else {
        report["bleed_status"] = "SKIPPED_SINGLE_MONITOR".into();
        None
    };
    d.start();
    thread::sleep(Duration::from_millis(550));
    focus_key(0x25);
    thread::sleep(Duration::from_millis(300));
    focus_key(0x25);
    thread::sleep(Duration::from_millis(300));
    focus_key(0x27);
    thread::sleep(Duration::from_millis(350));
    let reference = w.visible();
    assert!(first.hwnd != 0);
    let baseline = capture.frame(reference).expect("baseline desktop frame");
    report["border_delta_px"] = baseline.border.into();
    save(&report);
    let mut start = 0i64;
    let mut hz = 0i64;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn QueryPerformanceCounter(v: *mut i64) -> i32;
        fn QueryPerformanceFrequency(v: *mut i64) -> i32;
    }
    unsafe {
        QueryPerformanceFrequency(&mut hz);
        QueryPerformanceCounter(&mut start)
    };
    focus_key(0x27);
    let until = Instant::now() + Duration::from_millis(850);
    let mut frames = vec![];
    let mut film = vec![];
    let mut missing = 0;
    let mut primary_frames = 0;
    let observed = secondary.is_some();
    let observer = thread::spawn(move || {
        let mut bleed = 0;
        let mut samples = 0;
        if let Some(b) = secondary {
            while Instant::now() < until {
                if let Ok(p) = b.frame(reference) {
                    bleed += usize::from(p.sentinel);
                    samples += 1
                }
            }
        }
        (bleed, samples)
    });
    while Instant::now() < until {
        if let Ok(p) = capture.frame(reference) {
            if p.time > start && !p.mask.is_empty() {
                film.push((p.mask, p.edge.is_none()))
            }
            if p.time > start {
                primary_frames += 1;
                if p.edge.is_none() {
                    missing += 1
                }
            }
            if p.time > start && p.edge.is_some() {
                frames.push((p.time, p.edge.unwrap()));
            }
        }
    }
    let (bleed, samples) = observer.join().unwrap();
    d.stop();
    if !film.is_empty() {
        let mut bytes = vec![0u8; 960 * 240 * 3];
        for i in 0..8 {
            let (mask, anomaly) = &film[i * (film.len() - 1) / 7];
            for y in 0..120 {
                for x in 0..240 {
                    let src = (y * 240 + x) * 3;
                    let dst = (((i / 4) * 120 + y) * 960 + (i % 4) * 240 + x) * 3;
                    bytes[dst..dst + 3].copy_from_slice(
                        if *anomaly && (x < 2 || x >= 238 || y < 2 || y >= 118) {
                            &[255, 0, 0]
                        } else {
                            &mask[src..src + 3]
                        },
                    )
                }
            }
        }
        let mut png = png::Encoder::new(
            fs::File::create(output.join("filmstrip.png")).unwrap(),
            960,
            240,
        );
        png.set_color(png::ColorType::Rgb);
        png.write_header()
            .unwrap()
            .write_image_data(&bytes)
            .unwrap();
    }
    if observed {
        assert!(samples > 0, "no secondary frames");
        report["bleed_events"] = bleed.into()
    }
    let dt = |ticks: i64| ticks as f64 * 1000. / hz as f64;
    // Focus changes remove the configured two-pixel stroke from the sampled client edge.
    report["edge_tolerance_px"] = 2.into();
    report["primary_frames"] = primary_frames.into();
    report["missing_edge_frames"] = missing.into();
    report["captured_samples"] = frames.len().into();
    if std::env::var_os("PLANOR_TIMELINE").is_some() {
        for f in &frames {
            eprintln!("TIMELINE {:8.1} ms edge {}", dt(f.0 - start), f.1);
        }
    }
    let changed = frames
        .iter()
        .rposition(|f| Some(f.1) != frames.last().map(|x| x.1));
    if let Some(i) = changed {
        frames.truncate(i + 2)
    }
    // The scroll is the 180 ms animation (planor.example.toml `[animation] duration_ms`) plus scene latency.
    // The renderer's post-settle verification re-places the window ~250 ms after it stops; the idle time
    // before that correction is settled, not scrolling, so it is not a presentation gap.
    const SCROLL_WINDOW_MS: f64 = 180. + 100.;
    let mut gaps: Vec<_> = frames
        .windows(2)
        .filter(|p| dt(p[1].0 - start) <= SCROLL_WINDOW_MS)
        .map(|p| dt(p[1].0 - p[0].0))
        .collect();
    gaps.sort_by(f64::total_cmp);
    if !gaps.is_empty() {
        report["median_dt_ms"] = gaps[gaps.len() / 2].into();
        report["p99_dt_ms"] = gaps[(gaps.len() * 99).div_ceil(100) - 1].into();
        report["frozen_frames"] = frames
            .windows(2)
            .filter(|p| p[0].1 == p[1].1)
            .count()
            .into();
        report["non_monotonic_jumps"] = frames
            .windows(2)
            .filter(|p| p[1].1 - p[0].1 > 2)
            .count()
            .into()
    }
    report["input_to_first_pixel_ms"] = frames
        .iter()
        .find(|p| Some(p.1) != baseline.edge)
        .map(|p| dt(p.0 - start))
        .into();
    save(&report);
    assert!(
        baseline.border.is_some_and(|d| d <= 2),
        "focused border missing or misplaced"
    );
    assert!(
        frames
            .iter()
            .map(|f| f.1)
            .collect::<std::collections::HashSet<_>>()
            .len()
            >= 4,
        "insufficient distinct motion samples"
    );
    assert_eq!(missing, 0, "missing sentinel during motion");
    assert!(
        report["p99_dt_ms"].as_f64().unwrap() < 80.,
        "presentation gap during scroll"
    );
    assert_eq!(bleed, 0);
    assert_eq!(report["non_monotonic_jumps"], 0)
}
#[test]
#[ignore]
fn bounded_rejection_backoff() {
    let w = Sentinel::new(0, 0);
    let mut d = Daemon::new("backoff");
    let p = d.dir.join("planor/planor.toml");
    let raw = fs::read_to_string(&p).unwrap();
    assert!(
        raw.contains("spawn_on_cursor_monitor = true"),
        "planor.toml template must contain `spawn_on_cursor_monitor = true` (#97)"
    );
    let config = raw
        .replace(
            "[animation]\nenabled = true",
            "[animation]\nenabled = false",
        )
        .replace(
            "[diagnostics]\nenabled = false",
            "[diagnostics]\nenabled = true",
        )
        // Keep `_second` on the primary monitor with `w` even if the real cursor sits elsewhere (#96).
        .replace(
            "spawn_on_cursor_monitor = true",
            "spawn_on_cursor_monitor = false",
        );
    fs::write(p, config).unwrap();
    d.start();
    let before = w.rect();
    wait(|| w.rect() != before);
    unsafe {
        SetPropW(
            w.hwnd as _,
            wide("Planor.Test.Minimum").as_ptr(),
            1usize as _,
        );
    }
    let _second = Sentinel::new(1, 0);
    let log = d.dir.join("planor/frames.0.log");
    wait(|| {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .any(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .is_some_and(|v| v["retries"] == 2)
            })
    });
    unsafe {
        RemovePropW(w.hwnd as _, wide("Planor.Test.Minimum").as_ptr());
    }
    d.stop()
}
#[test]
#[ignore]
fn desktop_round_trip() {
    let original = winvd::get_current_desktop().unwrap();
    struct Return(winvd::Desktop);
    impl Drop for Return {
        fn drop(&mut self) {
            let _ = winvd::switch_desktop(self.0);
        }
    }
    let _return = Return(original);
    let other = winvd::get_desktops()
        .unwrap()
        .into_iter()
        .find(|d| *d != original && d.get_name().as_deref() == Ok("Planor verification"))
        .unwrap_or_else(|| {
            let d = winvd::create_desktop().unwrap();
            d.set_name("Planor verification").unwrap();
            d
        });
    let w = Sentinel::new(0, 0);
    let before = w.rect();
    let mut daemon = Daemon::new("desktop");
    daemon.start();
    wait(|| w.rect() != before);
    // Animation (180ms) plus the renderer's post-settle frame verification (250ms) must finish.
    thread::sleep(Duration::from_millis(900));
    let tile = w.rect();
    winvd::switch_desktop(other).unwrap();
    wait(|| winvd::get_current_desktop().ok() == Some(other));
    thread::sleep(Duration::from_millis(500));
    assert!(daemon.call("pause"));
    assert!(daemon.call("resume"));
    winvd::switch_desktop(original).unwrap();
    wait(|| winvd::get_current_desktop().ok() == Some(original));
    thread::sleep(Duration::from_millis(500));
    assert_eq!(w.rect(), tile);
    daemon.stop();
    assert_eq!(w.rect(), before)
}
#[test]
#[ignore]
fn owned_modal_follows_display_transfer() {
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let owner = Sentinel::new(0, 0);
    let dialog = Sentinel::new(1, owner.hwnd);
    let before = dialog.rect();
    let original = owner.rect();
    let mut d = Daemon::new("modal-transfer");
    d.start();
    wait(|| owner.rect() != original);
    thread::sleep(Duration::from_millis(400));
    let primary = unsafe { MonitorFromWindow(owner.hwnd as _, MONITOR_DEFAULTTONEAREST) };
    unsafe extern "system" fn monitor(m: HMONITOR, _: HDC, _: *mut RECT, data: isize) -> i32 {
        unsafe { (*(data as *mut Vec<isize>)).push(m as isize) };
        1
    }
    let mut monitors = Vec::<isize>::new();
    unsafe {
        EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(monitor),
            &mut monitors as *mut _ as isize,
        );
    }
    let Some(dst) = monitors.into_iter().find(|&m| m != primary as isize) else {
        eprintln!("SKIPPED_SINGLE_MONITOR");
        d.stop();
        return;
    };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as _,
        ..Default::default()
    };
    assert_ne!(unsafe { GetMonitorInfoW(dst as _, &mut info) }, 0);
    let area = info.rcWork;
    struct Cursor(POINT);
    impl Drop for Cursor {
        fn drop(&mut self) {
            unsafe {
                SetCursorPos(self.0.x, self.0.y);
            }
        }
    }
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p) };
    let _cursor = Cursor(p);
    let h = owner.hwnd as HWND;
    use windows_sys::Win32::UI::Accessibility::NotifyWinEvent;
    unsafe {
        NotifyWinEvent(EVENT_SYSTEM_MOVESIZESTART, h, 0, 0);
    }
    thread::sleep(Duration::from_millis(120));
    unsafe {
        SetCursorPos((area.left + area.right) / 2, (area.top + area.bottom) / 2);
        SetWindowPos(
            h,
            null_mut(),
            area.left + 25,
            area.top + 25,
            0,
            0,
            SWP_NOZORDER | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    thread::sleep(Duration::from_millis(150));
    unsafe {
        NotifyWinEvent(EVENT_SYSTEM_MOVESIZEEND, h, 0, 0);
    }
    wait(|| {
        let [x, y, r, b] = dialog.rect();
        x >= area.left && y >= area.top && r <= area.right && b <= area.bottom
    });
    d.stop();
    assert_eq!(dialog.rect(), before);
    assert_eq!(owner.rect(), original)
}

#[test]
#[ignore]
fn state_json_matches_real_windows() {
    aware();
    let ws = [
        Sentinel::new(0, 0),
        Sentinel::new(1, 0),
        Sentinel::new(2, 0),
    ];
    let mut d = Daemon::new("state-json");
    d.start();
    assert!(
        wait_settled(&d, ws.len(), Duration::from_millis(2500)),
        "state-json daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(80));

    let out = d.call_output(&["state", "--json"]);
    assert!(
        out.status.success(),
        "planor state --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid state JSON");
    assert_eq!(v["schema_version"].as_u64(), Some(1));
    let cfg_ver = v["config_version"].as_u64().expect("config_version");
    assert!(cfg_ver >= 1, "expected config_version >= 1, got {cfg_ver}");
    assert_eq!(v["test_mode"].as_bool(), Some(true));
    assert!(!v["monitors"].as_array().unwrap().is_empty());
    assert!(!v["strips"].as_array().unwrap().is_empty());
    let tiles = v["tiles"].as_array().expect("tiles array");
    assert_eq!(tiles.len(), ws.len(), "expected 3 tiled sentinels in state");
    let park_x = v["park_x"].as_i64().expect("park_x") as i32;

    for (i, w) in ws.iter().enumerate() {
        let tile = tiles
            .iter()
            .find(|t| t["id"].as_i64() == Some(w.hwnd as i64))
            .unwrap_or_else(|| panic!("sentinel {i} missing from state tiles"));
        let parked = tile["parked"].as_bool().unwrap();
        let vis = w.visible();
        if parked {
            assert!(
                vis.left >= park_x - 2,
                "parked sentinel {i} expected at park_x >= {park_x}, observed left {}",
                vis.left
            );
        } else {
            let er = &tile["expected_rect"];
            let (ex, ey, ew, eh) = (
                er["x"].as_i64().unwrap() as i32,
                er["y"].as_i64().unwrap() as i32,
                er["w"].as_i64().unwrap() as i32,
                er["h"].as_i64().unwrap() as i32,
            );
            let (ow, oh) = (vis.right - vis.left, vis.bottom - vis.top);
            assert!(
                (vis.left - ex).abs() <= 2
                    && (vis.top - ey).abs() <= 2
                    && (ow - ew).abs() <= 2
                    && (oh - eh).abs() <= 2,
                "sentinel {i} visible rect ({},{},{ow},{oh}) != expected ({ex},{ey},{ew},{eh})",
                vis.left,
                vis.top
            );
        }
    }

    assert!(d.call("reload"));
    let out2 = d.call_output(&["state", "--json"]);
    assert!(out2.status.success());
    let v2: serde_json::Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(v2["config_version"].as_u64(), Some(cfg_ver + 1));
    d.stop();
}

#[test]
#[ignore]
fn perf_spans_and_budgets() {
    aware();
    let ws = [
        Sentinel::new(0, 0),
        Sentinel::new(1, 0),
        Sentinel::new(2, 0),
    ];
    let mut d = Daemon::new("perf-check");
    d.start();
    assert!(
        wait_settled(&d, ws.len(), Duration::from_millis(2500)),
        "perf-check daemon did not settle within 2.5 s"
    );
    thread::sleep(Duration::from_millis(80));

    // Exercise warm scene + gesture-to-place path via IPC keys.
    press(&[ALT], LEFT);
    press(&[ALT], LEFT);
    press(&[ALT], RIGHT);
    thread::sleep(Duration::from_millis(200));

    let out = d.call_output(&["perf", "--json"]);
    assert!(
        out.status.success(),
        "planor perf --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let raw = String::from_utf8_lossy(&out.stdout).into_owned();
    let v: serde_json::Value = serde_json::from_str(&raw).expect("valid perf JSON");
    println!("PERF_JSON {}", serde_json::to_string(&v).unwrap());

    assert_eq!(v["schema_version"].as_u64(), Some(1));
    assert!(v["process"]["working_set_mb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(v["process"]["os_handles"].as_u64().unwrap_or(0) > 0);
    let spans = v["spans"].as_array().expect("spans array");
    // GPU profiling: overlay draws timed on the GPU, and the DWM/GPU counters sampled.
    let gpu = &v["process"]["gpu"];
    assert!(
        gpu["dwm_refresh_hz"].as_f64().unwrap_or(0.0) > 0.0,
        "perf report has no DWM refresh rate in process.gpu: {gpu}"
    );
    for required in [
        "daemon.tick",
        "daemon.layout",
        "render.scene",
        "win32.place",
        "gpu.draw",
    ] {
        assert!(
            spans.iter().any(|s| s["name"].as_str() == Some(required)),
            "missing expected span {required} in perf report"
        );
    }
    assert_eq!(
        v["budgets"]["all_pass"].as_bool(),
        Some(true),
        "perf budget breached: {}",
        v["budgets"]
    );

    let trace_path = d.dir.join("perf.trace.json");
    let tr_out = d.call_output(&[
        "perf",
        "--trace",
        trace_path.to_str().unwrap(),
        "--seconds",
        "60",
    ]);
    assert!(tr_out.status.success());
    let tr: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&trace_path).unwrap()).unwrap();
    assert_eq!(tr["clock"].as_str(), Some("qpc_us"));
    assert!(!tr["traceEvents"].as_array().unwrap().is_empty());

    assert!(d.call_args(&["perf", "--reset"]));
    d.stop();
}

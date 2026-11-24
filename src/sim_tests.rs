use crate::backend::sim::{FakeBackend, SimDesktop, SimWindow};
use crate::backend::WindowBackend;
use crate::daemon::{Engine, Journal};
use crate::win32::Event;
use crate::{render, Command, EXAMPLE};
use crossbeam_channel::{Receiver, Sender};
use planor::Rect;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_SHOW,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND,
    EVENT_SYSTEM_MOVESIZESTART,
};

static NEXT_SIM_DIR: AtomicU64 = AtomicU64::new(1);

fn test_cfg() -> toml::Value {
    let mut cfg: toml::Value = toml::from_str(EXAMPLE).expect("parse planor.example.toml");
    cfg["animation"]["enabled"] = toml::Value::Boolean(false);
    cfg["diagnostics"]["enabled"] = toml::Value::Boolean(false);
    cfg
}

fn three_monitor_rig() -> Vec<(String, Rect, Rect)> {
    vec![
        (
            "MON0".into(),
            Rect { x: 0, y: 0, w: 1920, h: 1152 },
            Rect { x: 0, y: 0, w: 1920, h: 1200 },
        ),
        (
            "MON1".into(),
            Rect { x: 1920, y: 0, w: 2560, h: 1392 },
            Rect { x: 1920, y: 0, w: 2560, h: 1440 },
        ),
        (
            "MON2".into(),
            Rect { x: -1440, y: 0, w: 1440, h: 2512 },
            Rect { x: -1440, y: 0, w: 1440, h: 2560 },
        ),
    ]
}

fn primary_only_rig() -> Vec<(String, Rect, Rect)> {
    vec![three_monitor_rig().into_iter().next().unwrap()]
}

struct SimHarness {
    engine: Engine<FakeBackend>,
    desktop: FakeBackend,
    _reload_rx: Receiver<toml::Value>,
    reload_tx: Sender<toml::Value>,
    event_rx: Receiver<Event>,
    rendered_epoch: Arc<AtomicU64>,
    rendered_overlay: Arc<AtomicU64>,
    renderer_join: Option<JoinHandle<()>>,
    dir: PathBuf,
    stopped: bool,
}

impl SimHarness {
    #[allow(dead_code)]
    fn new(monitors: Vec<(String, Rect, Rect)>) -> Self {
        Self::with_setup(monitors, test_cfg(), |_| {})
    }

    fn with_setup(
        monitors: Vec<(String, Rect, Rect)>,
        cfg: toml::Value,
        setup: impl FnOnce(&mut SimDesktop),
    ) -> Self {
        let id = NEXT_SIM_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("planor-sim-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let journal_path = dir.join("recovery.json");

        let mut sim = SimDesktop::new(monitors, cfg.clone());
        setup(&mut sim);
        let backend = FakeBackend::new(sim);

        let (render_tx, render_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (reload_tx, reload_rx) = crossbeam_channel::unbounded();
        let rendered_epoch = Arc::new(AtomicU64::new(0));
        let rendered_overlay = Arc::new(AtomicU64::new(0));

        let desktop_render = backend.clone();
        let epoch_render = rendered_epoch.clone();
        let overlay_render = rendered_overlay.clone();
        let event_render = event_tx.clone();
        let renderer_join = thread::spawn(move || {
            while let Ok(msg) = render_rx.recv() {
                match msg {
                    render::Message::Stop(ack) => {
                        let _ = ack.send(());
                        break;
                    }
                    render::Message::Clear(ack) => {
                        desktop_render.0.lock().unwrap().peeks.clear();
                        let _ = ack.send(());
                    }
                    render::Message::Warm(_) => {}
                    render::Message::OverlayUpdate { update_id, holes, colors } => {
                        let mut g = desktop_render.0.lock().unwrap();
                        g.overlay_updates += 1;
                        if let Some(last) = g.scenes.last_mut() {
                            last.holes = holes.to_vec();
                            last.tile_colors = colors;
                        }
                        drop(g);
                        overlay_render.store(update_id, Ordering::Release);
                    }
                    render::Message::Scene(scene) => {
                        let rejected = desktop_render.0.lock().unwrap().apply_scene(&scene);
                        if !rejected.is_empty() {
                            let _ = event_render.send(Event::Rejected(scene.epoch, rejected));
                        }
                        epoch_render.store(scene.epoch, Ordering::Release);
                    }
                }
            }
        });

        let mut journal = Journal::open(journal_path).unwrap();
        journal.restore(&backend).unwrap();
        let mut engine = Engine::new_for_test(backend.clone(), cfg, journal, render_tx);
        engine.topology();
        if engine.cfg["layout"]["tile_existing_on_startup"].as_bool() == Some(true) {
            engine.scan();
        } else {
            let existing = engine.backend.windows();
            engine.skipped.extend(existing);
        }

        let mut h = Self {
            engine,
            desktop: backend,
            _reload_rx: reload_rx,
            reload_tx,
            event_rx,
            rendered_epoch,
            rendered_overlay,
            renderer_join: Some(renderer_join),
            dir,
            stopped: false,
        };
        h.settle();
        h
    }

    fn cmd(&mut self, c: Command) -> String {
        self.engine
            .command(c, &self.reload_tx)
            .expect("sim command ok")
    }

    fn tick_now(&mut self) {
        self.engine.expire_timers_for_test();
        self.engine.tick().unwrap();
        self.settle();
    }

    fn settle(&mut self) {
        self.settle_inner(true)
    }
    /// Settle without the os_rect == expected_rect check (mid-drag, the carried window is where the mouse is).
    fn settle_quiet(&mut self) {
        self.settle_inner(false)
    }
    fn settle_inner(&mut self, check: bool) {
        for _ in 0..16 {
            let mut progressed = false;
            if self.engine.dirty {
                let prev = self.engine.seq;
                let prev_ov = self.engine.overlay_seq;
                self.engine.layout().unwrap();
                if self.engine.seq > prev {
                    let target = self.engine.seq;
                    let until = Instant::now() + Duration::from_secs(2);
                    while self.rendered_epoch.load(Ordering::Acquire) < target {
                        assert!(Instant::now() < until, "sim renderer timed out at seq {target}");
                        thread::yield_now();
                    }
                }
                if self.engine.overlay_seq > prev_ov {
                    let target = self.engine.overlay_seq;
                    let until = Instant::now() + Duration::from_secs(2);
                    while self.rendered_overlay.load(Ordering::Acquire) < target {
                        assert!(Instant::now() < until, "sim renderer timed out at overlay {target}");
                        thread::yield_now();
                    }
                }
                progressed = true;
            }
            while let Ok(ev) = self.event_rx.try_recv() {
                self.engine.event(ev);
                progressed = true;
            }
            if !progressed && !self.engine.dirty {
                break;
            }
        }
        assert!(
            !self.engine.dirty || self.engine.paused || self.engine.locked || self.engine.deadline.is_some(),
            "engine stayed dirty after settle iterations"
        );
        if check {
            self.assert_invariants_and_sync();
        }
    }

    fn state(&self) -> serde_json::Value {
        serde_json::from_str(&self.engine.state_snapshot(true).unwrap()).unwrap()
    }

    fn assert_invariants_and_sync(&self) {
        if self.engine.paused || self.engine.locked || self.engine.deadline.is_some() {
            return;
        }
        let stack_peek = self.engine.cfg["layout"]["stack_peek_px"]
            .as_integer()
            .unwrap_or(28) as i32;
        for ((desk, _), strip) in &self.engine.strips {
            if Some(*desk) != self.engine.desktop {
                continue;
            }
            let violations = planor::check_strip_invariants(strip, stack_peek);
            assert!(
                violations.is_empty(),
                "strip invariant violations: {:?}",
                violations.iter().map(|v| v.trace_line()).collect::<Vec<_>>()
            );
        }
        let snap = self.state();
        assert_eq!(snap["schema_version"].as_u64(), Some(1));
        let park_x = snap["park_x"].as_i64().unwrap() as i32;
        for tile in snap["tiles"].as_array().unwrap() {
            if tile["pinned"].as_bool() == Some(true) {
                continue;
            }
            let parked = tile["parked"].as_bool().unwrap();
            let os = &tile["os_rect"];
            if os.is_null() {
                continue;
            }
            if parked {
                assert_eq!(
                    os["x"].as_i64().unwrap() as i32,
                    park_x,
                    "parked tile {} not at park_x {park_x}: {tile}",
                    tile["id"]
                );
            } else {
                assert_eq!(
                    os, &tile["expected_rect"],
                    "visible tile {} os_rect != expected_rect",
                    tile["id"]
                );
            }
        }
    }

    fn rect(&self, id: isize) -> Rect {
        self.desktop.frame(id).unwrap()
    }

    fn stop_clean(&mut self) {
        if !self.stopped {
            self.cmd(Command::Stop);
            self.stopped = true;
        }
    }
}

impl Drop for SimHarness {
    fn drop(&mut self) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = self.engine.renderer.send(render::Message::Stop(tx));
        let _ = rx.recv_timeout(Duration::from_secs(2));
        if let Some(j) = self.renderer_join.take() {
            let _ = j.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn three_monitor_rig_landscape_landscape_portrait_startup_and_orientation() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(101, Rect { x: 100, y: 100, w: 600, h: 400 }));
        sim.add_window(SimWindow::new(102, Rect { x: 500, y: 100, w: 600, h: 400 }));
        sim.add_window(SimWindow::new(201, Rect { x: 2050, y: 100, w: 800, h: 600 }));
        sim.add_window(SimWindow::new(301, Rect { x: -1300, y: 100, w: 900, h: 600 }));
        sim.add_window(SimWindow::new(302, Rect { x: -1300, y: 900, w: 900, h: 600 }));
        sim.foreground = 101;
    });

    // Portrait strip (MON2) uses vertical layout; landscape strips (MON0, MON1) use horizontal layout.
    let s0 = &h.engine.strips[&(1, "MON0".into())];
    let s1 = &h.engine.strips[&(1, "MON1".into())];
    let s2 = &h.engine.strips[&(1, "MON2".into())];
    assert!(s0.area.horizontal());
    assert!(s1.area.horizontal());
    assert!(!s2.area.horizontal());

    let r301 = h.rect(301);
    let r302 = h.rect(302);
    assert_eq!(r301.x, -1440);
    assert_eq!(r302.x, -1440);
    assert_eq!(r301.w, 1440);
    assert!(r302.y >= r301.y + r301.h + 6, "portrait stack should flow top-to-bottom: {r301:?} {r302:?}");

    // Directional focus walk across monitor boundary: MON0 leftmost -> focus_left -> MON2 -> focus_right -> MON0 -> MON1.
    h.cmd(Command::Key { name: "focus_left".into() });
    h.settle();
    assert_eq!(h.engine.active, "MON2");
    assert!(matches!(h.desktop.foreground(), 301 | 302));

    h.cmd(Command::Key { name: "focus_right".into() });
    h.settle();
    assert_eq!(h.engine.active, "MON0");
    assert_eq!(h.desktop.foreground(), 101);

    h.cmd(Command::Key { name: "focus_right".into() });
    h.settle();
    assert_eq!(h.desktop.foreground(), 102);

    h.cmd(Command::Key { name: "focus_right".into() });
    h.settle();
    assert_eq!(h.engine.active, "MON1");
    assert_eq!(h.desktop.foreground(), 201);

    // Explicit monitor cycle hotkeys.
    h.cmd(Command::Key { name: "focus_monitor_prev".into() });
    h.settle();
    assert_eq!(h.engine.active, "MON0");
    h.cmd(Command::Key { name: "focus_monitor_prev".into() });
    h.settle();
    assert_eq!(h.engine.active, "MON2");
    h.cmd(Command::Key { name: "focus_monitor_next".into() });
    h.settle();
    assert_eq!(h.engine.active, "MON0");

    let snap = h.state();
    assert_eq!(snap["monitors"].as_array().unwrap().len(), 3);
    assert_eq!(snap["tiles"].as_array().unwrap().len(), 5);
    assert!(h.desktop.0.lock().unwrap().scenes.last().unwrap().tiles_len == 5);
    assert!(h.desktop.0.lock().unwrap().scenes.last().unwrap().epoch >= 1);
    h.stop_clean();
}

#[test]
fn three_monitor_cross_monitor_drag_landscape_to_portrait_with_owned_dialog() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(101, Rect { x: 100, y: 100, w: 600, h: 400 }));
        sim.add_window(SimWindow::new(102, Rect { x: 800, y: 100, w: 600, h: 400 }));
        let mut dlg = SimWindow::new(901, Rect { x: 250, y: 200, w: 300, h: 180 });
        dlg.owner = 101;
        sim.add_window(dlg);
        sim.add_window(SimWindow::new(301, Rect { x: -1300, y: 100, w: 900, h: 600 }));
        sim.foreground = 101;
    });

    // Owned modal must NOT be placed into any strip.
    assert!(!h.engine.strips.values().any(|s| s.locate(901).is_some()));

    {
        let mut g = h.desktop.0.lock().unwrap();
        g.cursor = (400, 120);
    }
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
    h.settle();

    // Drag owner 101 across onto portrait monitor MON2 (outside target 301).
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&101).unwrap().rect.x = -1100;
        g.windows.get_mut(&101).unwrap().rect.y = 2000;
        g.cursor = (-900, 2400);
    }
    h.engine.tick().unwrap();
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
    h.settle();

    assert_eq!(h.engine.active, "MON2");
    assert!(h.engine.strips[&(1, "MON2".into())].locate(101).is_some());
    assert!(h.engine.strips[&(1, "MON0".into())].locate(101).is_none());
    // Single remaining window 102 on MON0 fills the whole work area via `fill()`.
    assert_eq!(h.rect(102), Rect { x: 0, y: 0, w: 1920, h: 1152 });
    // Owned modal 901 transferred inside MON2 work area and captured in journal for restore.
    let dlg_r = h.rect(901);
    assert!(dlg_r.x >= -1440 && dlg_r.x + dlg_r.w <= 0, "modal not on portrait monitor: {dlg_r:?}");
    assert!(dlg_r.y >= 0 && dlg_r.y + dlg_r.h <= 2512);
    assert!(h.engine.journal.entries.contains_key(&901));

    h.stop_clean();
    // Exact original rects restored on stop for owner and modal.
    assert_eq!(h.rect(101), Rect { x: 100, y: 100, w: 600, h: 400 });
    assert_eq!(h.rect(901), Rect { x: 250, y: 200, w: 300, h: 180 });
}

fn setup_multi_monitor_windows(sim: &mut SimDesktop) {
    sim.add_window(SimWindow::new(101, Rect { x: 100, y: 100, w: 600, h: 400 }));
    sim.add_window(SimWindow::new(102, Rect { x: 500, y: 100, w: 600, h: 400 }));
    sim.add_window(SimWindow::new(103, Rect { x: 900, y: 100, w: 600, h: 400 }));
    sim.add_window(SimWindow::new(201, Rect { x: 2100, y: 120, w: 700, h: 500 }));
    sim.add_window(SimWindow::new(301, Rect { x: -1200, y: 120, w: 700, h: 500 }));
    sim.foreground = 101;
}

#[test]
fn scn_display_offline_15s_round_trip() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), test_cfg(), setup_multi_monitor_windows);
    let settled_rects: Vec<(isize, Rect)> = [101, 102, 103, 201, 301]
        .into_iter()
        .map(|id| (id, h.rect(id)))
        .collect();

    // Disconnect MON1 and MON2.
    h.desktop.0.lock().unwrap().monitors = primary_only_rig();
    h.engine.event(Event::Topology);
    assert!(h.engine.deadline.is_some());
    h.tick_now();

    // Secondary/tertiary windows become minimized orphans with homes recorded.
    assert!(h.engine.orphans.contains(&201));
    assert!(h.engine.orphans.contains(&301));
    assert_eq!(h.engine.homes.get(&201).map(String::as_str), Some("MON1"));
    assert_eq!(h.engine.homes.get(&301).map(String::as_str), Some("MON2"));
    assert!(h.desktop.is_iconic(201));
    assert!(h.desktop.is_iconic(301));
    // Primary windows stay untroubled, tiled and navigable.
    assert!(!h.desktop.is_iconic(101));
    h.cmd(Command::Key { name: "focus_right".into() });
    h.settle();
    assert_eq!(h.desktop.foreground(), 102);

    // Reconnect MON1 and MON2 -> orphans un-minimized and returned to original monitors & settled rects.
    h.desktop.0.lock().unwrap().monitors = three_monitor_rig();
    h.engine.event(Event::Topology);
    h.tick_now();

    assert!(h.engine.orphans.is_empty());
    assert!(h.engine.homes.is_empty());
    assert!(!h.desktop.is_iconic(201));
    assert!(!h.desktop.is_iconic(301));
    assert!(h.engine.strips[&(1, "MON1".into())].locate(201).is_some());
    assert!(h.engine.strips[&(1, "MON2".into())].locate(301).is_some());
    for (id, want) in settled_rects {
        assert_eq!(h.rect(id), want, "window {id} mismatch after round trip");
    }
    h.stop_clean();
}

#[test]
fn scn_display_short_blip_3s_and_delayed_taskbar_work_area() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), test_cfg(), setup_multi_monitor_windows);

    h.desktop.0.lock().unwrap().monitors = primary_only_rig();
    h.engine.event(Event::Topology);
    h.tick_now();
    assert_eq!(h.engine.orphans.len(), 2);

    // Reconnect initially reporting full height before Explorer reserves taskbar (work h = 1200).
    let mut pre_taskbar = three_monitor_rig();
    pre_taskbar[0].1.h = 1200;
    h.desktop.0.lock().unwrap().monitors = pre_taskbar;
    h.engine.event(Event::Topology);
    h.tick_now();
    assert_eq!(h.rect(101).h, 1200);

    // Now Explorer reserves the taskbar (work h = 1152); tick() monitor-poll detects the mismatch without WM_DISPLAYCHANGE.
    h.desktop.0.lock().unwrap().monitors = three_monitor_rig();
    h.tick_now();
    assert!(h.engine.deadline.is_some());
    h.tick_now();
    assert_eq!(h.rect(101).h, 1152);
    assert_eq!(h.rect(201).h, 1392);
    h.stop_clean();
}

#[test]
fn scn_display_pull_orphans_then_reconnect_returns_home() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), test_cfg(), setup_multi_monitor_windows);

    h.desktop.0.lock().unwrap().monitors = primary_only_rig();
    h.engine.event(Event::Topology);
    h.tick_now();

    h.cmd(Command::PullOrphans);
    h.settle();
    assert!(h.engine.orphans.is_empty());
    assert!(!h.desktop.is_iconic(201));
    assert!(!h.desktop.is_iconic(301));
    assert!(h.engine.strips[&(1, "MON0".into())].locate(201).is_some());
    assert!(h.engine.strips[&(1, "MON0".into())].locate(301).is_some());
    // Homes stay remembered so reconnecting MON1 & MON2 moves them back automatically.
    assert_eq!(h.engine.homes.len(), 2);

    h.desktop.0.lock().unwrap().monitors = three_monitor_rig();
    h.engine.event(Event::Topology);
    h.tick_now();
    assert!(h.engine.homes.is_empty());
    assert!(h.engine.strips[&(1, "MON0".into())].locate(201).is_none());
    assert!(h.engine.strips[&(1, "MON1".into())].locate(201).is_some());
    assert!(h.engine.strips[&(1, "MON2".into())].locate(301).is_some());
    h.stop_clean();
}

#[test]
fn scn_display_offline_then_stop_restores_all_windows() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), test_cfg(), setup_multi_monitor_windows);
    let journal_path = h.engine.journal.path.clone();
    assert!(journal_path.exists());

    h.desktop.0.lock().unwrap().monitors = primary_only_rig();
    h.engine.event(Event::Topology);
    h.tick_now();
    assert!(h.desktop.is_iconic(201));

    h.stop_clean();
    assert!(!journal_path.exists());
    assert!(!h.desktop.is_iconic(201));
    assert_eq!(h.rect(101), Rect { x: 100, y: 100, w: 600, h: 400 });
    assert_eq!(h.rect(201), Rect { x: 2100, y: 120, w: 700, h: 500 });
    assert_eq!(h.rect(301), Rect { x: -1200, y: 120, w: 700, h: 500 });
}

#[test]
fn scn_display_offline_then_kill_recover() {
    let journal_dir = std::env::temp_dir().join(format!("planor-sim-kill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&journal_dir);
    std::fs::create_dir_all(&journal_dir).unwrap();
    let journal_path = journal_dir.join("recovery.json");

    let cfg = test_cfg();
    let mut sim = SimDesktop::new(three_monitor_rig(), cfg.clone());
    setup_multi_monitor_windows(&mut sim);
    let backend = FakeBackend::new(sim);

    {
        let (rtx, rrx) = crossbeam_channel::unbounded();
        let desktop_r = backend.clone();
        let epoch_r = Arc::new(AtomicU64::new(0));
        let epoch_w = epoch_r.clone();
        let jh = thread::spawn(move || {
            while let Ok(msg) = rrx.recv() {
                match msg {
                    render::Message::Stop(ack) => {
                        let _ = ack.send(());
                        break;
                    }
                    render::Message::Clear(ack) => {
                        let _ = ack.send(());
                    }
                    render::Message::Warm(_) | render::Message::OverlayUpdate { .. } => {}
                    render::Message::Scene(s) => {
                        desktop_r.0.lock().unwrap().apply_scene(&s);
                        epoch_w.store(s.epoch, Ordering::Release);
                    }
                }
            }
        });
        let journal = Journal::open(journal_path.clone()).unwrap();
        let mut engine = Engine::new_for_test(backend.clone(), cfg.clone(), journal, rtx);
        engine.topology();
        engine.scan();
        engine.layout().unwrap();
        while epoch_r.load(Ordering::Acquire) < engine.seq {
            thread::yield_now();
        }
        backend.0.lock().unwrap().monitors = primary_only_rig();
        engine.event(Event::Topology);
        engine.expire_timers_for_test();
        engine.tick().unwrap();
        engine.layout().unwrap();
        assert!(backend.is_iconic(201));
        assert!(journal_path.exists());
        // Simulate SIGKILL: drop Engine without calling Stop / journal.restore.
        let (stx, srx) = crossbeam_channel::bounded(1);
        let _ = engine.renderer.send(render::Message::Stop(stx));
        let _ = srx.recv();
        let _ = jh.join();
    }

    // Monitors reconnect before Planor restarts with `tile_existing_on_startup = false`.
    backend.0.lock().unwrap().monitors = three_monitor_rig();
    let mut restart_cfg = test_cfg();
    restart_cfg["layout"]["tile_existing_on_startup"] = toml::Value::Boolean(false);
    let mut journal = Journal::open(journal_path.clone()).unwrap();
    journal.restore(&backend).unwrap();
    assert!(!journal_path.exists());

    assert!(!backend.is_iconic(201));
    assert!(!backend.is_iconic(301));
    assert_eq!(backend.frame(101).unwrap(), Rect { x: 100, y: 100, w: 600, h: 400 });
    assert_eq!(backend.frame(201).unwrap(), Rect { x: 2100, y: 120, w: 700, h: 500 });
    assert_eq!(backend.frame(301).unwrap(), Rect { x: -1200, y: 120, w: 700, h: 500 });

    let (rtx2, rrx2) = crossbeam_channel::unbounded();
    let jh2 = thread::spawn(move || {
        while let Ok(msg) = rrx2.recv() {
            match msg {
                render::Message::Stop(ack) => {
                    let _ = ack.send(());
                    break;
                }
                render::Message::Clear(ack) => {
                    let _ = ack.send(());
                }
                _ => {}
            }
        }
    });
    let mut engine2 = Engine::new_for_test(backend.clone(), restart_cfg, journal, rtx2);
    engine2.topology();
    let existing = engine2.backend.windows();
    engine2.skipped.extend(existing);
    engine2.layout().unwrap();
    assert!(engine2.strips.is_empty(), "expected no re-tiling on startup");
    let (stx, srx) = crossbeam_channel::bounded(1);
    let _ = engine2.renderer.send(render::Message::Stop(stx));
    let _ = srx.recv();
    let _ = jh2.join();
    let _ = std::fs::remove_dir_all(&journal_dir);
}

#[test]
fn scn_display_topology_while_dragging_cancels_cleanly() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), test_cfg(), setup_multi_monitor_windows);
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
    assert!(h.engine.drag.is_some());

    // Mid-drag topology change drops active drag and sets settle deadline.
    h.desktop.0.lock().unwrap().monitors = primary_only_rig();
    h.engine.event(Event::Topology);
    assert!(h.engine.drag.is_none());
    assert!(h.engine.deadline.is_some());

    // Stale drag end after topology must be ignored cleanly.
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
    h.tick_now();

    assert!(h.engine.orphans.contains(&201));
    h.desktop.0.lock().unwrap().monitors = three_monitor_rig();
    h.engine.event(Event::Topology);
    h.tick_now();
    assert!(h.engine.orphans.is_empty());
    h.stop_clean();
}

#[test]
fn sim_focus_stack_move_pin_fullscreen_rejection_holes_desktop_and_panic_exit() {
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        for i in 1..=4 {
            let mut w = SimWindow::new(i, Rect { x: 50 + (i as i32) * 80, y: 80, w: 600, h: 400 });
            if i == 2 {
                w.min_enforced_w = 1100; // simulates app rejecting <1100 px width
            }
            sim.add_window(w);
        }
        // Unmanaged external window carved as overlay hole.
        let mut ext = SimWindow::new(900, Rect { x: 100, y: 100, w: 300, h: 300 });
        ext.class = "TaskManagerWindow".into();
        ext.process = "Taskmgr.exe".into();
        ext.thickframe = false;
        sim.add_window(ext);
        // Window on desktop 2 (initially hidden from desktop 1).
        let mut d2 = SimWindow::new(800, Rect { x: 50, y: 50, w: 700, h: 500 });
        d2.desktop = 2;
        sim.add_window(d2);
        sim.foreground = 1;
    });

    // External window 900 excluded from strip and carved into scene holes.
    assert!(!h.engine.strips[&(1, "MON0".into())].columns.iter().any(|c| c.members.contains(&900)));
    assert!(h.desktop.0.lock().unwrap().scenes.last().unwrap().holes.contains(&Rect { x: 100, y: 100, w: 300, h: 300 }));

    // Rejection widening bumped column containing 2 to >= 1100 px.
    assert!(h.rect(2).w >= 1100, "expected rejected width adjustment: {:?}", h.rect(2));

    // Stack toggle merges focused window with neighbour; vertical focus cycles group members; second stack_toggle ungroups.
    h.cmd(Command::Key { name: "focus_right".into() });
    h.settle();
    let focused_before_stack = h.desktop.foreground();
    h.cmd(Command::Key { name: "stack_toggle".into() });
    h.settle();
    let s = &h.engine.strips[&(1, "MON0".into())];
    assert!(s.columns[s.focus].members.len() >= 2);
    h.cmd(Command::Key { name: "focus_down".into() });
    h.settle();
    let cycled = h.desktop.foreground();
    assert_ne!(cycled, focused_before_stack);
    h.cmd(Command::Key { name: "stack_toggle".into() });
    h.settle();

    // Move column left then right swaps columns cleanly.
    let col_order_before: Vec<isize> = h.engine.strips[&(1, "MON0".into())].columns.iter().map(|c| c.members[0]).collect();
    h.cmd(Command::Key { name: "move_left".into() });
    h.settle();
    let col_order_after: Vec<isize> = h.engine.strips[&(1, "MON0".into())].columns.iter().map(|c| c.members[0]).collect();
    assert_ne!(col_order_before, col_order_after);

    // Pin toggle sets topmost + removes from strip, then second toggle restores to strip.
    let pin_id = h.desktop.foreground();
    h.cmd(Command::Key { name: "toggle_pin".into() });
    h.settle();
    assert!(h.engine.pinned.contains(&pin_id));
    assert!(h.desktop.0.lock().unwrap().windows[&pin_id].topmost);
    h.cmd(Command::Key { name: "toggle_pin".into() });
    h.settle();
    assert!(!h.engine.pinned.contains(&pin_id));
    assert!(!h.desktop.0.lock().unwrap().windows[&pin_id].topmost);

    // Borderless fullscreen enter + exit via EVENT_OBJECT_LOCATIONCHANGE.
    let full_id = h.desktop.foreground();
    // Maximize toggle via IsZoomed + EVENT_OBJECT_LOCATIONCHANGE.
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&full_id).unwrap().zoomed = true;
    }
    h.engine.event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, full_id));
    h.settle();
    assert!(!h.desktop.is_zoomed(full_id));
    assert_eq!(h.rect(full_id).w, 1920);
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&full_id).unwrap().zoomed = true;
    }
    h.engine.event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, full_id));
    h.settle();
    assert!(h.rect(full_id).w < 1920);

    {
        let mut g = h.desktop.0.lock().unwrap();
        let w = g.windows.get_mut(&full_id).unwrap();
        w.rect = Rect { x: 0, y: 0, w: 1920, h: 1200 };
        w.caption = false;
    }
    h.engine.event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, full_id));
    h.settle();
    assert_eq!(h.engine.strips[&(1, "MON0".into())].fullscreen_id(), Some(full_id));
    assert_eq!(h.rect(full_id), Rect { x: 0, y: 0, w: 1920, h: 1200 });
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&full_id).unwrap().caption = true;
    }
    h.engine.event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, full_id));
    h.settle();
    assert_eq!(h.engine.strips[&(1, "MON0".into())].fullscreen_id(), None);

    // Focusing an off-screen/parked window via EVENT_SYSTEM_FOREGROUND reveals it onto the monitor.
    h.desktop.0.lock().unwrap().foreground = 1;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 1));
    h.settle();
    assert!(h.rect(1).x >= 0 && h.rect(1).x + h.rect(1).w <= 1920);

    // Dynamic new window open, minimize, restore & destroy.
    let new_id = h.desktop.0.lock().unwrap().add_window(SimWindow::new(50, Rect { x: 200, y: 200, w: 500, h: 350 }));
    h.engine.event(Event::Window(EVENT_OBJECT_SHOW, new_id));
    h.settle();
    assert!(h.engine.strips[&(1, "MON0".into())].locate(new_id).is_some());
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&new_id).unwrap().iconic = true;
    }
    h.engine.event(Event::Window(EVENT_SYSTEM_MINIMIZESTART, new_id));
    h.settle();
    assert!(h.engine.strips[&(1, "MON0".into())].locate(new_id).is_none());
    {
        let mut g = h.desktop.0.lock().unwrap();
        let w = g.windows.get_mut(&new_id).unwrap();
        w.iconic = false;
        w.visible = false;
    }
    h.engine.event(Event::Window(EVENT_OBJECT_HIDE, new_id));
    h.settle();
    assert!(h.engine.strips[&(1, "MON0".into())].locate(new_id).is_none());
    h.engine.event(Event::Window(EVENT_OBJECT_DESTROY, new_id));
    h.settle();

    // Virtual desktop switch discovers window 800 on desktop 2.
    h.desktop.0.lock().unwrap().current_desktop = Some(2);
    h.tick_now();
    assert_eq!(h.engine.desktop, Some(2));
    assert_eq!(h.rect(800), Rect { x: 0, y: 0, w: 1920, h: 1152 });

    // Switch back to desktop 1 & trigger panic_exit -> journal restores all captured rects & deletes journal.
    h.desktop.0.lock().unwrap().current_desktop = Some(1);
    h.tick_now();
    let jpath = h.engine.journal.path.clone();
    h.cmd(Command::Key { name: "panic_exit".into() });
    assert!(!jpath.exists());
    assert_eq!(h.rect(1), Rect { x: 130, y: 80, w: 600, h: 400 });
    assert_eq!(h.rect(800), Rect { x: 50, y: 50, w: 700, h: 500 });
    h.stopped = true;
}

// ---- 2026-11-22 Win+W notes: the same controls on landscape and portrait monitors ----

fn note_cfg() -> toml::Value {
    let mut cfg = test_cfg();
    // Deterministic: no momentum after the synthetic lift (its speed is effectively infinite).
    cfg["touchpad"]["swipe_inertia"] = toml::Value::Boolean(false);
    cfg
}

/// Three windows on every monitor of the rig (MON0 landscape, MON1 landscape right of it, MON2 portrait left).
fn three_per_monitor(sim: &mut SimDesktop) {
    for i in 0..3 {
        sim.add_window(SimWindow::new(101 + i, Rect { x: 100 + 300 * i as i32, y: 100, w: 600, h: 400 }));
        sim.add_window(SimWindow::new(201 + i, Rect { x: 2100 + 300 * i as i32, y: 100, w: 600, h: 400 }));
        sim.add_window(SimWindow::new(301 + i, Rect { x: -1300, y: 100 + 500 * i as i32, w: 900, h: 400 }));
    }
    sim.foreground = 101;
}

/// Feed a touchpad swipe of `fingers` fingers from `from` to `to` in `steps` reports, then lift.
fn swipe(h: &mut SimHarness, fingers: usize, from: (f64, f64), to: (f64, f64), steps: usize) {
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let p = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
        h.engine.event(Event::Contacts(vec![p; fingers]));
        h.settle();
    }
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
}

fn settle_now(h: &mut SimHarness) {
    if h.engine.settle.is_some() {
        h.engine.settle = Some(Instant::now() - Duration::from_millis(1));
    }
    h.tick_now();
}

fn focus(h: &mut SimHarness, id: isize) {
    h.desktop.0.lock().unwrap().foreground = id;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, id));
    h.settle();
}

/// Focus the first window of a monitor's strip so it is scrolled to its start.
fn to_start(h: &mut SimHarness, mon: &str) {
    let first = h.engine.strips[&(1, mon.to_string())].columns[0].members[0];
    focus(h, first);
    assert_eq!(h.engine.strips[&(1, mon.to_string())].offset, 0, "{mon} scrolled to its start");
}

fn zones(h: &SimHarness) -> Vec<render::Zone> {
    h.desktop.0.lock().unwrap().scenes.last().unwrap().zones.clone()
}

#[test]
fn n014_portrait_scrolls_with_vertical_fingers_like_landscape_with_sideways() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let (land, port) = ((1, "MON0".to_string()), (1, "MON2".to_string()));
    // Landscape: sideways fingers scroll the strip continuously.
    to_start(&mut h, "MON0");
    let before = h.engine.strips[&land].offset;
    swipe(&mut h, 3, (300., 200.), (200., 200.), 10);
    settle_now(&mut h);
    let after = h.engine.strips[&land].offset;
    assert!(after > before, "landscape: fingers left (natural) scroll the strip on: {before} -> {after}");
    // Portrait: the same gesture turned with the monitor, fingers up scroll it on, continuously.
    to_start(&mut h, "MON2");
    let before = h.engine.strips[&port].offset;
    let mut seen = vec![before];
    for i in 0..=10 {
        h.engine.event(Event::Contacts(vec![(200., 300. - 10. * i as f64); 3]));
        h.settle();
        seen.push(h.engine.strips[&port].offset);
    }
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    let distinct = seen.windows(2).filter(|w| w[1] != w[0]).count();
    assert!(seen.last() > seen.first(), "portrait: fingers up scroll the strip on: {seen:?}");
    assert!(distinct >= 5, "portrait: the strip follows the fingers step by step (smooth), not one jump: {seen:?}");
    assert_eq!(h.engine.active, "MON2", "portrait: a vertical swipe no longer hops focus between monitors");
    settle_now(&mut h);
    assert_eq!(h.engine.active, "MON2");
    h.stop_clean();
}

#[test]
fn n011_cross_axis_swipe_hops_monitors_both_ways_once_per_gesture() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    // Portrait (MON2, left of MON0): sideways fingers are across its strip, so they hop to MON0 on the
    // right; they used to scroll the portrait strip and never let the focus go back right.
    focus(&mut h, 301);
    assert_eq!(h.engine.active, "MON2");
    // Natural direction: fingers moving left = content left = focus moves right.
    swipe(&mut h, 3, (400., 200.), (100., 200.), 12);
    settle_now(&mut h);
    assert_eq!(h.engine.active, "MON0", "portrait: a sideways swipe hops right to MON0");
    // Landscape MON0: vertical fingers are across its strip; with no monitor above or below nothing hops
    // and nothing scrolls.
    let before = h.engine.strips[&(1, "MON0".to_string())].offset;
    swipe(&mut h, 3, (200., 400.), (200., 100.), 12);
    settle_now(&mut h);
    assert_eq!(h.engine.active, "MON0");
    assert_eq!(h.engine.strips[&(1, "MON0".to_string())].offset, before, "vertical fingers never scroll a landscape strip");
    // A long sideways swipe from the portrait hops once; it does not run on into MON0's strip.
    focus(&mut h, 301);
    assert_eq!(h.engine.active, "MON2");
    let land_before = h.engine.strips[&(1, "MON0".to_string())].offset;
    swipe(&mut h, 3, (900., 200.), (0., 200.), 40);
    assert_eq!(h.engine.active, "MON0", "hopped right");
    assert_eq!(h.engine.strips[&(1, "MON0".to_string())].offset, land_before, "the rest of the gesture was ignored");
    h.stop_clean();
}

#[test]
fn n023_win_wheel_scrolls_the_strip_under_the_cursor_on_both_orientations() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let (land, port) = ((1, "MON0".to_string()), (1, "MON2".to_string()));
    let wheel = |h: &mut SimHarness, delta: i32, tilt: bool, shift: bool, x: i32, y: i32| {
        h.engine.event(Event::Wheel { delta, tilt, shift, x, y });
        h.settle();
    };
    // (Win+Shift+wheel rolls one whole member per notch, so the deck never rests half rolled.)
    to_start(&mut h, "MON2");
    to_start(&mut h, "MON0");
    // Landscape: wheel down scrolls the strip on (sideways).
    let before = h.engine.strips[&land].offset;
    wheel(&mut h, -120, false, false, 900, 500);
    assert!(h.engine.strips[&land].offset > before, "landscape wheel down scrolls on");
    // Portrait: the cursor decides the monitor; wheel down scrolls the vertical strip on.
    let before = h.engine.strips[&port].offset;
    wheel(&mut h, -120, false, false, -700, 1200);
    assert_eq!(h.engine.active, "MON2", "the strip under the cursor is the one that scrolls");
    assert!(h.engine.strips[&port].offset > before, "portrait wheel down scrolls the strip down");
    wheel(&mut h, 240, false, false, -700, 1200);
    assert!(h.engine.strips[&port].offset <= before, "wheel up scrolls back");
    settle_now(&mut h);
    let s = &h.engine.strips[&port];
    assert_eq!(h.desktop.foreground(), s.active().unwrap(), "focus settles on the window the wheel brought in");
    // Win + Shift + wheel rolls the focused group.
    focus(&mut h, 101);
    h.cmd(Command::Key { name: "stack_toggle".into() });
    h.settle();
    let s = &h.engine.strips[&land];
    assert!(s.columns[s.focus].members.len() >= 2);
    let active = s.active();
    wheel(&mut h, 120, false, true, 900, 500);
    wheel(&mut h, 120, false, true, 900, 500);
    settle_now(&mut h);
    let s = &h.engine.strips[&land];
    assert_ne!(s.active(), active, "Win+Shift+wheel rolled the group");
    h.stop_clean();
}

/// Drag `id` with the mouse: press on it, move the cursor to `to` (the window follows), but do not release.
fn mouse_drag(h: &mut SimHarness, id: isize, to: (i32, i32)) {
    let r = h.rect(id);
    h.desktop.0.lock().unwrap().cursor = (r.x + r.w / 2, r.y + 10);
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, id));
    h.settle_quiet();
    {
        let mut g = h.desktop.0.lock().unwrap();
        let w = g.windows.get_mut(&id).unwrap();
        w.rect.x = to.0 - r.w / 2;
        w.rect.y = to.1 - 10;
        g.cursor = to;
    }
    h.engine.tick().unwrap();
    h.settle_quiet();
}
fn drop_now(h: &mut SimHarness, id: isize) {
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, id));
    h.settle();
}

#[test]
fn n012_drop_on_a_window_swaps_and_the_small_centre_zone_groups_on_both_orientations() {
    for (mon, ids) in [("MON0", [101, 102, 103]), ("MON2", [301, 302, 303])] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        let key = (1, mon.to_string());
        focus(&mut h, ids[0]);
        let visible: Vec<isize> = ids.iter().copied().filter(|&i| h.engine.strips[&key].in_view(i)).collect();
        assert!(visible.len() >= 2, "{mon}: need two visible windows, got {visible:?}");
        let (a, b) = (visible[0], visible[1]);
        let order = |h: &SimHarness| h.engine.strips[&key].columns.iter().map(|c| c.members[0]).collect::<Vec<_>>();
        let before = order(&h);
        // Off-centre on b (outside the middle half, an eighth of the way in): swap. Nothing dodges while dragging.
        let rb = h.rect(b);
        mouse_drag(&mut h, a, (rb.x + rb.w / 8, rb.y + rb.h / 8));
        assert_eq!(h.engine.drag.as_ref().map(|d| d.mode), Some(planor::Drag::Swap), "{mon}: off-centre is a swap");
        let z = zones(&h);
        assert!(!z.is_empty(), "{mon}: stack zones are drawn while dragging");
        assert!(z.iter().all(|z| z.monitor == mon), "{mon}: zones only on the monitor under the cursor");
        assert_eq!(h.rect(b), rb, "{mon}: the target does not dodge");
        drop_now(&mut h, a);
        let after = order(&h);
        let (ia, ib) = (before.iter().position(|&x| x == a).unwrap(), before.iter().position(|&x| x == b).unwrap());
        let mut want = before.clone();
        want.swap(ia, ib);
        assert_eq!(after, want, "{mon}: a and b exchanged places");
        assert!(zones(&h).is_empty(), "{mon}: zones gone after the drop");
        // Dead centre of b: stack.
        let rb = h.rect(b);
        mouse_drag(&mut h, a, (rb.x + rb.w / 2, rb.y + rb.h / 2));
        assert_eq!(h.engine.drag.as_ref().map(|d| d.mode), Some(planor::Drag::Stack), "{mon}: centre zone groups");
        drop_now(&mut h, a);
        let s = &h.engine.strips[&key];
        let (i, _) = s.locate(a).unwrap();
        assert!(s.columns[i].members.contains(&b), "{mon}: a joined b's group");
        h.stop_clean();
    }
}

#[test]
fn n018_dropping_a_group_member_on_its_sibling_reorders_the_group_on_both_orientations() {
    for (mon, ids) in [("MON0", [101, 102, 103]), ("MON2", [301, 302, 303])] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        let key = (1, mon.to_string());
        focus(&mut h, ids[1]);
        h.cmd(Command::Key { name: "stack_toggle".into() });
        h.settle();
        let s = &h.engine.strips[&key];
        let col = s.columns[s.focus].members.clone();
        assert_eq!(col.len(), 2, "{mon}: a pair formed");
        let active = s.active().unwrap();
        let other = *col.iter().find(|&&m| m != active).unwrap();
        // The sibling's peek: the part of its rectangle inside the strip area.
        let area = s.area;
        let r = s.rects(96).into_iter().find(|x| x.0 == other).unwrap().1;
        let (x0, y0) = (r.x.max(area.x), r.y.max(area.y));
        let (x1, y1) = ((r.x + r.w).min(area.x + area.w), (r.y + r.h).min(area.y + area.h));
        assert!(x1 > x0 && y1 > y0, "{mon}: sibling peek visible: {r:?}");
        mouse_drag(&mut h, active, ((x0 + x1) / 2, (y0 + y1) / 2));
        assert_eq!(h.engine.drag.as_ref().map(|d| d.mode), Some(planor::Drag::Swap), "{mon}: onto the sibling = reorder");
        drop_now(&mut h, active);
        let s = &h.engine.strips[&key];
        let (i, _) = s.locate(active).unwrap();
        let now = s.columns[i].members.clone();
        assert_eq!(now.len(), 2, "{mon}: still one pair (it used to pull the window out)");
        assert_eq!(now, col.iter().rev().copied().collect::<Vec<_>>(), "{mon}: order swapped");
        h.stop_clean();
    }
}

#[test]
fn n012_drop_across_monitors_goes_beside_and_the_zones_follow_the_cursor() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    to_start(&mut h, "MON2");
    to_start(&mut h, "MON0");
    assert!(h.engine.strips[&(1, "MON0".to_string())].in_view(101));
    let port = (1, "MON2".to_string());
    let target = h.engine.strips[&port].columns[0].members[0];
    let rt = h.rect(target);
    mouse_drag(&mut h, 101, (rt.x + rt.w / 8, rt.y + rt.h / 8));
    let z = zones(&h);
    assert!(!z.is_empty() && z.iter().all(|z| z.monitor == "MON2"), "highlights moved to the portrait monitor: {z:?}");
    assert_eq!(h.engine.drag.as_ref().map(|d| d.mode), Some(planor::Drag::Before), "from another monitor: beside, no swap");
    drop_now(&mut h, 101);
    assert_eq!(h.engine.strips[&port].locate(101), Some((0, 0)), "101 went in before the target on the portrait monitor");
    assert!(h.engine.strips[&port].locate(target).is_some(), "the target stays on the portrait monitor");
    assert!(h.engine.strips[&(1, "MON0".to_string())].locate(101).is_none());
    h.stop_clean();
}

#[test]
fn n020_a_new_window_that_moves_itself_is_put_back() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let id = 150;
    h.desktop.0.lock().unwrap().add_window(SimWindow::new(id, Rect { x: 300, y: 300, w: 500, h: 400 }));
    h.engine.event(Event::Window(EVENT_OBJECT_SHOW, id));
    h.settle();
    let slot = h.rect(id);
    // The app restores its own saved bounds a moment after showing.
    h.desktop.0.lock().unwrap().windows.get_mut(&id).unwrap().rect = Rect { x: 40, y: 60, w: 700, h: 500 };
    h.engine.event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, id));
    h.settle();
    assert_eq!(h.rect(id), slot, "the newborn window is back in its slot");
    h.stop_clean();
}

// ---- Recorded touchpad gestures (`planor gestures record`), replayed on landscape AND portrait ----

/// One gesture: the frames from the first touch to the lift of the last finger.
/// Fingers used (most seen) and centroid travel of the frames with that many fingers.
fn describe_gesture(frames: &[Vec<(f64, f64)>]) -> (usize, f64, f64) {
    let fingers = frames.iter().map(|f| f.len()).max().unwrap_or(0);
    let c = |f: &Vec<(f64, f64)>| {
        let n = f.len() as f64;
        (f.iter().map(|p| p.0).sum::<f64>() / n, f.iter().map(|p| p.1).sum::<f64>() / n)
    };
    let full: Vec<_> = frames.iter().filter(|f| f.len() == fingers).collect();
    match (full.first(), full.last()) {
        (Some(a), Some(b)) => {
            let (a, b) = (c(a), c(b));
            (fingers, b.0 - a.0, b.1 - a.1)
        }
        _ => (fingers, 0., 0.),
    }
}

/// Replays one gesture on `mon` ("MON0" landscape or "MON2" portrait) and checks the engine did what the
/// gesture means there. Returns a one-line description for the report.
fn replay_on(frames: &[Vec<(f64, f64)>], mon: &str) -> String {
    let (fingers, dx, dy) = describe_gesture(frames);
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let key = (1, mon.to_string());
    let horizontal_strip = h.engine.strips[&key].area.horizontal();
    let deadzone = h.engine.cfg["touchpad"]["axis_lock_deadzone"].as_integer().unwrap_or(12) as f64;
    let travel = dx.hypot(dy);
    let along = (dx.abs() >= dy.abs()) == horizontal_strip;
    let first = h.engine.strips[&key].columns[0].members[0];
    focus(&mut h, first);
    if fingers == 3 && !along {
        // Across the strip: give it a group of three to roll (a pair stops at its ends, N-010, so a swipe
        // towards the end it already shows would rightly do nothing).
        h.cmd(Command::Key { name: "stack_toggle".into() });
        h.settle();
        let rest = h.engine.strips[&key].columns.get(1).map(|c| c.members[0]);
        if let Some(third) = rest {
            focus(&mut h, third);
            h.cmd(Command::Key { name: "stack_toggle".into() });
            h.settle();
        }
        assert!(h.engine.strips[&key].columns.iter().any(|c| c.members.len() >= 3), "setup: a group of three on {mon}");
    }
    if fingers == 3 && along {
        // Along the strip: start in the middle so either direction can scroll.
        let s = h.engine.strips.get_mut(&key).unwrap();
        s.offset = i32::MAX / 4;
        s.scroll(0);
        s.offset /= 2;
        h.engine.dirty = true;
        h.settle();
    }
    let start = h.engine.strips[&key].clone();
    let (mut moved, mut rolled, mut dragged) = (false, false, false);
    for f in frames {
        h.engine.event(Event::Contacts(f.clone()));
        h.settle_quiet();
        let s = &h.engine.strips[&key];
        moved |= s.offset != start.offset;
        rolled |= s.columns.iter().any(|c| c.frac != 0.)
            || s.columns.iter().zip(&start.columns).any(|(a, b)| a.active != b.active);
        dragged |= h.engine.pad.is_some();
    }
    h.engine.event(Event::Contacts(vec![]));
    h.settle_quiet();
    settle_now(&mut h);
    h.settle(); // invariants and os_rect == expected_rect after the gesture
    let what = format!("{fingers} fingers, travel ({dx:.0}, {dy:.0}) on {mon}");
    assert!(h.engine.pad.is_none() && h.engine.drag.is_none() && !h.engine.pad_scrolling, "{what}: gesture left state behind");
    if fingers < 3 {
        // Cursor moves, taps, clicks, two-finger scroll and pinch: Planor must stay out of it.
        assert!(!moved && !dragged && !rolled, "{what}: Planor reacted to a {fingers}-finger gesture");
        return format!("{what}: ignored, as it should be");
    }
    if fingers >= 5 {
        // The whole hand lands and lifts in stages, so the first fingers may briefly start something; the
        // fifth cancels it. What counts is that the layout ends exactly as it was.
        let end = &h.engine.strips[&key];
        let order = |s: &planor::Strip| s.columns.iter().map(|c| (c.members.clone(), c.active)).collect::<Vec<_>>();
        assert_eq!(order(end), order(&start), "{what}: a five-finger gesture changed the layout");
        return format!("{what}: vetoed, layout unchanged");
    }
    if travel < deadzone * 2. {
        return format!("{what}: too short to mean anything, only checked it leaves no state");
    }
    match fingers {
        3 if along => {
            assert!(moved, "{what}: fingers along the strip must scroll it");
            format!("{what}: scrolled the strip")
        }
        3 => {
            assert!(rolled, "{what}: fingers across the strip must roll the group");
            assert_eq!(h.engine.strips[&key].offset, start.offset, "{what}: rolling must not scroll the strip");
            format!("{what}: rolled the group")
        }
        4 => {
            assert!(dragged, "{what}: four fingers must pick the window up");
            assert!(!moved || !h.engine.pad_scrolling, "{what}: no stray scroll after the drop");
            format!("{what}: carried and dropped the window")
        }
        _ => format!("{what}: no gesture for this many fingers"),
    }
}

#[test]
fn recorded_gestures_replay_on_landscape_and_portrait() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gestures/recorded");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect())
        .unwrap_or_default();
    files.sort();
    assert!(!files.is_empty(), "no recordings in {}", dir.display());
    let mut report = vec![];
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        // Guided recordings: the owner did not follow every instruction. Keep the gestures that match their
        // step (or the step before, when made late in the get-ready gap); set the rest aside.
        type GestureList = Vec<(String, Vec<Vec<(f64, f64)>>)>;
        let all = planor::replay::segments(&planor::replay::parse_events_jsonl(&text));
        let total = all.len();
        let gestures: GestureList = all
            .into_iter()
            .filter(|s| {
                let g = planor::replay::identify(s);
                s.label.is_empty()
                    || planor::replay::fits(&s.label, &g)
                    || (!s.previous.is_empty() && planor::replay::fits(&s.previous, &g))
            })
            .map(|s| (s.label, s.frames.into_iter().map(|f| f.1).collect()))
            .collect();
        report.push(format!(
            "{}: {} of {total} gestures used, {} set aside as not done as instructed",
            f.file_name().unwrap().to_string_lossy(),
            gestures.len(),
            total - gestures.len()
        ));
        assert!(!gestures.is_empty(), "{}: no gestures", f.display());
        for (n, (label, g)) in gestures.iter().enumerate() {
            for mon in ["MON0", "MON2"] {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| replay_on(g, mon)));
                match r {
                    Ok(line) => report.push(format!("{} #{n} [{label}]: {line}", f.file_name().unwrap().to_string_lossy())),
                    Err(e) => {
                        let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                        panic!("{} gesture #{n} [{label}] on {mon}: {msg}\nso far:\n{}", f.display(), report.join("\n"));
                    }
                }
            }
        }
    }
    println!("{}", report.join("\n"));
}

#[test]
fn win_wheel_is_for_the_mouse_and_capped_per_event() {
    // Owner's choice: Win + scroll is for the mouse, not the touchpad. The touchpad's fine steps (-3, -7, ...
    // and -5355 in its momentum, owner top-up recording) are ignored; mouse notches (multiples of 120) scroll,
    // at most two notches per event.
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let land = (1, "MON0".to_string());
    to_start(&mut h, "MON0");
    for delta in [-3, -7, -12, -5355] {
        h.engine.event(Event::Wheel { delta, tilt: false, shift: false, x: 900, y: 500 });
        h.settle();
    }
    assert_eq!(h.engine.strips[&land].offset, 0, "touchpad scroll steps are not Win+wheel");
    let step = h.engine.cfg["mouse"]["win_wheel_step_px"].as_integer().unwrap() as i32;
    h.engine.event(Event::Wheel { delta: -120, tilt: false, shift: false, x: 900, y: 500 });
    h.settle();
    let one = h.engine.strips[&land].offset;
    assert!(one > 0 && one <= step, "one mouse notch scrolls one step: {one}");
    h.engine.event(Event::Wheel { delta: -1200, tilt: false, shift: false, x: 900, y: 500 });
    h.settle();
    let more = h.engine.strips[&land].offset - one;
    assert!(more > 0 && more <= 2 * step, "a fast spin (10 notches in one event) scrolls at most two: {more}");
    h.stop_clean();
}

#[test]
fn non_dirtying_contacts_do_not_leave_stale_input_timestamp_on_both_orientations() {
    for (mon, id) in [("MON0", 101), ("MON2", 301)] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        focus(&mut h, id);
        assert!(h.engine.input_qpc_us.is_none());

        // Sub-deadzone 3-finger movement produces Swipe::None without dirtying layout.
        h.engine.event(Event::Contacts(vec![(200., 200.); 3]));
        h.engine.event(Event::Contacts(vec![(200.5, 200.1); 3]));
        assert!(!h.engine.dirty, "{mon}: sub-deadzone contact does not dirty layout");
        assert!(h.engine.input_qpc_us.is_none(), "{mon}: non-dirtying 3-finger contact must not leave stale input_qpc_us");
        h.engine.event(Event::Contacts(vec![]));
        h.settle_quiet();

        // Start a 4-finger pad drag, wait past the 8 ms move gate, then nudge without changing target/mode.
        h.engine.event(Event::Contacts(vec![(200., 200.); 4]));
        h.engine.event(Event::Contacts(vec![(216., 200.); 4]));
        h.settle_quiet();
        assert!(h.engine.pad.is_some(), "{mon}: 4-finger drag started");
        thread::sleep(Duration::from_millis(12));
        h.engine.event(Event::Contacts(vec![(216.2, 200.); 4]));
        if !h.engine.dirty {
            assert!(h.engine.input_qpc_us.is_none(), "{mon}: pad_move without scene must not leave stale input_qpc_us");
        }
        h.engine.event(Event::Contacts(vec![]));
        h.settle_quiet();
        settle_now(&mut h);
        h.stop_clean();
    }
}

#[test]
fn live_scroll_caches_holes_during_continuous_motion_on_both_orientations() {
    for (mon, id, hole_id, hole_rect, p0, p1, p2) in [
        (
            "MON0",
            101,
            901,
            Rect { x: 200, y: 200, w: 300, h: 300 },
            (400., 200.),
            (360., 200.),
            (320., 200.),
        ),
        (
            "MON2",
            301,
            902,
            Rect { x: -1200, y: 200, w: 300, h: 300 },
            (200., 400.),
            (200., 360.),
            (200., 320.),
        ),
    ] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
            three_per_monitor(sim);
            let mut ext = SimWindow::new(hole_id, hole_rect);
            ext.elevated = true;
            sim.add_window(ext);
        });
        focus(&mut h, id);
        let initial_holes = h.desktop.0.lock().unwrap().scenes.last().unwrap().holes.clone();
        assert!(initial_holes.contains(&hole_rect), "{mon}: initial scene includes hole {hole_rect:?}");

        // Enter live scroll along the monitor's strip axis.
        h.engine.event(Event::Contacts(vec![p0; 3]));
        h.settle_quiet();
        h.engine.event(Event::Contacts(vec![p1; 3]));
        h.settle_quiet();
        assert!(h.engine.pad_scrolling, "{mon}: should be live scrolling");

        // Move the unmanaged window right after holes were checked: live frames reuse cached holes.
        let mut moved = hole_rect;
        moved.w = 360;
        h.desktop.0.lock().unwrap().windows.get_mut(&hole_id).unwrap().rect = moved;
        h.engine.holes_checked = Instant::now();
        h.engine.event(Event::Contacts(vec![p2; 3]));
        h.settle_quiet();
        let cached = h.desktop.0.lock().unwrap().scenes.last().unwrap().holes.clone();
        assert!(cached.contains(&hole_rect), "{mon}: live scroll should reuse cached holes within 150 ms: {cached:?}");

        // Settling (live = false) refreshes holes immediately to the moved rect.
        h.engine.event(Event::Contacts(vec![]));
        h.settle_quiet();
        settle_now(&mut h);
        let refreshed = h.desktop.0.lock().unwrap().scenes.last().unwrap().holes.clone();
        assert!(refreshed.contains(&moved), "{mon}: settled scene must refresh holes to {moved:?}: {refreshed:?}");
        h.stop_clean();
    }
}

#[test]
fn newborn_window_gets_cover_while_scan_and_neighbours_do_not_on_both_orientations() {
    for (mon, cursor, new_rect) in [
        ("MON0", (400, 400), Rect { x: 200, y: 200, w: 600, h: 400 }),
        ("MON2", (-700, 800), Rect { x: -1200, y: 500, w: 700, h: 500 }),
    ] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        // Existing windows tiled via `scan()` never receive a newborn cover.
        assert!(h.desktop.0.lock().unwrap().scenes.iter().all(|s| s.covered.is_empty()), "{mon}: scan must not mark covers");

        h.desktop.0.lock().unwrap().cursor = cursor;
        let id = h.desktop.0.lock().unwrap().add_window(SimWindow::new(0, new_rect));
        h.engine.event(Event::Window(EVENT_OBJECT_SHOW, id));
        h.settle();

        let last_scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
        assert_eq!(last_scene.covered, vec![id], "{mon}: only newborn {id} covered, not neighbours");

        // Hiding/destroying removes any cover state cleanly.
        h.desktop.0.lock().unwrap().windows.get_mut(&id).unwrap().visible = false;
        h.engine.event(Event::Window(EVENT_OBJECT_HIDE, id));
        h.settle();
        assert!(!h.engine.covering.contains_key(&id), "{mon}: hide clears cover state");
        h.stop_clean();
    }
}

#[test]
fn cross_monitor_transfer_gets_cover_landscape_and_portrait() {
    // 1) Landscape MON0 -> Portrait MON2 via drag drop.
    {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        to_start(&mut h, "MON2");
        to_start(&mut h, "MON0");
        assert!(h.engine.strips[&(1, "MON2".into())].in_view(301));
        let r = h.rect(301);
        mouse_drag(&mut h, 101, (r.x + r.w / 8, r.y + r.h / 8));
        drop_now(&mut h, 101);
        assert!(h.engine.strips[&(1, "MON2".into())].locate(101).is_some(), "101 transferred to MON2");
        let covered = h.desktop.0.lock().unwrap().scenes.last().unwrap().covered.clone();
        assert!(covered.contains(&101), "landscape->portrait transfer must cover 101: {covered:?}");
        assert!(!covered.contains(&301), "existing destination neighbour 301 must not be covered");
        h.stop_clean();
    }
    // 2) Portrait MON2 -> Landscape MON0 via drag drop.
    {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        to_start(&mut h, "MON0");
        to_start(&mut h, "MON2");
        assert!(h.engine.strips[&(1, "MON0".into())].in_view(101));
        let r = h.rect(101);
        mouse_drag(&mut h, 301, (r.x + r.w / 8, r.y + r.h / 8));
        drop_now(&mut h, 301);
        assert!(h.engine.strips[&(1, "MON0".into())].locate(301).is_some(), "301 transferred to MON0");
        let covered = h.desktop.0.lock().unwrap().scenes.last().unwrap().covered.clone();
        assert!(covered.contains(&301), "portrait->landscape transfer must cover 301: {covered:?}");
        assert!(!covered.contains(&101), "existing destination neighbour 101 must not be covered");
        h.stop_clean();
    }
}

#[test]
fn placeholder_config_false_disables_cover_on_both_orientations() {
    for (mon, cursor, new_rect) in [
        ("MON0", (400, 400), Rect { x: 200, y: 200, w: 600, h: 400 }),
        ("MON2", (-700, 800), Rect { x: -1200, y: 500, w: 700, h: 500 }),
    ] {
        let mut cfg = note_cfg();
        cfg["animation"]["placeholder"] = toml::Value::Boolean(false);
        let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
        h.desktop.0.lock().unwrap().cursor = cursor;
        let id = h.desktop.0.lock().unwrap().add_window(SimWindow::new(0, new_rect));
        h.engine.event(Event::Window(EVENT_OBJECT_SHOW, id));
        h.settle();
        let covered = h.desktop.0.lock().unwrap().scenes.last().unwrap().covered.clone();
        assert!(covered.is_empty(), "{mon}: placeholder=false disables cover: {covered:?}");
        h.stop_clean();
    }
}

#[test]
fn high_refresh_and_vrr_momentum_steps_smoothly_on_both_orientations() {
    for hz in [48.0, 60.0, 120.0, 240.0] {
        for (mon, axis_delta) in [("MON0", (-60.0, 0.0)), ("MON2", (0.0, -60.0))] {
            let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
            h.desktop.0.lock().unwrap().max_refresh_hz = hz;
            h.engine.topology();
            h.settle();
            assert_eq!(h.engine.max_refresh_hz, hz);

            let key = (1, mon.to_string());
            to_start(&mut h, mon);
            let before = h.engine.strips[&key].offset;
            // Drive an active momentum inertia state and advance fast_tick intervals.
            let vertical = mon == "MON2";
            h.engine.inertia = Some((vertical, -1200.0, Instant::now() - Duration::from_millis(8)));
            assert!(h.engine.wants_fast_tick(), "{mon}@{hz}Hz wants fast tick during inertia");
            for _ in 0..4 {
                h.engine.fast_tick();
                h.settle_quiet();
                if let Some((v, s, _)) = h.engine.inertia {
                    h.engine.inertia = Some((v, s, Instant::now() - Duration::from_millis(planor::pacing::fast_tick_ms(hz))));
                }
            }
            let after = h.engine.strips[&key].offset;
            assert_ne!(after, before, "{mon}@{hz}Hz: momentum fast_tick advanced strip offset (dx/dy {axis_delta:?})");
            h.engine.inertia = None;
            h.engine.settle = Some(Instant::now() - Duration::from_millis(1));
            h.tick_now();
            h.stop_clean();
        }
    }
}

fn mouse_resize(h: &mut SimHarness, id: isize, new_rect: Rect, cursor: (i32, i32)) {
    let r = h.rect(id);
    h.desktop.0.lock().unwrap().cursor = (r.x + r.w / 2, r.y + r.h / 2);
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, id));
    h.settle_quiet();
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&id).unwrap().rect = new_rect;
        g.cursor = cursor;
    }
    h.engine.tick().unwrap();
    h.settle_quiet();
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, id));
    h.settle();
}

#[test]
fn sim_disallowed_resizes_snap_back_on_both_orientations() {
    for (mon, ids) in [("MON0", [101, 102, 103]), ("MON2", [301, 302, 303])] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        to_start(&mut h, mon);
        let (a, b) = (ids[0], ids[1]);
        let ra = h.rect(a);
        let rb = h.rect(b);

        // 1. Cross-axis edge drags (including dragging into taskbar area) snap back completely.
        if mon == "MON0" {
            mouse_resize(&mut h, a, Rect { y: 40, h: ra.h - 40, ..ra }, (ra.x + ra.w / 2, 40));
            assert_eq!(h.rect(a), ra, "{mon}: top-edge cross-axis resize snaps back");
            mouse_resize(&mut h, a, Rect { h: 1190, ..ra }, (ra.x + ra.w / 2, 1185));
            assert_eq!(h.rect(a), ra, "{mon}: bottom-edge into taskbar snaps back");
        } else {
            mouse_resize(&mut h, a, Rect { x: ra.x + 80, w: ra.w - 80, ..ra }, (ra.x + 80, ra.y + ra.h / 2));
            assert_eq!(h.rect(a), ra, "{mon}: left-edge cross-axis resize snaps back");
            mouse_resize(&mut h, a, Rect { w: ra.w - 150, ..ra }, (ra.x + ra.w - 150, ra.y + ra.h / 2));
            assert_eq!(h.rect(a), ra, "{mon}: right-edge cross-axis resize snaps back");
        }
        assert_eq!(h.rect(b), rb, "{mon}: neighbour untouched after cross-axis snap-back");

        // 2. Column 0 leading-edge drag (inward & outward) is ignored and snapped back.
        let (grow_lead, shrink_lead, lead_cursor) = if mon == "MON0" {
            (
                Rect { x: ra.x - 120, w: ra.w + 120, ..ra },
                Rect { x: ra.x + 120, w: ra.w - 120, ..ra },
                (ra.x + 120, ra.y + ra.h / 2),
            )
        } else {
            (
                Rect { y: ra.y - 120, h: ra.h + 120, ..ra },
                Rect { y: ra.y + 120, h: ra.h - 120, ..ra },
                (ra.x + ra.w / 2, ra.y + 120),
            )
        };
        mouse_resize(&mut h, a, shrink_lead, lead_cursor);
        assert_eq!(h.rect(a), ra, "{mon}: col 0 inward leading resize ignored");
        mouse_resize(&mut h, a, grow_lead, (ra.x + 100, ra.y + 100));
        assert_eq!(h.rect(a), ra, "{mon}: col 0 outward leading resize ignored");

        // 3. Escape-cancelled resize (rect unchanged) and paused resize leave layout untouched.
        mouse_resize(&mut h, a, ra, (ra.x + ra.w / 2, ra.y + ra.h / 2));
        assert_eq!(h.rect(a), ra, "{mon}: unchanged rect is a no-op");

        h.cmd(Command::Pause);
        let paused_try = if mon == "MON0" { Rect { w: ra.w + 100, ..ra } } else { Rect { h: ra.h + 100, ..ra } };
        mouse_resize(&mut h, a, paused_try, (ra.x + 200, ra.y + 200));
        h.cmd(Command::Resume);
        h.settle();
        assert_eq!(h.rect(a), ra, "{mon}: resize while paused is ignored");
        h.stop_clean();
    }

    // 4. Lone window shrink snaps back to full work area, and sub-min / pair squeeze clamp to min.
    for (mon, origin, single_w, single_h, pair_b_rect) in [
        ("MON0", (100, 100), 600, 400, Rect { x: 800, y: 100, w: 600, h: 400 }),
        ("MON2", (-1300, 200), 900, 500, Rect { x: -1300, y: 1200, w: 900, h: 500 }),
    ] {
        // Lone window shrink.
        let mut solo = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
            let mut w = SimWindow::new(501, Rect { x: origin.0, y: origin.1, w: single_w, h: single_h });
            w.min_w = 400;
            w.min_h = 400;
            sim.add_window(w);
            sim.foreground = 501;
        });
        let full = solo.rect(501);
        let shrunk = if mon == "MON0" {
            Rect { w: full.w - 400, ..full }
        } else {
            Rect { h: full.h - 400, ..full }
        };
        mouse_resize(&mut solo, 501, shrunk, (full.x + 300, full.y + 300));
        assert_eq!(solo.rect(501), full, "{mon}: lone window shrink snaps back to full monitor");
        solo.stop_clean();

        // Pair sub-minimum shrink + squeezing neighbour below its minimum.
        let mut pair_h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
            for (wid, r) in [(501, Rect { x: origin.0, y: origin.1, w: single_w, h: single_h }), (502, pair_b_rect)] {
                let mut w = SimWindow::new(wid, r);
                w.min_w = 400;
                w.min_h = 400;
                sim.add_window(w);
            }
            sim.foreground = 501;
        });
        let r1 = pair_h.rect(501);
        let tiny = if mon == "MON0" { Rect { w: 50, ..r1 } } else { Rect { h: 50, ..r1 } };
        mouse_resize(&mut pair_h, 501, tiny, (r1.x + 200, r1.y + 200));
        let len1 = if mon == "MON0" { pair_h.rect(501).w } else { pair_h.rect(501).h };
        assert!(len1 >= 400, "{mon}: sub-minimum shrink clamped to >= 400, got {len1}");

        // Squeeze neighbour 501 down to ~90 px by dragging 502's leading edge far toward origin.
        let r2 = pair_h.rect(502);
        let squeeze = if mon == "MON0" {
            let new_x = 100;
            Rect { x: new_x, w: (r2.x + r2.w) - new_x, ..r2 }
        } else {
            let new_y = 100;
            Rect { y: new_y, h: (r2.y + r2.h) - new_y, ..r2 }
        };
        mouse_resize(&mut pair_h, 502, squeeze, (r1.x + 200, r1.y + 200));
        let len1_after = if mon == "MON0" { pair_h.rect(501).w } else { pair_h.rect(501).h };
        assert!(len1_after >= 400, "{mon}: squeezed neighbour stayed >= 400 min, got {len1_after}");
        pair_h.stop_clean();
    }
}

#[test]
fn sim_varied_allowed_and_corner_resizes_on_both_orientations() {
    for (mon, ids) in [("MON0", [101, 102, 103]), ("MON2", [301, 302, 303])] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        let key = (1, mon.to_string());
        to_start(&mut h, mon);
        let (a, b, c) = (ids[0], ids[1], ids[2]);

        // 1. Allowed trailing resize and middle-column leading resize.
        let ra = h.rect(a);
        let grow = if mon == "MON0" { Rect { w: ra.w + 120, ..ra } } else { Rect { h: ra.h + 120, ..ra } };
        mouse_resize(&mut h, a, grow, (ra.x + 300, ra.y + 300));
        assert_eq!(h.rect(a), grow, "{mon}: trailing-edge grow honoured");

        focus(&mut h, b);
        let rb = h.rect(b);
        let lead_shrink = if mon == "MON0" {
            Rect { x: rb.x + 80, w: rb.w - 80, ..rb }
        } else {
            Rect { y: rb.y + 80, h: rb.h - 80, ..rb }
        };
        mouse_resize(&mut h, b, lead_shrink, (rb.x + 200, rb.y + 200));
        let rb_now = h.rect(b);
        if mon == "MON0" {
            assert_eq!(rb_now.w, rb.w - 80);
            assert_eq!(rb_now.x + rb_now.w - rb.x - rb.w, 0, "{mon}: leading resize keeps trailing X edge");
        } else {
            assert_eq!(rb_now.h, rb.h - 80);
            assert_eq!(rb_now.y + rb_now.h - rb.y - rb.h, 0, "{mon}: leading resize keeps trailing Y edge");
        }

        // 2. Corner drags: col 1 top-left keeps strip change & snaps cross-axis; col 0 top-left snaps all back.
        let rb2 = h.rect(b);
        let corner_b = if mon == "MON0" {
            Rect { x: rb2.x - 60, y: 40, w: rb2.w + 60, h: rb2.h - 40 }
        } else {
            Rect { x: rb2.x + 90, y: rb2.y - 60, w: rb2.w - 90, h: rb2.h + 60 }
        };
        mouse_resize(&mut h, b, corner_b, (rb2.x + 200, rb2.y + 200));
        let rb2_after = h.rect(b);
        if mon == "MON0" {
            assert_eq!((rb2_after.w, rb2_after.y, rb2_after.h), (rb2.w + 60, rb2.y, rb2.h));
        } else {
            assert_eq!((rb2_after.h, rb2_after.x, rb2_after.w), (rb2.h + 60, rb2.x, rb2.w));
        }

        to_start(&mut h, mon);
        let ra0 = h.rect(a);
        let corner_a0 = Rect { x: ra0.x + 50, y: ra0.y + 50, w: ra0.w - 50, h: ra0.h - 50 };
        mouse_resize(&mut h, a, corner_a0, (ra0.x + 200, ra0.y + 200));
        assert_eq!(h.rect(a), ra0, "{mon}: col 0 top-left corner disallowed on both axes -> full snap back");

        // 3. Edge drag with cursor crossing onto neighbouring monitor while window stays mostly on home monitor.
        let (cross_rect, cross_cursor) = if mon == "MON0" {
            (Rect { w: 1200, ..ra0 }, (2100, 500))
        } else {
            (Rect { h: 1400, ..ra0 }, (200, 500))
        };
        mouse_resize(&mut h, a, cross_rect, cross_cursor);
        assert!(h.engine.strips[&key].locate(a).is_some(), "{mon}: must stay on home monitor as a resize, not transfer");
        if mon == "MON0" {
            assert_eq!(h.rect(a).w, 1200);
        } else {
            assert_eq!(h.rect(a).h, 1400);
        }

        // 4. Stacked/grouped column resize: cross-axis snaps back; strip-axis resizes all group members together.
        focus(&mut h, a);
        h.cmd(Command::Key { name: "stack_toggle".into() });
        h.settle();
        let active_id = h.engine.strips[&key].active().unwrap();
        let col_idx = h.engine.strips[&key].locate(active_id).unwrap().0;
        let sibling_id = h.engine.strips[&key].columns[col_idx].members.iter().copied().find(|&m| m != active_id).unwrap();
        let r_stack = h.rect(active_id);
        let cross_stack = if mon == "MON0" { Rect { h: r_stack.h - 90, ..r_stack } } else { Rect { w: r_stack.w - 90, ..r_stack } };
        mouse_resize(&mut h, active_id, cross_stack, (r_stack.x + 200, r_stack.y + 200));
        assert_eq!(h.rect(active_id), r_stack, "{mon}: stacked cross-axis resize snaps back");

        let strip_stack = if mon == "MON0" { Rect { w: r_stack.w - 100, ..r_stack } } else { Rect { h: r_stack.h - 100, ..r_stack } };
        mouse_resize(&mut h, active_id, strip_stack, (r_stack.x + 200, r_stack.y + 200));
        assert_eq!(
            h.engine.strips[&key].columns[col_idx].length,
            if mon == "MON0" { strip_stack.w } else { strip_stack.h },
            "{mon}: stacked column resizes along strip axis for all members {sibling_id}"
        );

        // 5. Pinned window free resize persists without snapping back or disturbing the strip.
        focus(&mut h, c);
        h.cmd(Command::Key { name: "toggle_pin".into() });
        h.settle();
        assert!(h.engine.pinned.contains(&c));
        let pinned_rect = if mon == "MON0" {
            Rect { x: 220, y: 220, w: 520, h: 340 }
        } else {
            Rect { x: -1200, y: 300, w: 520, h: 340 }
        };
        mouse_resize(&mut h, c, pinned_rect, (pinned_rect.x + 100, pinned_rect.y + 100));
        assert_eq!(h.rect(c), pinned_rect, "{mon}: pinned window keeps free resize on both axes");
        h.stop_clean();
    }
}

#[test]
fn sim_app_rejected_resize_enforces_minimum_on_landscape_and_portrait() {
    // 1. Landscape MON0: app rejects width < 1100 px after user shrinks column.
    {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
            let mut w1 = SimWindow::new(101, Rect { x: 100, y: 100, w: 600, h: 400 });
            w1.min_enforced_w = 1100;
            sim.add_window(w1);
            sim.add_window(SimWindow::new(102, Rect { x: 800, y: 100, w: 600, h: 400 }));
            sim.foreground = 101;
        });
        // Settle initial scan already triggered rejection >= 1100; grow first, then user drags inward to 500.
        let r = h.rect(101);
        assert!(r.w >= 1100);
        mouse_resize(&mut h, 101, Rect { w: 500, ..r }, (500, 400));
        assert!(h.rect(101).w >= 1100, "landscape app rejection restores >= 1100 px width: {:?}", h.rect(101));
        h.stop_clean();
    }
    // 2. Portrait MON2: app rejects height < 1300 px after user shrinks column.
    {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
            let mut w1 = SimWindow::new(301, Rect { x: -1300, y: 100, w: 900, h: 500 });
            w1.min_enforced_h = 1300;
            sim.add_window(w1);
            sim.add_window(SimWindow::new(302, Rect { x: -1300, y: 900, w: 900, h: 500 }));
            sim.foreground = 301;
        });
        let r = h.rect(301);
        assert!(r.h >= 1300);
        mouse_resize(&mut h, 301, Rect { h: 500, ..r }, (-700, 500));
        assert!(h.rect(301).h >= 1300, "portrait app rejection restores >= 1300 px height: {:?}", h.rect(301));
        h.stop_clean();
    }
}

#[test]
fn hot_cfg_reloads_and_arc_scenes_stay_synced_on_landscape_and_portrait() {
    for (mon, first_id, second_id, hole_id, initial_hole, moved_hole) in [
        (
            "MON0",
            101,
            102,
            901,
            Rect { x: 100, y: 100, w: 300, h: 300 },
            Rect { x: 180, y: 140, w: 340, h: 280 },
        ),
        (
            "MON2",
            301,
            302,
            902,
            Rect { x: -1200, y: 200, w: 300, h: 300 },
            Rect { x: -1100, y: 320, w: 320, h: 360 },
        ),
    ] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
            three_per_monitor(sim);
            let mut ext = SimWindow::new(hole_id, initial_hole);
            ext.elevated = true;
            sim.add_window(ext);
        });
        focus(&mut h, first_id);

        // Initial HotCfg and Arc monitors/holes match configuration & topology.
        assert_eq!(h.engine.hot.focused_color, Some(0x3B82F6), "{mon}: initial focused_color");
        assert_eq!(h.engine.hot.gap_px, 6, "{mon}: initial gap_px");
        assert_eq!(h.engine.hot.stack_peek_px, Some(96), "{mon}: initial stack_peek_px");
        assert!(h.engine.hot.anim_placeholder, "{mon}: initial placeholder");
        assert!((h.engine.hot.swipe_scroll_gain - 4.0).abs() < 1e-9, "{mon}: initial swipe_scroll_gain");
        assert!((h.engine.hot.win_wheel_step_px - 240.0).abs() < 1e-9, "{mon}: initial win_wheel_step_px");
        assert_eq!(h.engine.displays_arc.as_ref(), h.engine.displays.as_slice(), "{mon}: displays Arc synced");
        assert!(h.engine.last_holes_arc.contains(&initial_hole), "{mon}: initial holes Arc synced");

        // Update TOML config + move unmanaged hole window, then trigger Reload.
        {
            let mut d = h.desktop.0.lock().unwrap();
            d.config["borders"]["focused_color"] = toml::Value::String("#FF00AA".into());
            d.config["layout"]["gap_px"] = toml::Value::Integer(2);
            d.config["layout"]["stack_peek_px"] = toml::Value::Integer(42);
            d.config["animation"]["enabled"] = toml::Value::Boolean(false);
            d.config["animation"]["placeholder"] = toml::Value::Boolean(false);
            d.config["touchpad"]["swipe_scroll_gain"] = toml::Value::Float(2.5);
            d.config["mouse"]["win_wheel_step_px"] = toml::Value::Integer(360);
            d.windows.get_mut(&hole_id).unwrap().rect = moved_hole;
        }
        h.cmd(Command::Reload);
        h.settle();

        assert_eq!(h.engine.hot.focused_color, Some(0xFF00AA), "{mon}: reloaded focused_color");
        assert_eq!(h.engine.hot.gap_px, 2, "{mon}: reloaded gap_px");
        assert_eq!(h.engine.hot.stack_peek_px, Some(42), "{mon}: reloaded stack_peek_px");
        assert!(!h.engine.hot.anim_enabled, "{mon}: reloaded anim.enabled");
        assert!(!h.engine.hot.anim_placeholder, "{mon}: reloaded anim.placeholder");
        assert!((h.engine.hot.swipe_scroll_gain - 2.5).abs() < 1e-9, "{mon}: reloaded swipe_scroll_gain");
        assert!((h.engine.hot.win_wheel_step_px - 360.0).abs() < 1e-9, "{mon}: reloaded win_wheel_step_px");

        // Strip layout & Scene Arc holes reflect the reloaded gap and updated unmanaged window rect.
        let r1 = h.rect(first_id);
        let r2 = h.rect(second_id);
        let gap = if mon == "MON0" {
            r2.x - (r1.x + r1.w)
        } else {
            r2.y - (r1.y + r1.h)
        };
        assert_eq!(gap, 2, "{mon}: reloaded gap_px applied along strip axis");
        let last_holes = h.desktop.0.lock().unwrap().scenes.last().unwrap().holes.clone();
        assert!(last_holes.contains(&moved_hole), "{mon}: Scene holes Arc refreshed to moved rect");
        assert!(!last_holes.contains(&initial_hole), "{mon}: stale hole rect evicted");
        h.stop_clean();
    }
}
#[test]
fn config_reload_bumps_version_and_applies_gap() {
    let mut h = SimHarness::new(primary_only_rig());
    let before = h.engine.config_version;
    assert_eq!(h.engine.hot.gap_px, 6);
    h.desktop.0.lock().unwrap().config["layout"]["gap_px"] = toml::Value::Integer(11);
    h.cmd(Command::Reload);
    h.settle();
    assert_eq!(h.engine.config_version, before + 1, "one reload, one version bump");
    assert_eq!(h.engine.hot.gap_px, 11);
    h.stop_clean();
}

fn tile_color(h: &SimHarness, id: isize) -> Option<u32> {
    h.desktop
        .0
        .lock()
        .unwrap()
        .scenes
        .last()
        .and_then(|s| s.tile_colors.iter().find(|(tid, _)| *tid == id).and_then(|(_, c)| *c))
}

#[test]
fn issue_48_focus_highlight_removed_when_focus_on_unmanaged_app_dialog_or_system_ui() {
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(101, Rect { x: 10, y: 10, w: 800, h: 600 }));
        sim.add_window(SimWindow::new(102, Rect { x: 900, y: 10, w: 800, h: 600 }));
        // Elevated Task Manager (unmanaged external)
        let mut tm = SimWindow::new(901, Rect { x: 200, y: 200, w: 600, h: 500 });
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        sim.add_window(tm);
        // Owned floating dialog (external() == false, not managed)
        let mut dlg = SimWindow::new(902, Rect { x: 300, y: 300, w: 400, h: 300 });
        dlg.owner = 901;
        sim.add_window(dlg);
        // Planor.Note (Win+W note box: toolwindow + Planor class, external() == false)
        let mut note = SimWindow::new(903, Rect { x: 400, y: 200, w: 560, h: 112 });
        note.class = "Planor.Note".into();
        note.toolwindow = true;
        note.topmost = true;
        sim.add_window(note);
        // Start menu (Windows.UI.Core.CoreWindow, external() == false)
        let mut start = SimWindow::new(904, Rect { x: 600, y: 400, w: 600, h: 600 });
        start.class = "Windows.UI.Core.CoreWindow".into();
        start.topmost = true;
        sim.add_window(start);
        sim.foreground = 101;
    });

    assert_eq!(tile_color(&h, 101), Some(0x3B82F6), "managed foreground tile shows focus highlight");
    assert_eq!(tile_color(&h, 102), None);

    // Focus each unmanaged window: focus highlight must disappear in every case.
    for (fg, label) in [
        (901, "elevated Task Manager"),
        (902, "owned dialog"),
        (903, "Win+W note box (polled via tick)"),
        (904, "Start menu"),
    ] {
        h.desktop.0.lock().unwrap().foreground = fg;
        if fg == 903 {
            // Simulate WINEVENT_SKIPOWNPROCESS: no EVENT_SYSTEM_FOREGROUND, detected by tick().
            h.engine.tick().unwrap();
            h.settle_quiet();
        } else {
            h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, fg));
            h.settle_quiet();
        }
        assert_eq!(tile_color(&h, 101), None, "{label}: focus highlight must be removed");
        assert_eq!(tile_color(&h, 102), None, "{label}: no other tile should gain highlight");
    }

    // Focus back on managed tile 102 -> highlight returns on 102.
    h.desktop.0.lock().unwrap().foreground = 102;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 102));
    h.settle_quiet();
    assert_eq!(tile_color(&h, 102), Some(0x3B82F6), "highlight restores when managed tile regains focus");
    assert_eq!(tile_color(&h, 101), None);
    h.stop_clean();
}

#[test]
fn issue_52_task_manager_behind_managed_windows_does_not_erase_highlight() {
    let tm_rect = Rect { x: 200, y: 150, w: 1200, h: 700 };
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        // Put Task Manager BEHIND both managed windows in Z-order.
        let mut tm = SimWindow::new(901, tm_rect);
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        sim.add_window(tm);
        sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
        sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
        sim.foreground = 101;
    });

    // Refresh holes after the initial layout places 101 and 102 into their tiled slots.
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();

    // Managed windows 101 and 102 sit above Task Manager and tile the monitor -> Task Manager
    // is completely behind them (except the 6px gap between 101 and 102), so neither
    // 101 nor 102's area is punched out of the overlay!
    let r101 = h.rect(101);
    let r102 = h.rect(102);
    let holes_behind = h.desktop.0.lock().unwrap().scenes.last().unwrap().holes.clone();
    for hole in &holes_behind {
        let ov1 = (hole.x + hole.w).min(r101.x + r101.w) - hole.x.max(r101.x);
        let ov2 = (hole.x + hole.w).min(r102.x + r102.w) - hole.x.max(r102.x);
        assert!(ov1 <= 0, "Task Manager behind 101 must not punch a hole over 101: hole={hole:?} r101={r101:?}");
        assert!(ov2 <= 0, "Task Manager behind 102 must not punch a hole over 102: hole={hole:?} r102={r102:?}");
    }
    assert_eq!(tile_color(&h, 101), Some(0x3B82F6), "focus highlight remains intact on 101");

    // Raise Task Manager in front of both managed windows -> full rect is now carved as a hole.
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.z_order.retain(|&id| id != 901);
        g.z_order.insert(0, 901);
    }
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();
    let holes_front = h.desktop.0.lock().unwrap().scenes.last().unwrap().holes.clone();
    assert_eq!(holes_front, vec![tm_rect], "Task Manager in front of managed tiles cuts its full rect");
    h.stop_clean();
}

#[test]
fn issue_17_system_flyouts_and_note_box_punch_holes_only_when_visible_and_uncloaked() {
    let start_rect = Rect { x: 640, y: 300, w: 640, h: 700 };
    let note_rect = Rect { x: 680, y: 340, w: 560, h: 112 };
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
        let mut start = SimWindow::new(910, start_rect);
        start.class = "Windows.UI.Core.CoreWindow".into();
        start.topmost = true;
        start.cloaked = true; // closed Start menu is cloaked by DWM
        sim.add_window(start);
        let mut note = SimWindow::new(911, note_rect);
        note.class = "Planor.Note".into();
        note.toolwindow = true;
        note.topmost = true;
        note.visible = false;
        sim.add_window(note);
        sim.foreground = 101;
    });

    assert!(
        h.desktop.0.lock().unwrap().scenes.last().unwrap().holes.is_empty(),
        "cloaked Start menu and hidden note box must not punch holes"
    );

    // Open Start menu (uncloak) + show Win+W note box -> both punch holes.
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&910).unwrap().cloaked = false;
        g.windows.get_mut(&911).unwrap().visible = true;
    }
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();
    let holes = h.desktop.0.lock().unwrap().scenes.last().unwrap().holes.clone();
    assert!(holes.contains(&start_rect), "visible Start menu must punch a hole: {holes:?}");
    assert!(holes.contains(&note_rect), "visible Win+W note box must punch a hole: {holes:?}");

    // Close both -> holes vanish.
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&910).unwrap().cloaked = true;
        g.windows.get_mut(&911).unwrap().visible = false;
    }
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert!(
        h.desktop.0.lock().unwrap().scenes.last().unwrap().holes.is_empty(),
        "holes must clear when Start menu and note box close"
    );
    h.stop_clean();
}

#[test]
fn issue_17_edge_resize_does_not_show_green_drop_target_border_or_zones() {
    let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
        sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
        sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
        sim.foreground = 101;
    });

    let r1 = h.rect(101);
    let r2 = h.rect(102);
    // Start edge resize of 101 (Windows sends EVENT_SYSTEM_MOVESIZESTART for resizes too).
    h.desktop.0.lock().unwrap().cursor = (r1.x + r1.w, r1.y + r1.h / 2);
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
    h.settle_quiet();

    // Drag right edge of 101 into 102's area (width grows from r1.w to r1.w + 200).
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&101).unwrap().rect.w = r1.w + 200;
        g.cursor = (r2.x + 150, r2.y + r2.h / 2);
    }
    h.engine.tick().unwrap();
    h.settle_quiet();

    assert!(zones(&h).is_empty(), "edge resize must not show green stack drop zones");
    assert_ne!(
        tile_color(&h, 102),
        h.engine.hot.drop_target_color,
        "edge resize must not draw green drop-target border on neighbor"
    );
    assert_eq!(h.engine.drag.as_ref().map(|d| d.resizing), Some(true));

    // Finish resize -> column resizes normally.
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
    h.settle_quiet();
    assert_eq!(h.rect(101).w, r1.w + 200);
    h.stop_clean();
}

#[test]
fn issue_50_51_moving_unmanaged_window_after_open_close_sends_neutral_zero_duration_scene() {
    let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
        sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
        let mut tm = SimWindow::new(901, Rect { x: 200, y: 200, w: 500, h: 400 });
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        sim.add_window(tm);
        sim.foreground = 101;
    });

    // Open a new window -> triggers an `open_close` scene.
    let w2 = h
        .desktop
        .0
        .lock()
        .unwrap()
        .add_window(SimWindow::new(102, Rect { x: 100, y: 100, w: 800, h: 600 }));
    h.engine.event(Event::Window(EVENT_OBJECT_SHOW, w2));
    h.settle();
    let open_scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
    assert_eq!(open_scene.action, "open_close");

    // Move Task Manager -> hole refresh must send an `OverlayUpdate` (NOT a new Scene epoch or
    // animated `"open_close"` handoff!).
    let seq_before = h.engine.seq;
    let ov_before = h.desktop.0.lock().unwrap().overlay_updates;
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.z_order.retain(|&id| id != 901);
        g.z_order.insert(0, 901);
        g.windows.get_mut(&901).unwrap().rect = Rect { x: 320, y: 240, w: 500, h: 400 };
    }
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert_eq!(h.engine.seq, seq_before, "hole refresh must not supersede scene epoch");
    assert_eq!(h.desktop.0.lock().unwrap().overlay_updates, ov_before + 1);
    let hole_scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
    assert_eq!(hole_scene.holes, vec![Rect { x: 320, y: 240, w: 500, h: 400 }]);
    h.stop_clean();
}

#[test]
fn issue_50_51_focus_open_and_hole_change_mid_animation_preserve_motion() {
    let mut cfg = note_cfg();
    cfg["animation"]["enabled"] = toml::Value::Boolean(true);
    cfg["animation"]["duration_ms"] = toml::Value::Integer(180);
    let mut h = SimHarness::with_setup(primary_only_rig(), cfg, |sim| {
        sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
        sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
        sim.add_window(SimWindow::new(103, Rect { x: 1920, y: 0, w: 900, h: 1000 }));
        let mut tm = SimWindow::new(901, Rect { x: 200, y: 200, w: 500, h: 400 });
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        sim.add_window(tm);
        sim.foreground = 101;
    });

    // 1. Focus key -> 180 ms scroll scene, followed immediately by EVENT_SYSTEM_FOREGROUND:
    //    must NOT bump scene epoch or shorten the 180 ms scroll animation!
    h.cmd(Command::Key { name: "focus_right".into() });
    h.settle();
    let scroll_seq = h.engine.seq;
    let scroll_scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
    assert_eq!(scroll_scene.action, "scroll");
    assert_eq!(scroll_scene.duration, 180);

    let fg = h.desktop.0.lock().unwrap().foreground;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, fg));
    h.settle_quiet();
    assert_eq!(h.engine.seq, scroll_seq, "FOREGROUND after focus key must not cut scroll animation short");

    // 2. Open new window -> 180 ms open_close scene, followed by FOREGROUND:
    //    must NOT bump scene epoch or drop open_close handoff!
    let w4 = h
        .desktop
        .0
        .lock()
        .unwrap()
        .add_window(SimWindow::new(104, Rect { x: 100, y: 100, w: 800, h: 600 }));
    h.engine.event(Event::Window(EVENT_OBJECT_SHOW, w4));
    h.settle();
    let open_seq = h.engine.seq;
    let open_scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
    assert_eq!(open_scene.action, "open_close");
    assert_eq!(open_scene.duration, 180);

    h.desktop.0.lock().unwrap().foreground = w4;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, w4));
    h.settle_quiet();
    assert_eq!(h.engine.seq, open_seq, "FOREGROUND after open must not drop open_close animation/handoff");

    // 3. Hole change mid-animation (e.g. Task Manager exposed by parked tile) -> OverlayUpdate,
    //    NOT a new Scene epoch!
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.z_order.retain(|&id| id != 901);
        g.z_order.insert(0, 901);
        g.windows.get_mut(&901).unwrap().rect = Rect { x: 250, y: 250, w: 500, h: 400 };
    }
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert_eq!(h.engine.seq, open_seq, "hole change mid-animation must not supersede active scene");
    assert_eq!(
        h.desktop.0.lock().unwrap().scenes.last().unwrap().holes,
        vec![Rect { x: 250, y: 250, w: 500, h: 400 }]
    );
    h.stop_clean();
}

#[test]
fn issue_52_two_desktop_cloaked_managed_window_does_not_occlude_holes() {
    let tm_rect = Rect { x: 200, y: 150, w: 600, h: 500 };
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        // Managed window 101 on desktop 1.
        sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 1920, h: 1140 }));
        sim.foreground = 101;
    });

    // Switch to virtual desktop 2: desktop 1's window 101 becomes cloaked (still visible=true
    // with its on-screen rect, sitting above desktop 2's Task Manager in Z-order).
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.current_desktop = Some(2);
        g.windows.get_mut(&101).unwrap().cloaked = true;
        let mut tm = SimWindow::new(901, tm_rect);
        tm.desktop = 2;
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        g.add_window(tm);
        // Put desktop-1 cloaked window 101 ABOVE Task Manager 901 in Z-order.
        g.z_order.retain(|&id| id != 101);
        g.z_order.insert(0, 101);
        // Add a managed window 201 on desktop 2 behind Task Manager.
        let mut w201 = SimWindow::new(201, Rect { x: 0, y: 0, w: 900, h: 1000 });
        w201.desktop = 2;
        g.windows.insert(201, w201);
        g.z_order.push(201);
        g.foreground = 201;
    }
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();

    let holes = h.desktop.0.lock().unwrap().scenes.last().unwrap().holes.clone();
    assert_eq!(
        holes,
        vec![tm_rect],
        "cloaked window on desktop 1 must not occlude Task Manager hole on desktop 2"
    );
    h.stop_clean();
}

#[test]
fn issue_52_pinned_window_never_punches_hole_and_occludes_unmanaged_behind_it() {
    let tm_rect = Rect { x: 200, y: 200, w: 500, h: 400 };
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
        sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
        sim.foreground = 102;
    });

    // Pin 102 and position it directly over Task Manager 901.
    h.cmd(Command::Key { name: "toggle_pin".into() });
    h.settle();
    assert!(h.engine.pinned.contains(&102));

    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&102).unwrap().rect = tm_rect;
        let mut tm = SimWindow::new(901, tm_rect);
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        g.add_window(tm);
        // Z-order: pinned 102 in front of Task Manager 901 in front of tile 101.
        g.z_order = vec![102, 901, 101];
    }
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();

    let holes = h.desktop.0.lock().unwrap().scenes.last().unwrap().holes.clone();
    assert!(
        holes.is_empty(),
        "pinned window must not punch a hole for itself and must occlude Task Manager behind it: {holes:?}"
    );
    assert_eq!(
        tile_color(&h, 102),
        h.engine.hot.pinned_color,
        "pinned window keeps its orange border"
    );
    h.stop_clean();
}

#[test]
fn issue_48_managed_owned_dialog_keeps_highlight_while_unmanaged_owned_dialog_removes_it() {
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
        sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
        // Save As dialog owned by managed tile 101.
        let mut managed_dlg = SimWindow::new(801, Rect { x: 200, y: 200, w: 500, h: 400 });
        managed_dlg.owner = 101;
        sim.add_window(managed_dlg);
        // Elevated Task Manager + dialog owned by Task Manager.
        let mut tm = SimWindow::new(901, Rect { x: 250, y: 250, w: 600, h: 500 });
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        sim.add_window(tm);
        let mut unmanaged_dlg = SimWindow::new(902, Rect { x: 300, y: 300, w: 400, h: 300 });
        unmanaged_dlg.owner = 901;
        sim.add_window(unmanaged_dlg);
        sim.foreground = 101;
    });

    assert_eq!(tile_color(&h, 101), Some(0x3B82F6));

    // Focus dialog owned by managed tile 101 -> 101 keeps its focus highlight!
    h.desktop.0.lock().unwrap().foreground = 801;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 801));
    h.settle_quiet();
    assert_eq!(
        tile_color(&h, 101),
        Some(0x3B82F6),
        "dialog owned by managed window must keep its focus highlight"
    );

    // Focus dialog owned by unmanaged Task Manager 901 -> focus highlight is removed!
    h.desktop.0.lock().unwrap().foreground = 902;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 902));
    h.settle_quiet();
    assert_eq!(
        tile_color(&h, 101),
        None,
        "dialog owned by unmanaged window must remove focus highlight"
    );
    h.stop_clean();
}

#[test]
fn issue_48_transient_null_foreground_debounced_while_sustained_null_removes_highlight() {
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
        sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
        sim.foreground = 101;
    });

    assert_eq!(tile_color(&h, 101), Some(0x3B82F6));

    // Transient NULL for 1 tick (X -> 0 -> Y): highlight must NOT blink off.
    h.desktop.0.lock().unwrap().foreground = 0;
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert_eq!(
        tile_color(&h, 101),
        Some(0x3B82F6),
        "single-tick transient NULL foreground must not blink off highlight"
    );

    // Focus lands on 102 -> highlight moves to 102 cleanly.
    h.desktop.0.lock().unwrap().foreground = 102;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 102));
    h.settle_quiet();
    assert_eq!(tile_color(&h, 102), Some(0x3B82F6));
    assert_eq!(tile_color(&h, 101), None);

    // Sustained NULL for 2 ticks (Y -> 0 -> 0): highlight is removed.
    h.desktop.0.lock().unwrap().foreground = 0;
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert_eq!(tile_color(&h, 102), Some(0x3B82F6), "tick 1 still debounced");
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert_eq!(tile_color(&h, 102), None, "sustained NULL foreground removes highlight");
    h.stop_clean();
}

#[test]
fn issue_17_dpi_change_drag_restore_pad_drag_fling_and_corner_resizes() {
    // 1. Cross-DPI drag: window moves first, then WM_DPICHANGED resizes it mid-drag -> must
    //    stay classified as a move and transfer to MON1 (not snap back as a resize).
    {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        to_start(&mut h, "MON0");
        to_start(&mut h, "MON1");
        let r1 = h.rect(101);
        h.desktop.0.lock().unwrap().cursor = (r1.x + r1.w / 2, r1.y + 15);
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.settle_quiet();
        // Move title bar first -> latches `moved = true`.
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect = Rect { x: r1.x + 120, y: r1.y + 40, ..r1 };
            g.cursor = (r1.x + r1.w / 2 + 120, r1.y + 55);
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert_eq!(h.engine.drag.as_ref().map(|d| (d.moved, d.resizing)), Some((true, false)));
        // Cross onto MON1 and simulate WM_DPICHANGED changing window width/height!
        let r201 = h.rect(201);
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect =
                Rect { x: r201.x + 50, y: r201.y + 50, w: r1.w + 240, h: r1.h + 180 };
            g.cursor = (r201.x + r201.w / 8, r201.y + r201.h / 8);
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert_eq!(
            h.engine.drag.as_ref().map(|d| (d.moved, d.resizing)),
            Some((true, false)),
            "WM_DPICHANGED mid-drag must not flip resizing"
        );
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
        h.settle();
        assert!(
            h.engine.strips[&(1, "MON1".into())].locate(101).is_some(),
            "cross-DPI drag must transfer 101 to MON1"
        );
        h.stop_clean();
    }

    // 2. Drag-restore: window shrinks AND moves both edges on first report -> classified as move,
    //    swaps with neighbor on drop instead of resizing.
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
            sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
            sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
            sim.foreground = 101;
        });
        let r1 = h.rect(101);
        let r2 = h.rect(102);
        h.desktop.0.lock().unwrap().cursor = (r1.x + r1.w / 2, r1.y + 15);
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.settle_quiet();
        // Both left & right edges shift and size shrinks (drag-restore from snapped/maximized).
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect =
                Rect { x: r2.x + 40, y: r2.y + 40, w: r1.w - 150, h: r1.h - 200 };
            g.cursor = (r2.x + r2.w / 8, r2.y + r2.h / 8);
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert_eq!(h.engine.drag.as_ref().map(|d| (d.moved, d.resizing)), Some((true, false)));
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
        h.settle();
        assert_eq!(
            h.engine.strips[&(1, "MON0".into())].columns[0].members,
            vec![102],
            "drag-restore dropped on 102 must swap instead of resizing"
        );
        h.stop_clean();
    }

    // 3. Pad drag and fling with a size change: `pad_drag` stays true after `pad.take()` in
    //    `pad_end`, so a size change during pad drag/fling never turns into a resize.
    {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        focus(&mut h, 101);
        // Start 4-finger pad drag.
        h.engine.event(Event::Contacts(vec![(200., 200.); 4]));
        h.engine.event(Event::Contacts(vec![(225., 200.); 4]));
        h.settle_quiet();
        assert!(h.engine.pad.is_some());
        assert_eq!(h.engine.drag.as_ref().map(|d| d.pad_drag), Some(true));
        // Simulate window size changing during pad drag (e.g. DPI or app relayout) and fling right.
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect.w += 180;
        }
        h.engine.event(Event::Contacts(vec![(500., 200.); 4]));
        h.settle_quiet();
        h.engine.event(Event::Contacts(vec![]));
        h.settle();
        assert!(
            h.engine.strips[&(1, "MON1".into())].locate(101).is_some(),
            "pad fling with size change must still transfer to MON1"
        );
        h.stop_clean();
    }

    // 4. Top, left, and top-left corner resizes all latch `resizing = true` (no drop zones).
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
            sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
            sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
            sim.foreground = 102;
        });
        let r2 = h.rect(102);
        for (label, next_rect) in [
            ("top edge", Rect { y: r2.y + 40, h: r2.h - 40, ..r2 }),
            ("left edge", Rect { x: r2.x + 60, w: r2.w - 60, ..r2 }),
            ("top-left corner", Rect { x: r2.x + 60, y: r2.y + 40, w: r2.w - 60, h: r2.h - 40 }),
        ] {
            let cur_r2 = h.rect(102);
            h.desktop.0.lock().unwrap().cursor = (cur_r2.x, cur_r2.y);
            h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 102));
            h.settle_quiet();
            {
                let mut g = h.desktop.0.lock().unwrap();
                g.windows.get_mut(&102).unwrap().rect = Rect {
                    x: next_rect.x - r2.x + cur_r2.x,
                    y: next_rect.y - r2.y + cur_r2.y,
                    w: next_rect.w - r2.w + cur_r2.w,
                    h: next_rect.h - r2.h + cur_r2.h,
                };
                g.cursor = (cur_r2.x - 100, cur_r2.y + 200);
            }
            h.engine.tick().unwrap();
            h.settle_quiet();
            assert_eq!(
                h.engine.drag.as_ref().map(|d| d.resizing),
                Some(true),
                "{label}: must latch resizing=true"
            );
            assert!(zones(&h).is_empty(), "{label}: must not show green drop zones");
            h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 102));
            h.settle_quiet();
        }
        h.stop_clean();
    }
}

#[test]
fn issue_17_tab_tearoff_and_unended_drag_clear_drag_session() {
    // 1. Tab tear-off: MOVESIZESTART on source window 101, MOVESIZEEND on new window 201.
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
            sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
            sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
            sim.foreground = 101;
        });
        let r1 = h.rect(101);
        let r2 = h.rect(102);
        h.desktop.0.lock().unwrap().cursor = (r1.x + 100, r1.y + 15);
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.settle_quiet();
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect = Rect { x: r1.x + 40, y: r1.y + 20, ..r1 };
            g.cursor = (r2.x + r2.w / 2, r2.y + r2.h / 2);
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert!(h.engine.drag.is_some());

        // New torn-off window 201 receives MOVESIZEEND instead of source 101.
        let new_id = h
            .desktop
            .0
            .lock()
            .unwrap()
            .add_window(SimWindow::new(201, Rect { x: 400, y: 200, w: 800, h: 600 }));
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, new_id));
        h.settle_quiet();
        assert!(h.engine.drag.is_none(), "MOVESIZEEND on torn-off window must clear source drag");
        assert!(zones(&h).is_empty(), "green drop zones must clear after mismatched MOVESIZEEND");
        assert_ne!(tile_color(&h, 102), h.engine.hot.drop_target_color);
        h.stop_clean();
    }

    // 2. Unended drag: mouse button pressed during drag and released without MOVESIZEEND.
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
            sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
            sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
            sim.foreground = 101;
        });
        let r1 = h.rect(101);
        let r2 = h.rect(102);
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.keys_down.insert(0x01); // VK_LBUTTON down
            g.cursor = (r1.x + 100, r1.y + 15);
        }
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.settle_quiet();
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect = Rect { x: r1.x + 80, y: r1.y + 20, ..r1 };
            g.cursor = (r2.x + r2.w / 2, r2.y + r2.h / 2);
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert!(h.engine.drag.is_some());
        assert!(!zones(&h).is_empty());

        // Release VK_LBUTTON without ever delivering EVENT_SYSTEM_MOVESIZEEND.
        h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
        h.desktop.0.lock().unwrap().windows.get_mut(&101).unwrap().in_movesize = Some(false);
        h.engine.tick().unwrap();
        // Fast-forward the release timestamp past the 250 ms grace period.
        if let Some(d) = h.engine.drag.as_mut() {
            d.lbutton_up_since = Some(Instant::now() - Duration::from_millis(300));
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert!(h.engine.drag.is_none(), "unended drag must clear once GetGUIThreadInfo confirms not in movesize");
        assert!(zones(&h).is_empty(), "green drop zones must clear on unended drag recovery");
        h.stop_clean();
    }

    // 3. Dragged window hidden/destroyed mid-drag clears drag session immediately.
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
            sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
            sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
            sim.foreground = 101;
        });
        let r1 = h.rect(101);
        h.desktop.0.lock().unwrap().cursor = (r1.x + 100, r1.y + 15);
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.settle_quiet();
        assert!(h.engine.drag.is_some());
        h.desktop.0.lock().unwrap().windows.get_mut(&101).unwrap().visible = false;
        h.engine.event(Event::Window(EVENT_OBJECT_DESTROY, 101));
        h.settle_quiet();
        assert!(h.engine.drag.is_none(), "destroying dragged window must clear drag session");
        h.stop_clean();
    }
}

#[test]
fn issue_17_late_movesizeend_after_release_preserves_cross_monitor_transfer() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    to_start(&mut h, "MON0");
    to_start(&mut h, "MON1");
    let r1 = h.rect(101);
    let r201 = h.rect(201);
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.keys_down.insert(0x01); // VK_LBUTTON down
        g.cursor = (r1.x + r1.w / 2, r1.y + 15);
    }
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
    h.settle_quiet();
    // Drag 101 onto MON1.
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&101).unwrap().rect = Rect { x: r201.x + 60, y: r201.y + 60, ..r1 };
        g.cursor = (r201.x + r201.w / 8, r201.y + r201.h / 8);
    }
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert!(!zones(&h).is_empty(), "drop zones visible while holding mouse button");

    // Release mouse button, while browser thread is busy doing DPI relayout (`in_movesize == None`).
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.engine.tick().unwrap();
    // Simulate 1 second elapsed after release without MOVESIZEEND yet.
    if let Some(d) = h.engine.drag.as_mut() {
        d.lbutton_up_since = Some(Instant::now() - Duration::from_secs(1));
    }
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert!(zones(&h).is_empty(), "drop visuals must clear promptly after release");
    assert!(h.engine.drag.is_some(), "drag session must stay alive for late MOVESIZEEND");

    // Late MOVESIZEEND arrives 1 s after release -> cross-monitor transfer still completes!
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
    h.settle();
    assert!(
        h.engine.strips[&(1, "MON1".into())].locate(101).is_some(),
        "late MOVESIZEEND 1 s after release must still complete transfer to MON1"
    );
    h.stop_clean();
}

#[test]
fn issue_48_dialog_owned_by_other_managed_window_highlights_owner_not_previous_active() {
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
        sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
        // Dialog owned by managed window 102 (NOT the currently active 101).
        let mut dlg = SimWindow::new(802, Rect { x: 1000, y: 200, w: 400, h: 300 });
        dlg.owner = 102;
        sim.add_window(dlg);
        sim.foreground = 101;
    });

    assert_eq!(tile_color(&h, 101), Some(0x3B82F6));
    assert_eq!(tile_color(&h, 102), None);

    // Dialog owned by 102 gains focus -> 102 gets the highlight, NOT 101!
    h.desktop.0.lock().unwrap().foreground = 802;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 802));
    h.settle_quiet();
    assert_eq!(tile_color(&h, 102), Some(0x3B82F6), "owner 102 must receive the focus highlight");
    assert_eq!(tile_color(&h, 101), None, "previous active tile 101 must lose the focus highlight");
    h.stop_clean();
}

#[test]
fn issue_17_snapped_drag_restore_with_top_edge_anchored_and_off_monitor_latched_resize() {
    // 1. Drag-restore of a snapped window sampled while top edge is still within 2 px of origin
    //    (cur.y == orig.y, cur.h < orig.h, horizontal edges shifted): must classify as `moved`,
    //    NOT `resizing`, and swap on drop!
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
            sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
            sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
            sim.foreground = 101;
        });
        let r1 = h.rect(101);
        let r2 = h.rect(102);
        h.desktop.0.lock().unwrap().cursor = (r1.x + r1.w / 2, r1.y + 15);
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.settle_quiet();
        // Top edge stays at r1.y + 1 (within 2 px!), height shrinks, AND both X edges shift.
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect =
                Rect { x: r1.x + 150, y: r1.y + 1, w: r1.w - 200, h: r1.h - 200 };
            g.cursor = (r2.x + r2.w / 8, r2.y + r2.h / 8);
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert_eq!(
            h.engine.drag.as_ref().map(|d| (d.moved, d.resizing)),
            Some((true, false)),
            "snapped drag-restore with top edge within 2 px must classify as move, not resize"
        );
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
        h.settle();
        assert_eq!(
            h.engine.strips[&(1, "MON0".into())].columns[0].members,
            vec![102],
            "snapped drag-restore dropped on 102 must swap"
        );
        h.stop_clean();
    }

    // 2. Latched edge resize dragged mostly off the home monitor (`on_home == false`):
    //    must still honor the resize instead of snapping back.
    {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
            sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 900, h: 1000 }));
            sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 900, h: 1000 }));
            sim.foreground = 102;
        });
        let r2 = h.rect(102);
        h.desktop.0.lock().unwrap().cursor = (r2.x + r2.w, r2.y + r2.h / 2);
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 102));
        h.settle_quiet();
        // Drag right edge far onto MON1 so >50% of the window's area sits past MON0's right edge (1920).
        let huge_w = 2400;
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&102).unwrap().rect = Rect { w: huge_w, ..r2 };
            g.cursor = (r2.x + huge_w, r2.y + r2.h / 2);
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert_eq!(h.engine.drag.as_ref().map(|d| d.resizing), Some(true));
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 102));
        h.settle();
        assert!(
            h.engine.strips[&(1, "MON0".into())].columns[1].length > r2.w,
            "latched edge resize dragged mostly off home monitor must resize, not snap back"
        );
        h.stop_clean();
    }
}

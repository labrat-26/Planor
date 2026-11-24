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

        let desktop_render = backend.clone();
        let epoch_render = rendered_epoch.clone();
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
                self.engine.layout().unwrap();
                if self.engine.seq > prev {
                    let target = self.engine.seq;
                    let until = Instant::now() + Duration::from_secs(2);
                    while self.rendered_epoch.load(Ordering::Acquire) < target {
                        assert!(Instant::now() < until, "sim renderer timed out at seq {target}");
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
                    render::Message::Warm(_) => {}
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
    // Win + wheel rolls the focused group (#35); Win + Shift + wheel scrolls the strip instead.
    focus(&mut h, 101);
    h.cmd(Command::Key { name: "stack_toggle".into() });
    h.settle();
    let s = &h.engine.strips[&land];
    assert!(s.columns[s.focus].members.len() >= 2);
    let active = s.active();
    wheel(&mut h, 120, false, false, 900, 500);
    settle_now(&mut h);
    let s = &h.engine.strips[&land];
    assert_ne!(s.active(), active, "Win+wheel rolled the focused group");
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
            h.engine.inertia = Some((vertical, -1200.0, Instant::now() - Duration::from_millis(8), Some(96.0)));
            assert!(h.engine.wants_fast_tick(), "{mon}@{hz}Hz wants fast tick during inertia");
            for _ in 0..4 {
                h.engine.fast_tick();
                h.settle_quiet();
                if let Some((v, s, _, d)) = h.engine.inertia {
                    h.engine.inertia = Some((v, s, Instant::now() - Duration::from_millis(planor::pacing::fast_tick_ms(hz)), d));
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

#[test]
fn edge_resize_suppresses_merge_boxes_and_borders_while_move_shows_them() {
    use crate::daemon::is_edge_resize;

    // Unit-level classification checks: 4 edges, 4 corners, cross-DPI move, snapped drag-restore.
    let orig = Rect { x: 100, y: 50, w: 1200, h: 900 };
    assert!(is_edge_resize(orig, Rect { x: 100, y: 50, w: 900, h: 900 }), "right edge resize");
    assert!(is_edge_resize(orig, Rect { x: 250, y: 50, w: 1050, h: 900 }), "left edge resize");
    assert!(is_edge_resize(orig, Rect { x: 100, y: 50, w: 1200, h: 700 }), "bottom edge resize");
    assert!(is_edge_resize(orig, Rect { x: 100, y: 150, w: 1200, h: 800 }), "top edge resize");
    assert!(is_edge_resize(orig, Rect { x: 200, y: 150, w: 1100, h: 800 }), "top-left corner resize");
    assert!(
        !is_edge_resize(orig, Rect { x: 1600, y: 60, w: 960, h: 720 }),
        "cross-DPI move shifts both horizontal edges"
    );
    assert!(
        !is_edge_resize(orig, Rect { x: 220, y: 50, w: 900, h: 700 }),
        "snapped drag-restore keeps top edge near orig.y but shifts both horizontal edges"
    );

    // Simulation test (#37) on both landscape (MON0) and portrait (MON2).
    for (mon, first_id, second_id, resize_fn) in [
        (
            "MON0",
            101,
            102,
            (|r: Rect| Rect { w: r.w - 220, ..r }) as fn(Rect) -> Rect,
        ),
        (
            "MON2",
            301,
            302,
            (|r: Rect| Rect { h: r.h - 260, ..r }) as fn(Rect) -> Rect,
        ),
    ] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        focus(&mut h, first_id);
        let r1 = h.rect(first_id);
        let r2 = h.rect(second_id);
        let cursor_over_second = (r2.x + r2.w / 2, r2.y + r2.h / 2);

        let key = (h.engine.desktop.unwrap(), mon.to_string());

        // 1. Edge resize: merge zones & drop borders must NOT appear even if cursor enters neighbour.
        h.desktop.0.lock().unwrap().cursor = cursor_over_second;
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, first_id));
        h.desktop.0.lock().unwrap().windows.get_mut(&first_id).unwrap().rect = resize_fn(r1);
        h.engine.tick().unwrap();
        h.settle_quiet();

        let last = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
        assert!(last.zones.is_empty(), "{mon}: resize must not emit merge zones");
        assert!(
            h.engine.drag.as_ref().is_some_and(|d| d.resizing),
            "{mon}: drag session should latch resizing=true"
        );

        // Complete resize -> column shrinks, does NOT group/swap with second_id.
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, first_id));
        h.settle();
        let after_resize = h.rect(first_id);
        if mon == "MON0" {
            assert_eq!(after_resize.w, r1.w - 220, "{mon}: width resized");
        } else {
            assert_eq!(after_resize.h, r1.h - 260, "{mon}: height resized");
        }
        assert!(
            h.engine.strips[&key].locate(first_id).unwrap().0
                != h.engine.strips[&key].locate(second_id).unwrap().0,
            "{mon}: resize must not group windows"
        );

        // 2. Real move: merge zones DO appear and dropping in zone groups the windows.
        let r1_now = h.rect(first_id);
        let r2_now = h.rect(second_id);
        let target_center = (r2_now.x + r2_now.w / 2, r2_now.y + r2_now.h / 2);
        h.desktop.0.lock().unwrap().cursor = (r1_now.x + 20, r1_now.y + 10);
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, first_id));
        {
            let mut d = h.desktop.0.lock().unwrap();
            d.windows.get_mut(&first_id).unwrap().rect = Rect {
                x: r1_now.x + 120,
                y: r1_now.y + 80,
                ..r1_now
            };
            d.cursor = target_center;
        }
        h.engine.tick().unwrap();
        h.settle_quiet();

        let move_scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
        assert!(!move_scene.zones.is_empty(), "{mon}: real move must show merge zones");
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, first_id));
        h.settle();
        assert_eq!(
            h.engine.strips[&key].locate(first_id).unwrap().0,
            h.engine.strips[&key].locate(second_id).unwrap().0,
            "{mon}: dropping inside stack zone must group windows"
        );
        h.stop_clean();
    }
}

#[test]
fn drag_to_group_with_left_and_bottom_windows_while_scrolled_syncs_drop_zones() {
    // Covers #18 (grouping with left and bottom windows, landscape + portrait, while scrolled)
    // and #55 (instant sync of drop boxes during edge-scroll + rate-limited edge-scroll).
    let mut anim_cfg = note_cfg();
    anim_cfg["animation"]["enabled"] = toml::Value::Boolean(true);
    anim_cfg["animation"]["duration_ms"] = toml::Value::Integer(180);

    // 1. Landscape (MON0, 1920x1152): 3 windows scrolled sideways so left window (101) is partially visible.
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), anim_cfg.clone(), |sim| {
            sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 960, h: 1152 }));
            sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 960, h: 1152 }));
            sim.add_window(SimWindow::new(103, Rect { x: 1920, y: 0, w: 960, h: 1152 }));
        });
        focus(&mut h, 103);
        // Scroll partially so 101 peeks on the left, 102 in middle, 103 on right.
        let key = (h.engine.desktop.unwrap(), "MON0".to_string());
        h.engine.strips.get_mut(&key).unwrap().offset = 400;
        h.engine.dirty = true;
        h.settle();

        let r103 = h.rect(103);
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 103));
        // Move window 103 toward the left edge to trigger edge-scroll.
        {
            let mut d = h.desktop.0.lock().unwrap();
            d.windows.get_mut(&103).unwrap().rect = Rect { x: r103.x - 300, y: 40, ..r103 };
            d.cursor = (10, 500);
        }
        let off_before = h.engine.strips[&key].offset;
        h.engine.tick().unwrap();
        let off_after_one = h.engine.strips[&key].offset;
        assert!(off_after_one < off_before, "edge-scroll should scroll left toward left windows");
        assert!(
            h.engine.instant,
            "edge-scroll must mark scene instant so drop boxes never lag behind animating windows"
        );
        // Calling tick() again immediately (<30ms) must be rate-limited so left windows aren't flung away.
        h.engine.tick().unwrap();
        assert_eq!(
            h.engine.strips[&key].offset,
            off_after_one,
            "edge-scroll must be rate-limited against 120Hz bursts"
        );
        h.settle_quiet();

        // Verify left window (101) has a drop zone inside its visible rect, and dropping there groups 103 with 101.
        let zones = crate::daemon::drop_zones(&h.engine.strips[&key], 96, 50, 103);
        let left_zone = zones
            .iter()
            .find(|(id, _, _)| *id == 101)
            .expect("left window 101 must have a drop zone");
        assert!(left_zone.2.x >= 0 && left_zone.2.x + left_zone.2.w <= 1920);
        h.desktop.0.lock().unwrap().cursor = (
            left_zone.2.x + left_zone.2.w / 2,
            left_zone.2.y + left_zone.2.h / 2,
        );
        h.engine.tick().unwrap();
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 103));
        h.settle();
        assert_eq!(
            h.engine.strips[&key].locate(103).unwrap().0,
            h.engine.strips[&key].locate(101).unwrap().0,
            "dropping 103 into scrolled left window 101's drop zone must group them"
        );
        h.stop_clean();
    }

    // 2. Portrait (MON2, -1440,0,1440,2512): 3 windows scrolled so bottom window (303) is partially visible.
    {
        let mut h = SimHarness::with_setup(three_monitor_rig(), anim_cfg, three_per_monitor);
        focus(&mut h, 301);
        let key = (h.engine.desktop.unwrap(), "MON2".to_string());
        h.engine.strips.get_mut(&key).unwrap().offset = 300;
        h.engine.dirty = true;
        h.settle();

        let r301 = h.rect(301);
        let zones = crate::daemon::drop_zones(&h.engine.strips[&key], 96, 50, 301);
        let bottom_zone = zones
            .iter()
            .find(|(id, _, _)| *id == 303)
            .expect("bottom window 303 on portrait monitor must have a drop zone");
        assert!(
            bottom_zone.2.y >= 0 && bottom_zone.2.y + bottom_zone.2.h <= 2512,
            "portrait bottom drop zone must stay within monitor work area"
        );

        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 301));
        {
            let mut d = h.desktop.0.lock().unwrap();
            d.windows.get_mut(&301).unwrap().rect = Rect { x: r301.x + 80, y: r301.y + 400, ..r301 };
            d.cursor = (
                bottom_zone.2.x + bottom_zone.2.w / 2,
                bottom_zone.2.y + bottom_zone.2.h / 2,
            );
        }
        h.engine.tick().unwrap();
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 301));
        h.settle();
        assert_eq!(
            h.engine.strips[&key].locate(301).unwrap().0,
            h.engine.strips[&key].locate(303).unwrap().0,
            "dropping 301 into bottom window 303's drop zone on portrait monitor must group them"
        );
        h.stop_clean();
    }
}

#[test]
fn repro_225940320_pad_drag_zones_match_drawn_tiles_and_left_drop_groups() {
    // Reproduces exact bundle 225940320 layout (1920x1140 work area, 1920px windows, offset 1455)
    // using the real 4-finger pad_drag path (Event::Contacts).
    let rig = vec![(
        "LEN403A".to_string(),
        Rect { x: 0, y: 0, w: 1920, h: 1140 },
        Rect { x: 0, y: 0, w: 1920, h: 1200 },
    )];
    let mut cfg = note_cfg();
    cfg["animation"]["enabled"] = toml::Value::Boolean(true);
    cfg["animation"]["duration_ms"] = toml::Value::Integer(180);
    cfg["layout"]["stack_peek_px"] = toml::Value::Integer(28);

    let mut h = SimHarness::with_setup(rig, cfg, |sim| {
        // Add in reverse z-order so scan() builds columns [1376768, 11339466, 1049722, 26216776].
        sim.add_window(SimWindow::new(26216776, Rect { x: 0, y: 0, w: 1920, h: 1140 }));
        sim.add_window(SimWindow::new(1049722, Rect { x: 0, y: 0, w: 957, h: 1140 }));
        sim.add_window(SimWindow::new(11339466, Rect { x: 0, y: 0, w: 1920, h: 1140 }));
        sim.add_window(SimWindow::new(1376768, Rect { x: 0, y: 0, w: 1920, h: 1140 }));
    });
    let key = (h.engine.desktop.unwrap(), "LEN403A".to_string());
    {
        let s = h.engine.strips.get_mut(&key).unwrap();
        assert_eq!(s.columns[0].members, vec![1376768]);
        assert_eq!(s.columns[1].members, vec![11339466]);
        assert_eq!(s.columns[2].members, vec![1049722]);
        assert_eq!(s.columns[3].members, vec![26216776]);
        s.columns[0].length = 1920;
        s.columns[1].length = 1920;
        s.columns[2].length = 957;
        s.columns[3].length = 1920;
        s.focus = 3;
        s.offset = 1455;
    }
    h.desktop.0.lock().unwrap().foreground = 26216776;
    h.engine.dirty = true;
    h.settle();

    // Start 4-finger pad drag via Event::Contacts (origin -> move past deadzone -> DragStart).
    let pts = |dx: f64, dy: f64| {
        vec![
            (100.0 + dx, 100.0 + dy),
            (120.0 + dx, 100.0 + dy),
            (140.0 + dx, 100.0 + dy),
            (160.0 + dx, 100.0 + dy),
        ]
    };
    h.engine.event(Event::Contacts(pts(0.0, 0.0)));
    h.engine.event(Event::Contacts(pts(-40.0, 0.0)));
    assert!(h.engine.pad.is_some(), "4-finger swipe must start pad drag");
    assert!(
        h.engine.drag.as_ref().is_some_and(|d| d.pad_drag && d.moved),
        "pad drag must mark pad_drag=true and moved=true"
    );

    // Move cursor over left window 1376768's visible drop zone (x in 116..348) and update.
    h.desktop.0.lock().unwrap().cursor = (232, 570);
    if let Some(p) = h.engine.pad.as_mut() {
        p.cursor = (232.0, 570.0);
    }
    h.engine.tick().unwrap();
    h.settle_quiet();

    let drag_scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
    assert_eq!(drag_scene.duration, 0, "drag hover scene must be instant so zones match tiles");
    assert_eq!(drag_scene.zones.len(), 2, "both visible neighbours (1376768 and 11339466) show zones");

    // Assert EVERY zone against its own tile's clipped rect and stack_zone(...).
    let tile_rect = |id: isize| {
        drag_scene
            .tiles
            .iter()
            .find(|&&(tid, _)| tid == id)
            .map(|&(_, r)| r)
            .unwrap()
    };
    let clip_to_work = |r: Rect| {
        let x = r.x.max(0);
        let y = r.y.max(0);
        let w = (r.x + r.w).min(1920) - x;
        let h = (r.y + r.h).min(1140) - y;
        Rect { x, y, w, h }
    };
    let expected_left_zone = planor::stack_zone(clip_to_work(tile_rect(1376768)), 50);
    let expected_right_zone = planor::stack_zone(clip_to_work(tile_rect(11339466)), 50);
    assert_eq!(tile_rect(1376768), Rect { x: -1455, y: 0, w: 1920, h: 1140 });
    assert_eq!(tile_rect(11339466), Rect { x: 471, y: 0, w: 1920, h: 1140 });
    assert_eq!(
        drag_scene.zones.iter().map(|z| z.rect).collect::<Vec<_>>(),
        vec![expected_left_zone, expected_right_zone],
        "each drop zone must match stack_zone(clip(own_tile, work_area), 50) exactly"
    );

    // 1. Exact recorded drop from bundle 225940320: cursor near (1401, 633) over 11339466 (471..1920).
    h.desktop.0.lock().unwrap().cursor = (1401, 633);
    if let Some(p) = h.engine.pad.as_mut() {
        p.cursor = (1401.0, 633.0);
    }
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    {
        let s = &h.engine.strips[&key];
        assert_eq!(s.locate(26216776).unwrap().0, 1, "recorded drop at (1401,633) groups onto 11339466 at col 1");
        assert_eq!(s.locate(11339466).unwrap().0, 1);
        assert_eq!(s.offset, 1926, "full-width 1920px group reveals at offset 1926 so focused window is unparked");
        assert_eq!(s.locate(1376768).unwrap().0, 0, "left window 1376768 remains intact at col 0");
    }
    // Scrolling/focusing left immediately restores the left window at x = 0.
    h.cmd(Command::Key { name: "focus_left".into() });
    h.settle();
    assert_eq!(h.engine.strips[&key].offset, 0);
    assert_eq!(h.rect(1376768), Rect { x: 0, y: 0, w: 1920, h: 1140 });
    h.stop_clean();
}

#[test]
fn fast_tick_drives_smooth_refresh_paced_edge_scroll() {
    // Finding #3: edge scrolling runs via wants_fast_tick / fast_tick at display refresh rate,
    // without requiring 32 ms tick() or pad_move() calls.
    let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
        sim.add_window(SimWindow::new(103, Rect { x: 1920, y: 0, w: 960, h: 1152 }));
        sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 960, h: 1152 }));
        sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 960, h: 1152 }));
    });
    let key = (h.engine.desktop.unwrap(), "MON0".to_string());
    focus(&mut h, 103);
    h.engine.strips.get_mut(&key).unwrap().offset = 600;
    h.engine.dirty = true;
    h.settle();

    let r103 = h.rect(103);
    h.desktop.0.lock().unwrap().cursor = (r103.x + 100, 500);
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 103));
    {
        let mut d = h.desktop.0.lock().unwrap();
        d.windows.get_mut(&103).unwrap().rect = Rect { x: r103.x - 200, y: 40, ..r103 };
        d.cursor = (10, 500); // within 48px of left edge
    }
    // First tick enters edge-scroll and arms wants_fast_tick().
    h.engine.tick().unwrap();
    assert!(h.engine.wants_fast_tick(), "active drag at strip edge must request fast_tick");

    // Drive edge scrolling exclusively via fast_tick() at ~60 Hz (17 ms intervals).
    let start_offset = h.engine.strips[&key].offset;
    let start_t = Instant::now();
    let mut steps = 0;
    for _ in 0..4 {
        thread::sleep(Duration::from_millis(18));
        let before = h.engine.strips[&key].offset;
        h.engine.fast_tick();
        let after = h.engine.strips[&key].offset;
        assert!(after < before, "fast_tick must advance edge scroll smoothly");
        assert!(
            before - after <= 45,
            "each fast_tick step must stay small and clamped (got {})",
            before - after
        );
        steps += 1;
    }
    let elapsed_s = start_t.elapsed().as_secs_f64();
    let total_px = (start_offset - h.engine.strips[&key].offset) as f64;
    let speed = total_px / elapsed_s;
    assert_eq!(steps, 4);
    assert!(
        (500.0..=1300.0).contains(&speed),
        "fast_tick edge-scroll speed should be ~900 px/s (got {speed:.1} px/s over {elapsed_s:.3}s)"
    );

    // Moving cursor away from the edge disarms wants_fast_tick().
    h.desktop.0.lock().unwrap().cursor = (500, 500);
    h.engine.fast_tick();
    assert!(!h.engine.wants_fast_tick(), "leaving edge zone must disarm fast_tick");
    h.stop_clean();
}

#[test]
fn win_wheel_cycles_group_members_and_burst_never_traps_on_both_orientations() {
    // Repro of #35 (bundle 000625060) + Finding #5:
    // - Focused group: plain Win+wheel cycles members (clamping cleanly at pair ends, wrapping on 3+);
    //   Win+Shift+wheel does the opposite (scrolls the strip away from the group).
    // - Focused ungrouped column: plain Win+wheel scrolls the strip, and a burst that scrolls across
    //   a group keeps scrolling the strip without getting trapped.
    for (mon, w1, _w2, w3, cursor) in [
        ("MON0", 101, 102, 103, (500, 500)),
        ("MON2", 301, 302, 303, (-700, 500)),
    ] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
            three_per_monitor(sim);
            // Add a 4th window on each monitor so the strip has scrollable overflow even with a 2-window group.
            sim.add_window(SimWindow::new(104, Rect { x: 1000, y: 100, w: 600, h: 400 }));
            sim.add_window(SimWindow::new(304, Rect { x: -1300, y: 1600, w: 900, h: 400 }));
        });
        let key = (h.engine.desktop.unwrap(), mon.to_string());
        focus(&mut h, w1);
        h.cmd(Command::Key { name: "stack_toggle".into() });
        h.settle();
        let s = &h.engine.strips[&key];
        assert_eq!(s.columns[s.focus].members.len(), 2, "{mon}: col 0 is a 2-window pair");
        let first_active = s.active().unwrap();
        let first_idx = s.columns[s.focus].active;
        let step_delta = if first_idx == 0 { -120 } else { 120 };

        // 1. Plain Win+wheel on focused 2-window pair cycles to the other member.
        h.engine.event(Event::Wheel { delta: step_delta, tilt: false, shift: false, x: cursor.0, y: cursor.1 });
        h.settle();
        let second_active = h.engine.strips[&key].active().unwrap();
        assert_ne!(second_active, first_active, "{mon}: Win+Scroll cycles pair member");

        // 2. Another notch in the same direction hits the pair end: no wrap, no dead scene.
        let scenes_at_end = h.desktop.0.lock().unwrap().scenes.len();
        h.engine.event(Event::Wheel { delta: step_delta, tilt: false, shift: false, x: cursor.0, y: cursor.1 });
        h.settle();
        assert_eq!(h.engine.strips[&key].active().unwrap(), second_active, "{mon}: pair clamps at end");
        assert_eq!(h.desktop.0.lock().unwrap().scenes.len(), scenes_at_end, "{mon}: clamped pair end emits no dead scene");

        // 3. Opposite notch returns to first member.
        h.engine.event(Event::Wheel { delta: -step_delta, tilt: false, shift: false, x: cursor.0, y: cursor.1 });
        h.settle();
        assert_eq!(h.engine.strips[&key].active().unwrap(), first_active, "{mon}: opposite Win+Scroll returns");

        // 4. Win+Shift+wheel on a focused group does the opposite: scrolls the strip!
        let off_before = h.engine.strips[&key].offset;
        h.engine.event(Event::Wheel { delta: -120, tilt: false, shift: true, x: cursor.0, y: cursor.1 });
        h.settle();
        assert!(h.engine.strips[&key].offset > off_before, "{mon}: Win+Shift+wheel scrolls strip away from focused group");
        settle_now(&mut h);

        // 5. Focused ungrouped column (w3): plain Win+wheel scrolls the strip, and a burst scrolling back
        //    across the group at col 0 keeps scrolling the strip without getting trapped rolling the group.
        focus(&mut h, w3);
        let group_active_before = h.engine.strips[&key].columns[0].active;
        let off_start = h.engine.strips[&key].offset;
        // Scroll back toward col 0 in a multi-notch burst (without settling between notches).
        for _ in 0..6 {
            h.engine.event(Event::Wheel { delta: 120, tilt: false, shift: false, x: cursor.0, y: cursor.1 });
        }
        h.settle();
        assert!(h.engine.strips[&key].offset < off_start, "{mon}: ungrouped Win+wheel scrolls strip");
        assert_eq!(
            h.engine.strips[&key].columns[0].active, group_active_before,
            "{mon}: strip-scroll burst across group must not get trapped rolling the group"
        );
        settle_now(&mut h);

        // 6. 3-member group: plain Win+wheel wraps around all 3 members.
        focus(&mut h, w3);
        h.cmd(Command::Key { name: "stack_toggle".into() });
        h.settle();
        let group_col = h.engine.strips[&key].focus;
        assert_eq!(h.engine.strips[&key].columns[group_col].members.len(), 3, "{mon}: 3-member group");
        let start_id = h.engine.strips[&key].active().unwrap();
        let mut seen = vec![start_id];
        for _ in 0..3 {
            h.engine.event(Event::Wheel { delta: -120, tilt: false, shift: false, x: cursor.0, y: cursor.1 });
            h.settle();
            seen.push(h.engine.strips[&key].active().unwrap());
        }
        assert_eq!(seen.last().copied(), Some(start_id), "{mon}: 3-member group wraps back to start after 3 notches");
        assert_eq!(seen[0..3].iter().collect::<std::collections::HashSet<_>>().len(), 3, "{mon}: visits all 3 members");
        h.stop_clean();
    }
}

#[test]
fn touchpad_fling_velocity_edges_shipped_inertia_one_motion_and_wrapping() {
    // Covers #16, #45, and Findings #1, #2, #3, #4, #8, #9.
    use crate::daemon::{fling_advance_direction, scroll_fling_velocity};

    // Finding #3: velocity edge cases (pause-then-lift, single report, slow drag, short flick, symmetry).
    assert_eq!(scroll_fling_velocity(&[(0.0, 25.0)]), 0.0, "single report must not be a fling");
    assert_eq!(
        scroll_fling_velocity(&[(0.09, 30.0), (0.06, 30.0)]),
        0.0,
        "pause >40ms before lift must not be a fling"
    );
    let v_slow = scroll_fling_velocity(&[(0.09, 2.0), (0.06, 2.0), (0.03, 2.0), (0.0, 2.0)]);
    assert_eq!(fling_advance_direction(v_slow, 8.0, true, 250.0, 18.0), None, "slow drag must not fling");
    assert_eq!(
        fling_advance_direction(500.0, 10.0, true, 250.0, 18.0),
        None,
        "tiny travel below min_travel must not fling"
    );
    let v_right = scroll_fling_velocity(&[(0.06, 30.0), (0.03, 30.0), (0.0, 30.0)]);
    let v_left = scroll_fling_velocity(&[(0.06, -30.0), (0.03, -30.0), (0.0, -30.0)]);
    assert!((v_right + v_left).abs() < 1e-9, "velocity must be exactly symmetric left vs right");
    assert!((v_right - 1000.0).abs() < 1e-6, "n-1 deltas over n-1 intervals: 60 / 0.06 = 1000");
    assert_eq!(fling_advance_direction(v_right, 90.0, true, 250.0, 18.0), Some(-1));
    assert_eq!(fling_advance_direction(v_left, -90.0, true, 250.0, 18.0), Some(1));
    assert_eq!(fling_advance_direction(v_right, 90.0, false, 250.0, 18.0), Some(1));
    assert_eq!(fling_advance_direction(v_left, -90.0, false, 250.0, 18.0), Some(-1));

    // Finding #4: Shipped default config (`swipe_inertia = true`): fast flick under 50% on a 3-column
    // strip advances directly to the adjacent column in ONE continuous motion (no inertia pause/second slide,
    // and never skips past the next window).
    let mut shipped_cfg = test_cfg();
    shipped_cfg["touchpad"]["swipe_inertia"] = toml::Value::Boolean(true);
    shipped_cfg["touchpad"]["swipe_natural_direction"] = toml::Value::Boolean(true);
    shipped_cfg["touchpad"]["swipe_scroll_gain"] = toml::Value::Float(4.0);
    let mut h = SimHarness::with_setup(three_monitor_rig(), shipped_cfg, three_per_monitor);
    let k0 = (h.engine.desktop.unwrap(), "MON0".to_string());
    // Make col 2 wide (1600px) so a short flick under 50% keeps focus == 2 before lift (matching bundle 212643035).
    h.engine.strips.get_mut(&k0).unwrap().columns[2].length = 1600;
    focus(&mut h, 103); // rightmost column (col 2 of [101, 102, 103])
    let pts = |x: f64| vec![(x, 100.0), (x + 20.0, 100.0), (x + 40.0, 100.0)];

    // Fast flick right (natural -> toward col 1), releasing well under 50%.
    h.engine.event(Event::Contacts(pts(100.0)));
    h.engine.event(Event::Contacts(pts(115.0)));
    h.engine.event(Event::Contacts(pts(135.0)));
    h.engine.event(Event::Contacts(pts(155.0)));
    assert_eq!(h.engine.strips[&k0].focus, 2, "still col 2 before lift");
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(h.engine.strips[&k0].focus, 1, "shipped inertia=true fling advances directly to col 1, never skipping to col 0");
    assert!(h.engine.inertia.is_none(), "one-motion fling must not leave decaying inertia");
    assert!(h.engine.settle.is_none(), "one-motion fling must not wait on a settle pause");

    // Symmetric fast flick left (natural -> toward col 2).
    h.engine.event(Event::Contacts(pts(200.0)));
    h.engine.event(Event::Contacts(pts(185.0)));
    h.engine.event(Event::Contacts(pts(165.0)));
    h.engine.event(Event::Contacts(pts(145.0)));
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(h.engine.strips[&k0].focus, 2, "symmetric fling advances directly back to col 2");
    assert!(h.engine.inertia.is_none());
    assert!(h.engine.settle.is_none());

    // Boundary-clamped inertia stops immediately without emitting redundant no-op scenes.
    focus(&mut h, 103);
    let scenes_before = h.desktop.0.lock().unwrap().scenes.len();
    h.engine.inertia = Some((false, -800.0, Instant::now() - Duration::from_millis(16), Some(96.0)));
    h.engine.fast_tick();
    assert!(h.engine.inertia.is_none(), "inertia clamped at strip boundary must stop immediately");
    assert_eq!(h.desktop.0.lock().unwrap().scenes.len(), scenes_before, "clamped inertia emits no redundant scenes");

    // Finding #2: focus key and Win+wheel during inertia clear inertia/scroll state immediately.
    h.engine.inertia = Some((false, 600.0, Instant::now() - Duration::from_millis(16), Some(96.0)));
    h.cmd(Command::Key { name: "focus_left".into() });
    h.settle();
    assert!(h.engine.inertia.is_none(), "focus key must cancel active inertia");
    assert!(h.engine.scroll_start_focus.is_none(), "focus key must clear scroll_start_focus");

    h.engine.inertia = Some((false, 600.0, Instant::now() - Duration::from_millis(16), Some(96.0)));
    h.engine.event(Event::Wheel { delta: -120, tilt: false, shift: false, x: 500, y: 500 });
    h.settle();
    assert!(h.engine.inertia.is_none(), "Win+wheel must cancel active inertia");
    settle_now(&mut h);

    // Finding #1: monitor hop (portrait MON2 -> landscape MON0 and back) with fast lift must NOT advance
    // focus on the landed monitor.
    focus(&mut h, 301);
    let mon0_active_before = h.engine.strips[&k0].active().unwrap();
    // Sideways swipe right on portrait MON2 (cross-axis) hops right to MON0.
    let hop_pts = |x: f64| vec![(x, 300.0), (x + 20.0, 300.0), (x + 40.0, 300.0)];
    h.engine.event(Event::Contacts(hop_pts(200.0)));
    h.engine.event(Event::Contacts(hop_pts(150.0)));
    h.engine.event(Event::Contacts(hop_pts(90.0)));
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    settle_now(&mut h);
    assert_eq!(h.engine.active, "MON0", "fast cross-axis swipe from MON2 hops to MON0");
    assert_eq!(
        h.engine.strips[&k0].active().unwrap(),
        mon0_active_before,
        "hop with fast lift must land on MON0's active window without an extra column advance"
    );

    // Finding #9: 3-member wrapping group fling past last member wraps to member 0 (and 0 -> 2).
    focus(&mut h, 101);
    h.cmd(Command::Key { name: "stack_toggle".into() });
    h.settle();
    focus(&mut h, 103);
    h.cmd(Command::Key { name: "stack_toggle".into() });
    h.settle();
    let last_id = {
        let s = &h.engine.strips[&k0];
        assert_eq!(s.columns[0].members.len(), 3);
        s.columns[0].members[2]
    };
    focus(&mut h, last_id);
    assert_eq!(h.engine.strips[&k0].columns[0].active, 2);
    let vpts = |y: f64| vec![(200.0, y), (220.0, y), (240.0, y)];
    // Fast upward flick under 50% (natural -> dir = +1) wraps from member 2 -> 0 in one motion.
    h.engine.event(Event::Contacts(vpts(300.0)));
    h.engine.event(Event::Contacts(vpts(285.0)));
    h.engine.event(Event::Contacts(vpts(265.0)));
    h.engine.event(Event::Contacts(vpts(245.0)));
    assert_eq!(h.engine.strips[&k0].columns[0].active, 2, "still member 2 before lift");
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(h.engine.strips[&k0].columns[0].active, 0, "3-member group fling wraps 2 -> 0");

    // Fast downward flick under 50% (natural -> dir = -1) wraps from member 0 -> 2 in one motion.
    h.engine.event(Event::Contacts(vpts(200.0)));
    h.engine.event(Event::Contacts(vpts(215.0)));
    h.engine.event(Event::Contacts(vpts(235.0)));
    h.engine.event(Event::Contacts(vpts(255.0)));
    assert_eq!(h.engine.strips[&k0].columns[0].active, 0, "still member 0 before lift");
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(h.engine.strips[&k0].columns[0].active, 2, "3-member group fling wraps 0 -> 2");
    h.stop_clean();

    // Finding #8: fitting 2- and 3-column strip (max_offset == 0) flings symmetrically left and right.
    for n_cols in [2usize, 3] {
        let mut cfg = test_cfg();
        cfg["touchpad"]["swipe_inertia"] = toml::Value::Boolean(true);
        cfg["touchpad"]["swipe_natural_direction"] = toml::Value::Boolean(true);
        let mut hf = SimHarness::with_setup(primary_only_rig(), cfg, |sim| {
            for i in 0..n_cols {
                sim.add_window(SimWindow::new(501 + i as isize, Rect { x: 100 * i as i32, y: 100, w: 500, h: 400 }));
            }
        });
        let kf = (hf.engine.desktop.unwrap(), "MON0".to_string());
        {
            let s = hf.engine.strips.get_mut(&kf).unwrap();
            let col_w = (s.area.w - s.gap * (n_cols as i32 - 1)) / n_cols as i32;
            for c in &mut s.columns {
                c.length = col_w;
            }
            s.offset = 0;
            s.focus = 0;
        }
        hf.desktop.0.lock().unwrap().foreground = 501;
        // Fling left (natural -> +1): 0 -> 1.
        hf.engine.event(Event::Contacts(pts(200.0)));
        hf.engine.event(Event::Contacts(pts(175.0)));
        hf.engine.event(Event::Contacts(pts(145.0)));
        hf.engine.event(Event::Contacts(pts(115.0)));
        hf.engine.event(Event::Contacts(vec![]));
        hf.settle();
        assert_eq!(hf.engine.strips[&kf].focus, 1, "fitting {n_cols}-col strip: fling +1 advances 0 -> 1");
        if n_cols == 3 {
            // Fling +1 again: 1 -> 2.
            hf.engine.event(Event::Contacts(pts(200.0)));
            hf.engine.event(Event::Contacts(pts(175.0)));
            hf.engine.event(Event::Contacts(pts(145.0)));
            hf.engine.event(Event::Contacts(pts(115.0)));
            hf.engine.event(Event::Contacts(vec![]));
            hf.settle();
            assert_eq!(hf.engine.strips[&kf].focus, 2, "fitting 3-col strip: fling +1 advances 1 -> 2");
            // Fling -1: 2 -> 1.
            hf.engine.event(Event::Contacts(pts(100.0)));
            hf.engine.event(Event::Contacts(pts(125.0)));
            hf.engine.event(Event::Contacts(pts(155.0)));
            hf.engine.event(Event::Contacts(pts(185.0)));
            hf.engine.event(Event::Contacts(vec![]));
            hf.settle();
            assert_eq!(hf.engine.strips[&kf].focus, 1, "fitting 3-col strip: fling -1 advances 2 -> 1");
        }
        // Fling -1: 1 -> 0.
        hf.engine.event(Event::Contacts(pts(100.0)));
        hf.engine.event(Event::Contacts(pts(125.0)));
        hf.engine.event(Event::Contacts(pts(155.0)));
        hf.engine.event(Event::Contacts(pts(185.0)));
        hf.engine.event(Event::Contacts(vec![]));
        hf.settle();
        assert_eq!(hf.engine.strips[&kf].focus, 0, "fitting {n_cols}-col strip: fling -1 advances 1 -> 0");
        hf.stop_clean();
    }
}

#[test]
fn touchpad_scroll_matches_external_reference_across_dpis_and_preserves_drag() {
    // Repro of #47 (bundle 001541529) + Round 2 Findings #4, #5, #6:
    // - 2560px @ 96 DPI matches previous external-monitor feel 1:1 (384 px / inch at gain=4.0 = 15% viewport/inch).
    // - 1920px @ 144 DPI laptop (and 3840px @ 192 DPI 4K) traverses the exact same 15% of viewport per inch.
    // - Raw-unit digitizer (`contact_dpi = None`) does not divide by cursor_dpi.
    // - Group roll normalizes to physical inches across DPIs/heights.
    // - Physical fling/inertia thresholds make the same decision at 96, 144, and 192 DPI.
    // - Captured DPI persists across pointer crossing monitors during inertia.
    use crate::daemon::{fling_advance_direction, group_roll_delta, strip_scroll_px};

    let ext_px = strip_scroll_px(96.0 * 4.0, 2560, Some(96.0));
    let lap_px = strip_scroll_px(144.0 * 4.0, 1920, Some(144.0));
    let lap_cursor_on_ext_px = strip_scroll_px(96.0 * 4.0, 1920, Some(96.0));
    let uhd_px = strip_scroll_px(192.0 * 4.0, 3840, Some(192.0));
    let raw_px = strip_scroll_px(96.0 * 4.0, 2560, None);
    assert!((ext_px - 384.0).abs() < 1e-6, "2560px @ 96 DPI preserves 384 px/in external feel (got {ext_px})");
    assert!((ext_px / 2560.0 - 0.15).abs() < 1e-6);
    assert!((lap_px / 1920.0 - 0.15).abs() < 1e-6, "1920px @ 144 DPI matches 15% viewport/in");
    assert!((lap_cursor_on_ext_px / 1920.0 - 0.15).abs() < 1e-6, "cursor monitor DPI must not skew laptop scroll");
    assert!((uhd_px / 3840.0 - 0.15).abs() < 1e-6, "universal across arbitrary resolutions/DPIs (4K @ 200%)");
    assert!((raw_px - 384.0).abs() < 1e-6, "raw-unit digitizer (None) is independent of cursor DPI");

    // Round 2 Finding #5: group roll normalized to physical inches across DPIs.
    let roll_96 = group_roll_delta(96.0 * 4.0, Some(96.0));
    let roll_144 = group_roll_delta(144.0 * 4.0, Some(144.0));
    let roll_192 = group_roll_delta(192.0 * 4.0, Some(192.0));
    assert!((roll_96 - roll_144).abs() < 1e-9 && (roll_96 - roll_192).abs() < 1e-9, "group roll identical across DPIs");
    assert!((roll_96 - 0.552).abs() < 0.002, "matches external landscape reference (~0.552 steps/in)");

    // Round 2 Finding #4: same physical flick (in/s and inches) makes the exact same fling decision at 96/144/192 DPI.
    for dpi in [96.0, 144.0, 192.0] {
        // Below threshold: 2.0 in/s, 0.25 in -> None at every DPI.
        let slow_speed = 2.0 * dpi;
        let travel = 0.25 * dpi;
        assert_eq!(
            fling_advance_direction(slow_speed / dpi, travel / dpi, true, 250.0 / 96.0, 18.0 / 96.0),
            None,
            "2.0 in/s must not fling at {dpi} DPI"
        );
        // Above threshold: 3.0 in/s, 0.25 in -> Some(-1) at every DPI.
        let fast_speed = 3.0 * dpi;
        assert_eq!(
            fling_advance_direction(fast_speed / dpi, travel / dpi, true, 250.0 / 96.0, 18.0 / 96.0),
            Some(-1),
            "3.0 in/s must fling identically at {dpi} DPI"
        );
    }

    let mut cfg = note_cfg();
    cfg["touchpad"]["swipe_inertia"] = toml::Value::Boolean(false);
    cfg["touchpad"]["swipe_natural_direction"] = toml::Value::Boolean(true);
    cfg["touchpad"]["swipe_scroll_gain"] = toml::Value::Float(4.0);

    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
    let k0 = (h.engine.desktop.unwrap(), "MON0".to_string());
    let k1 = (h.engine.desktop.unwrap(), "MON1".to_string());
    to_start(&mut h, "MON0");
    to_start(&mut h, "MON1");

    // 0.5-inch physical swipe on 144 DPI laptop (MON0, 1920px): input.rs emits 72 units.
    focus(&mut h, 101);
    h.desktop.0.lock().unwrap().contact_dpi = Some(144.0);
    let pts = |x: f64| vec![(x, 100.0), (x + 20.0, 100.0), (x + 40.0, 100.0)];
    h.engine.event(Event::Contacts(pts(300.0)));
    h.engine.event(Event::Contacts(pts(276.0))); // -24
    h.engine.event(Event::Contacts(pts(228.0))); // -48
    let frac0 = h.engine.strips[&k0].offset as f64 / h.engine.strips[&k0].area.w as f64;
    h.engine.event(Event::Contacts(vec![]));
    settle_now(&mut h);

    // Identical 0.5-inch physical swipe on 96 DPI external landscape (MON1, 2560px): input.rs emits 48 units.
    focus(&mut h, 201);
    h.desktop.0.lock().unwrap().contact_dpi = Some(96.0);
    h.engine.event(Event::Contacts(pts(300.0)));
    h.engine.event(Event::Contacts(pts(284.0))); // -16
    h.engine.event(Event::Contacts(pts(252.0))); // -32
    let frac1 = h.engine.strips[&k1].offset as f64 / h.engine.strips[&k1].area.w as f64;
    h.engine.event(Event::Contacts(vec![]));
    settle_now(&mut h);

    assert!(frac0 > 0.05 && frac1 > 0.05);
    assert!(
        (frac0 - frac1).abs() < 0.015,
        "same physical swipe scrolls same viewport fraction on 144 DPI laptop ({frac0:.4}) and 96 DPI external ({frac1:.4})"
    );

    // Round 2 Finding #6: pointer crossing monitors (changing backend.contact_dpi) during inertia
    // must keep the captured DPI for the entire coast.
    to_start(&mut h, "MON0");
    focus(&mut h, 101);
    h.engine.inertia = Some((false, -600.0, Instant::now() - Duration::from_millis(16), Some(144.0)));
    h.desktop.0.lock().unwrap().contact_dpi = Some(96.0); // pointer crossed to 96 DPI monitor mid-coast
    h.engine.fast_tick();
    assert_eq!(
        h.engine.inertia.map(|i| i.3),
        Some(Some(144.0)),
        "inertia must retain captured gesture DPI even if pointer crosses monitors mid-coast"
    );
    h.engine.inertia = None;

    // 4-finger drag on 144 DPI laptop still enters pad drag normally.
    focus(&mut h, 101);
    h.desktop.0.lock().unwrap().contact_dpi = Some(144.0);
    h.engine.event(Event::Contacts(vec![(200.0, 200.0); 4]));
    h.engine.event(Event::Contacts(vec![(230.0, 200.0); 4]));
    assert!(h.engine.pad.is_some(), "4-finger drag must still trigger normally at 144 DPI");
    h.engine.event(Event::Contacts(vec![]));
    settle_now(&mut h);
    h.stop_clean();
}

#[test]
fn round2_blocking_regressions_half_roll_settle_group_flings_and_wheel_mode_expiry() {
    // Round 2 Finding #1: cancelling a pending scroll (focus key, inertia, or wheel) must settle half-rolled frac to 0.
    let mut cfg = test_cfg();
    cfg["touchpad"]["swipe_inertia"] = toml::Value::Boolean(true);
    cfg["touchpad"]["swipe_natural_direction"] = toml::Value::Boolean(true);
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
    let k0 = (h.engine.desktop.unwrap(), "MON0".to_string());
    let k1 = (h.engine.desktop.unwrap(), "MON1".to_string());
    focus(&mut h, 101);
    h.cmd(Command::Key { name: "stack_toggle".into() });
    h.settle();
    focus(&mut h, 103);
    h.cmd(Command::Key { name: "stack_toggle".into() });
    h.settle();
    assert_eq!(h.engine.strips[&k0].columns[0].members.len(), 3);

    // 1a. Half-rolled (frac = 0.4) with settle pending -> focus key clears settle AND settles frac to 0.
    h.engine.strips.get_mut(&k0).unwrap().columns[0].frac = 0.4;
    h.engine.settle = Some(Instant::now() + Duration::from_millis(200));
    h.cmd(Command::Key { name: "focus_right".into() });
    h.settle();
    assert!(
        h.engine.strips[&k0].columns.iter().all(|c| c.frac == 0.0),
        "focus key during pending settle must settle half-rolled frac to 0"
    );

    // 1b. Half-rolled during active inertia -> focus key settles frac to 0.
    focus(&mut h, 101);
    h.engine.strips.get_mut(&k0).unwrap().columns[0].frac = 0.4;
    h.engine.inertia = Some((true, 300.0, Instant::now() - Duration::from_millis(16), Some(96.0)));
    h.cmd(Command::Key { name: "focus_right".into() });
    h.settle();
    assert!(
        h.engine.strips[&k0].columns.iter().all(|c| c.frac == 0.0),
        "focus key during inertia must settle half-rolled frac to 0"
    );

    // 1c. Half-rolled with settle pending -> Win+wheel settles frac to 0.
    focus(&mut h, 101);
    h.engine.strips.get_mut(&k0).unwrap().columns[0].frac = 0.4;
    h.engine.settle = Some(Instant::now() + Duration::from_millis(200));
    h.engine.event(Event::Wheel { delta: -120, tilt: false, shift: true, x: 500, y: 500 });
    h.settle();
    assert!(
        h.engine.strips[&k0].columns.iter().all(|c| c.frac == 0.0),
        "Win+wheel during pending settle must settle half-rolled frac to 0"
    );
    settle_now(&mut h);

    // Round 2 Finding #2a: Wrapping 3-deck fling at frac = 0.7 forward & backward must land on exact
    // adjacent member (never double-skip a member!).
    let m0 = h.engine.strips[&k0].columns[0].members[0];
    let vpts = |y: f64| vec![(200.0, y), (220.0, y), (240.0, y)];
    // Forward (+1) from member 0 with enough live travel to reach frac ≈ 0.7 (> 0.5) before lift:
    focus(&mut h, m0);
    h.engine.event(Event::Contacts(vpts(400.0)));
    h.engine.event(Event::Contacts(vpts(360.0)));
    h.engine.event(Event::Contacts(vpts(315.0)));
    h.engine.event(Event::Contacts(vpts(275.0))); // -125 -> ~0.72 steps (> 0.5)
    assert!(h.engine.strips[&k0].columns[0].frac > 0.5, "live roll crossed 0.5 before lift");
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(h.engine.strips[&k0].columns[0].active, 1, "3-deck forward fling at frac>0.5 lands on 1 (no skip to 2)");
    assert_eq!(h.engine.strips[&k0].columns[0].frac, 0.0);

    // Backward (-1) from member 1 with live travel reaching frac < -0.5 before lift -> lands on 0 (no skip to 2):
    h.engine.event(Event::Contacts(vpts(200.0)));
    h.engine.event(Event::Contacts(vpts(240.0)));
    h.engine.event(Event::Contacts(vpts(285.0)));
    h.engine.event(Event::Contacts(vpts(325.0)));
    assert!(h.engine.strips[&k0].columns[0].frac < -0.5, "live backward roll crossed -0.5 before lift");
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(h.engine.strips[&k0].columns[0].active, 0, "3-deck backward fling at frac<-0.5 lands on 0 (no skip to 2)");
    assert_eq!(h.engine.strips[&k0].columns[0].frac, 0.0);

    // Round 2 Finding #2b: 2-member pair fling 0->1 and 1->0, both under 50% and over 50%.
    h.cmd(Command::Key { name: "stack_toggle".into() }); // ungroup m0 -> remaining 2-member pair in col 0
    h.settle();
    let pair_col = h.engine.strips[&k0].columns.iter().position(|c| c.members.len() == 2).unwrap();
    let p0 = h.engine.strips[&k0].columns[pair_col].members[0];
    let p1 = h.engine.strips[&k0].columns[pair_col].members[1];
    // Pair 0 -> 1 under 50%:
    focus(&mut h, p0);
    h.engine.event(Event::Contacts(vpts(300.0)));
    h.engine.event(Event::Contacts(vpts(280.0)));
    h.engine.event(Event::Contacts(vpts(255.0)));
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(h.engine.strips[&k0].columns[pair_col].active, 1, "pair 0->1 under 50% flings to 1");
    // Pair 1 -> 0 under 50% (backward roll sets active=0, frac~0.75 before lift; must still fling cleanly to 0!):
    focus(&mut h, p1);
    h.engine.event(Event::Contacts(vpts(200.0)));
    h.engine.event(Event::Contacts(vpts(220.0)));
    h.engine.event(Event::Contacts(vpts(245.0)));
    assert!(h.engine.strips[&k0].columns[pair_col].frac > 0.5, "pair backward roll under 50% has frac > 0.5");
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(h.engine.strips[&k0].columns[pair_col].active, 0, "pair 1->0 under 50% flings cleanly to 0");
    assert!(h.engine.inertia.is_none(), "pair 1->0 fling is one motion, no inertia fallback");
    // Pair 0 -> 1 over 50%:
    h.engine.event(Event::Contacts(vpts(400.0)));
    h.engine.event(Event::Contacts(vpts(355.0)));
    h.engine.event(Event::Contacts(vpts(305.0)));
    h.engine.event(Event::Contacts(vpts(265.0)));
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(h.engine.strips[&k0].columns[pair_col].active, 1, "pair 0->1 over 50% lands on 1");
    // Pair 1 -> 0 over 50%:
    h.engine.event(Event::Contacts(vpts(200.0)));
    h.engine.event(Event::Contacts(vpts(245.0)));
    h.engine.event(Event::Contacts(vpts(295.0)));
    h.engine.event(Event::Contacts(vpts(335.0)));
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(h.engine.strips[&k0].columns[pair_col].active, 0, "pair 1->0 over 50% lands on 0");

    // Round 2 Finding #3: wheel_mode must not go stale across monitors, after pair ends, or on Shift+wheel ungrouped.
    // 3a. Hit pair end on MON0, then immediately wheel over MON1 (ungrouped): MON1 must scroll!
    focus(&mut h, p0);
    h.engine.event(Event::Wheel { delta: 120, tilt: false, shift: false, x: 500, y: 500 }); // clamped at pair start 0
    to_start(&mut h, "MON1");
    let mon1_before = h.engine.strips[&k1].offset;
    h.engine.event(Event::Wheel { delta: -120, tilt: false, shift: false, x: 2500, y: 500 });
    h.settle();
    assert!(
        h.engine.strips[&k1].offset > mon1_before,
        "wheel over MON1 after hitting pair end on MON0 must scroll MON1 strip, not stay stuck in roll mode"
    );
    settle_now(&mut h);

    // 3b. Shift+wheel on an ungrouped column must not leave a stale roll mode behind.
    to_start(&mut h, "MON1");
    let mon1_before2 = h.engine.strips[&k1].offset;
    h.engine.event(Event::Wheel { delta: -120, tilt: false, shift: true, x: 2500, y: 500 }); // ungrouped Shift+wheel
    assert!(h.engine.wheel_mode.is_none(), "inapplicable roll mode must not be stored");
    h.engine.event(Event::Wheel { delta: -120, tilt: false, shift: false, x: 2500, y: 500 });
    h.settle();
    assert!(h.engine.strips[&k1].offset > mon1_before2, "subsequent plain wheel scrolls MON1");
    settle_now(&mut h);

    // 3c. Pair end on MON0, wait past 260ms window, switch focus to ungrouped col without FOREGROUND -> plain wheel scrolls.
    focus(&mut h, p0);
    h.engine.event(Event::Wheel { delta: 120, tilt: false, shift: false, x: 500, y: 500 });
    if let Some((_, ref mut at, _, _)) = h.engine.wheel_mode {
        *at = Instant::now() - Duration::from_millis(300);
    }
    let ungrouped_col = h.engine.strips[&k0].columns.iter().position(|c| c.members.len() == 1).unwrap();
    h.engine.strips.get_mut(&k0).unwrap().focus = ungrouped_col;
    h.engine.event(Event::Wheel { delta: -120, tilt: false, shift: false, x: 500, y: 500 });
    assert_eq!(
        h.engine.wheel_mode.as_ref().map(|m| m.3),
        Some(false),
        "after 260ms window expires, wheel starts fresh burst in strip-scroll mode"
    );
    h.stop_clean();
}

#[test]
fn snapped_drag_restore_and_cross_dpi_and_back_do_not_resize() {
    // Finding #5: once a drag latches moved=true, size changes from snapped drag-restore or
    // crossing DPI and returning must NEVER be misclassified as a resize.
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let key0 = (h.engine.desktop.unwrap(), "MON0".to_string());
    focus(&mut h, 101);

    // 1. Snapped drag-restore: top edge stays at orig.y while window moves horizontally and shrinks.
    let r1 = h.rect(101);
    let r2 = h.rect(102);
    h.desktop.0.lock().unwrap().cursor = (r1.x + 100, r1.y + 10);
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
    {
        let mut d = h.desktop.0.lock().unwrap();
        d.windows.get_mut(&101).unwrap().rect = Rect {
            x: r1.x + 250,
            y: r1.y,
            w: r1.w - 180,
            h: r1.h - 200,
        };
        d.cursor = (r2.x + r2.w / 2, r2.y + r2.h / 2);
    }
    h.engine.tick().unwrap();
    assert!(
        h.engine.drag.as_ref().is_some_and(|d| d.moved && !d.resizing),
        "snapped drag-restore must latch moved=true, resizing=false"
    );
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
    h.settle();
    assert_eq!(
        h.engine.strips[&key0].locate(101).unwrap().0,
        h.engine.strips[&key0].locate(102).unwrap().0,
        "snapped drag-restore over neighbour stack zone must group, not resize"
    );
    h.stop_clean();

    // 2. Cross-DPI-and-back: move 101 to MON1 (DPI changes w/h), then return to MON0 and drop on 102.
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let key0 = (h.engine.desktop.unwrap(), "MON0".to_string());
    focus(&mut h, 101);
    let r1 = h.rect(101);
    let r2 = h.rect(102);
    h.desktop.0.lock().unwrap().cursor = (r1.x + 100, r1.y + 10);
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
    // Move onto MON1 with DPI-changed size.
    {
        let mut d = h.desktop.0.lock().unwrap();
        d.windows.get_mut(&101).unwrap().rect = Rect {
            x: 2100,
            y: 120,
            w: r1.w + 240,
            h: r1.h + 100,
        };
        d.cursor = (2200, 200);
    }
    h.engine.tick().unwrap();
    assert!(h.engine.drag.as_ref().is_some_and(|d| d.moved && !d.resizing));
    // Return to MON0 over 102's stack zone while still keeping a different size.
    {
        let mut d = h.desktop.0.lock().unwrap();
        d.windows.get_mut(&101).unwrap().rect = Rect {
            x: r2.x + 40,
            y: r2.y + 40,
            w: r1.w + 240,
            h: r1.h + 100,
        };
        d.cursor = (r2.x + r2.w / 2, r2.y + r2.h / 2);
    }
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
    h.settle();
    assert_eq!(
        h.engine.strips[&key0].locate(101).unwrap().0,
        h.engine.strips[&key0].locate(102).unwrap().0,
        "cross-DPI-and-back drop must group, not resize"
    );
    h.stop_clean();
}

#[test]
fn same_monitor_drop_animates_and_release_near_edge_does_not_scroll() {
    // Finding #6 (same-monitor drop keeps animation duration even if final drag_update changes target),
    // Finding #7 (release within 48px of edge does not scroll), and Finding #8 (smooth edge_scroll_step).
    use crate::daemon::edge_scroll_step;

    // Finding #8 unit checks:
    assert_eq!(edge_scroll_step(0.005, 60.0, -1, 0.0), None, "sub-refresh interval skipped");
    let (px60, rem60) = edge_scroll_step(1.0 / 60.0, 60.0, 1, 0.0).unwrap();
    assert_eq!(px60, 15, "900 px/s * 1/60s = 15 px");
    assert!(rem60.abs() < 1e-6);
    let (px_clamped, _) = edge_scroll_step(0.5, 60.0, -1, 0.0).unwrap();
    assert_eq!(px_clamped, -45, "stalls clamped to 50 ms -> 45 px max");
    assert_eq!(edge_scroll_step(0.02, 60.0, 0, 0.0), None, "dir=0 returns None");

    let mut anim_cfg = note_cfg();
    anim_cfg["animation"]["enabled"] = toml::Value::Boolean(true);
    anim_cfg["animation"]["duration_ms"] = toml::Value::Integer(180);

    // Finding #6: final drag_update inside drag_end changes target/mode -> drop must still animate!
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), anim_cfg.clone(), |sim| {
            sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 960, h: 1152 }));
            sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 960, h: 1152 }));
        });
        focus(&mut h, 101);
        let r1 = h.rect(101);
        let r2 = h.rect(102);
        h.desktop.0.lock().unwrap().cursor = (r1.x + 50, r1.y + 50);
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.desktop.0.lock().unwrap().windows.get_mut(&101).unwrap().rect =
            Rect { x: r1.x + 40, y: r1.y + 40, ..r1 };
        h.engine.tick().unwrap();
        h.settle_quiet();

        // Now move cursor directly into 102's stack zone and release without an intermediate tick.
        h.desktop.0.lock().unwrap().cursor = (r2.x + r2.w / 2, r2.y + r2.h / 2);
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
        h.settle();
        let drop_scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
        assert_eq!(
            drop_scene.duration, 180,
            "same-monitor drop must preserve animation duration even when final update changes target"
        );
        h.stop_clean();
    }

    // Finding #7: dropping within 48px of the strip edge must NOT edge-scroll on release.
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), anim_cfg, |sim| {
            sim.add_window(SimWindow::new(103, Rect { x: 1920, y: 0, w: 960, h: 1152 }));
            sim.add_window(SimWindow::new(102, Rect { x: 960, y: 0, w: 960, h: 1152 }));
            sim.add_window(SimWindow::new(101, Rect { x: 0, y: 0, w: 960, h: 1152 }));
        });
        focus(&mut h, 102);
        let key = (h.engine.desktop.unwrap(), "MON0".to_string());
        // Offset 400 keeps 102 (col 1: 963..1920 -> 563..1520) completely in view, so reveal() is a no-op
        // and only an illegal edge-scroll in drag_end could change offset.
        h.engine.strips.get_mut(&key).unwrap().offset = 400;
        h.engine.dirty = true;
        h.settle();

        let r102 = h.rect(102);
        h.desktop.0.lock().unwrap().cursor = (r102.x + 100, 500);
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 102));
        h.desktop.0.lock().unwrap().windows.get_mut(&102).unwrap().rect =
            Rect { x: r102.x - 200, y: 40, ..r102 };
        h.engine.tick().unwrap();
        // Place cursor at x=10 (within 48px of left edge) and release immediately.
        // Hold Shift (drag_stack_modifier_vk = 0x10) so the drop cancels cleanly and only edge-scroll could change offset.
        {
            let mut d = h.desktop.0.lock().unwrap();
            d.cursor = (10, 500);
            d.keys_down.insert(0x10);
        }
        let offset_before_release = h.engine.strips[&key].offset;
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 102));
        assert_eq!(
            h.engine.strips[&key].offset, offset_before_release,
            "drag_end must not edge-scroll the strip on release"
        );
        h.stop_clean();
    }
}

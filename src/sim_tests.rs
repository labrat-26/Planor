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
        self.assert_invariants_and_sync();
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

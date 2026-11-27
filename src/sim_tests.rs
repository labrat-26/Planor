use crate::backend::WindowBackend;
use crate::backend::sim::{FakeBackend, SimDesktop, SimWindow};
use crate::daemon::{Engine, Journal};
use crate::win32::Event;
use crate::{Command, EXAMPLE, render};
use crossbeam_channel::{Receiver, Sender};
use planor::Rect;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
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
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1152,
            },
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1200,
            },
        ),
        (
            "MON1".into(),
            Rect {
                x: 1920,
                y: 0,
                w: 2560,
                h: 1392,
            },
            Rect {
                x: 1920,
                y: 0,
                w: 2560,
                h: 1440,
            },
        ),
        (
            "MON2".into(),
            Rect {
                x: -1440,
                y: 0,
                w: 1440,
                h: 2512,
            },
            Rect {
                x: -1440,
                y: 0,
                w: 1440,
                h: 2560,
            },
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
                    render::Message::OverlayUpdate {
                        update_id,
                        holes,
                        colors,
                        ..
                    } => {
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
                        assert!(
                            Instant::now() < until,
                            "sim renderer timed out at seq {target}"
                        );
                        thread::yield_now();
                    }
                }
                if self.engine.overlay_seq > prev_ov {
                    let target = self.engine.overlay_seq;
                    let until = Instant::now() + Duration::from_secs(2);
                    while self.rendered_overlay.load(Ordering::Acquire) < target {
                        assert!(
                            Instant::now() < until,
                            "sim renderer timed out at overlay {target}"
                        );
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
            !self.engine.dirty
                || self.engine.paused
                || self.engine.locked
                || self.engine.deadline.is_some(),
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
            .unwrap_or(crate::daemon::DEFAULT_STACK_PEEK as i64) as i32;
        for ((desk, _), strip) in &self.engine.strips {
            if Some(*desk) != self.engine.desktop {
                continue;
            }
            let violations = planor::check_strip_invariants(strip, stack_peek);
            assert!(
                violations.is_empty(),
                "strip invariant violations: {:?}",
                violations
                    .iter()
                    .map(|v| v.trace_line())
                    .collect::<Vec<_>>()
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
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 100,
                y: 100,
                w: 600,
                h: 400,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 500,
                y: 100,
                w: 600,
                h: 400,
            },
        ));
        sim.add_window(SimWindow::new(
            201,
            Rect {
                x: 2050,
                y: 100,
                w: 800,
                h: 600,
            },
        ));
        sim.add_window(SimWindow::new(
            301,
            Rect {
                x: -1300,
                y: 100,
                w: 900,
                h: 600,
            },
        ));
        sim.add_window(SimWindow::new(
            302,
            Rect {
                x: -1300,
                y: 900,
                w: 900,
                h: 600,
            },
        ));
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
    assert!(
        r302.y >= r301.y + r301.h + 6,
        "portrait stack should flow top-to-bottom: {r301:?} {r302:?}"
    );

    // Directional focus walk across monitor boundary: MON0 leftmost -> focus_left -> MON2 -> focus_right -> MON0 -> MON1.
    h.cmd(Command::Key {
        name: "focus_left".into(),
    });
    h.settle();
    assert_eq!(h.engine.active, "MON2");
    assert!(matches!(h.desktop.foreground(), 301 | 302));

    h.cmd(Command::Key {
        name: "focus_right".into(),
    });
    h.settle();
    assert_eq!(h.engine.active, "MON0");
    assert_eq!(h.desktop.foreground(), 101);

    h.cmd(Command::Key {
        name: "focus_right".into(),
    });
    h.settle();
    assert_eq!(h.desktop.foreground(), 102);

    h.cmd(Command::Key {
        name: "focus_right".into(),
    });
    h.settle();
    assert_eq!(h.engine.active, "MON1");
    assert_eq!(h.desktop.foreground(), 201);

    // Explicit monitor cycle hotkeys.
    h.cmd(Command::Key {
        name: "focus_monitor_prev".into(),
    });
    h.settle();
    assert_eq!(h.engine.active, "MON0");
    h.cmd(Command::Key {
        name: "focus_monitor_prev".into(),
    });
    h.settle();
    assert_eq!(h.engine.active, "MON2");
    h.cmd(Command::Key {
        name: "focus_monitor_next".into(),
    });
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
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 100,
                y: 100,
                w: 600,
                h: 400,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 800,
                y: 100,
                w: 600,
                h: 400,
            },
        ));
        let mut dlg = SimWindow::new(
            901,
            Rect {
                x: 250,
                y: 200,
                w: 300,
                h: 180,
            },
        );
        dlg.owner = 101;
        sim.add_window(dlg);
        sim.add_window(SimWindow::new(
            301,
            Rect {
                x: -1300,
                y: 100,
                w: 900,
                h: 600,
            },
        ));
        sim.foreground = 101;
    });

    // Owned modal must NOT be placed into any strip.
    assert!(!h.engine.strips.values().any(|s| s.locate(901).is_some()));

    {
        let mut g = h.desktop.0.lock().unwrap();
        g.cursor = (400, 120);
    }
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
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
    assert_eq!(
        h.rect(102),
        Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1152
        }
    );
    // Owned modal 901 transferred inside MON2 work area and captured in journal for restore.
    let dlg_r = h.rect(901);
    assert!(
        dlg_r.x >= -1440 && dlg_r.x + dlg_r.w <= 0,
        "modal not on portrait monitor: {dlg_r:?}"
    );
    assert!(dlg_r.y >= 0 && dlg_r.y + dlg_r.h <= 2512);
    assert!(h.engine.journal.entries.contains_key(&901));

    h.stop_clean();
    // Exact original rects restored on stop for owner and modal.
    assert_eq!(
        h.rect(101),
        Rect {
            x: 100,
            y: 100,
            w: 600,
            h: 400
        }
    );
    assert_eq!(
        h.rect(901),
        Rect {
            x: 250,
            y: 200,
            w: 300,
            h: 180
        }
    );
}

fn setup_multi_monitor_windows(sim: &mut SimDesktop) {
    sim.add_window(SimWindow::new(
        101,
        Rect {
            x: 100,
            y: 100,
            w: 600,
            h: 400,
        },
    ));
    sim.add_window(SimWindow::new(
        102,
        Rect {
            x: 500,
            y: 100,
            w: 600,
            h: 400,
        },
    ));
    sim.add_window(SimWindow::new(
        103,
        Rect {
            x: 900,
            y: 100,
            w: 600,
            h: 400,
        },
    ));
    sim.add_window(SimWindow::new(
        201,
        Rect {
            x: 2100,
            y: 120,
            w: 700,
            h: 500,
        },
    ));
    sim.add_window(SimWindow::new(
        301,
        Rect {
            x: -1200,
            y: 120,
            w: 700,
            h: 500,
        },
    ));
    sim.foreground = 101;
}

#[test]
fn scn_display_offline_15s_round_trip() {
    let mut h =
        SimHarness::with_setup(three_monitor_rig(), test_cfg(), setup_multi_monitor_windows);
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
    h.cmd(Command::Key {
        name: "focus_right".into(),
    });
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
    let mut h =
        SimHarness::with_setup(three_monitor_rig(), test_cfg(), setup_multi_monitor_windows);

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
    let mut h =
        SimHarness::with_setup(three_monitor_rig(), test_cfg(), setup_multi_monitor_windows);

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
    let mut h =
        SimHarness::with_setup(three_monitor_rig(), test_cfg(), setup_multi_monitor_windows);
    let journal_path = h.engine.journal.path.clone();
    assert!(journal_path.exists());

    h.desktop.0.lock().unwrap().monitors = primary_only_rig();
    h.engine.event(Event::Topology);
    h.tick_now();
    assert!(h.desktop.is_iconic(201));

    h.stop_clean();
    assert!(!journal_path.exists());
    assert!(!h.desktop.is_iconic(201));
    assert_eq!(
        h.rect(101),
        Rect {
            x: 100,
            y: 100,
            w: 600,
            h: 400
        }
    );
    assert_eq!(
        h.rect(201),
        Rect {
            x: 2100,
            y: 120,
            w: 700,
            h: 500
        }
    );
    assert_eq!(
        h.rect(301),
        Rect {
            x: -1200,
            y: 120,
            w: 700,
            h: 500
        }
    );
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
    assert_eq!(
        backend.frame(101).unwrap(),
        Rect {
            x: 100,
            y: 100,
            w: 600,
            h: 400
        }
    );
    assert_eq!(
        backend.frame(201).unwrap(),
        Rect {
            x: 2100,
            y: 120,
            w: 700,
            h: 500
        }
    );
    assert_eq!(
        backend.frame(301).unwrap(),
        Rect {
            x: -1200,
            y: 120,
            w: 700,
            h: 500
        }
    );

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
    assert!(
        engine2.strips.is_empty(),
        "expected no re-tiling on startup"
    );
    let (stx, srx) = crossbeam_channel::bounded(1);
    let _ = engine2.renderer.send(render::Message::Stop(stx));
    let _ = srx.recv();
    let _ = jh2.join();
    let _ = std::fs::remove_dir_all(&journal_dir);
}

#[test]
fn scn_display_topology_while_dragging_cancels_cleanly() {
    let mut h =
        SimHarness::with_setup(three_monitor_rig(), test_cfg(), setup_multi_monitor_windows);
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
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
            let mut w = SimWindow::new(
                i,
                Rect {
                    x: 50 + (i as i32) * 80,
                    y: 80,
                    w: 600,
                    h: 400,
                },
            );
            if i == 2 {
                w.min_enforced_w = 1100; // simulates app rejecting <1100 px width
            }
            sim.add_window(w);
        }
        // Unmanaged external window carved as overlay hole.
        let mut ext = SimWindow::new(
            900,
            Rect {
                x: 100,
                y: 100,
                w: 300,
                h: 300,
            },
        );
        ext.class = "TaskManagerWindow".into();
        ext.process = "Taskmgr.exe".into();
        ext.thickframe = false;
        sim.add_window(ext);
        // Window on desktop 2 (initially hidden from desktop 1).
        let mut d2 = SimWindow::new(
            800,
            Rect {
                x: 50,
                y: 50,
                w: 700,
                h: 500,
            },
        );
        d2.desktop = 2;
        sim.add_window(d2);
        sim.foreground = 1;
    });

    // External window 900 excluded from strip and carved into scene holes.
    assert!(
        !h.engine.strips[&(1, "MON0".into())]
            .columns
            .iter()
            .any(|c| c.members.contains(&900))
    );
    assert!(
        h.desktop
            .0
            .lock()
            .unwrap()
            .scenes
            .last()
            .unwrap()
            .holes
            .contains(&Rect {
                x: 100,
                y: 100,
                w: 300,
                h: 300
            })
    );

    // Rejection widening bumped column containing 2 to >= 1100 px.
    assert!(
        h.rect(2).w >= 1100,
        "expected rejected width adjustment: {:?}",
        h.rect(2)
    );

    // Stack toggle merges focused window with neighbour; vertical focus cycles group members; second stack_toggle ungroups.
    h.cmd(Command::Key {
        name: "focus_right".into(),
    });
    h.settle();
    let focused_before_stack = h.desktop.foreground();
    h.cmd(Command::Key {
        name: "stack_toggle".into(),
    });
    h.settle();
    let s = &h.engine.strips[&(1, "MON0".into())];
    assert!(s.columns[s.focus].members.len() >= 2);
    h.cmd(Command::Key {
        name: "focus_down".into(),
    });
    h.settle();
    let cycled = h.desktop.foreground();
    assert_ne!(cycled, focused_before_stack);
    h.cmd(Command::Key {
        name: "stack_toggle".into(),
    });
    h.settle();

    // Move column left then right swaps columns cleanly.
    let col_order_before: Vec<isize> = h.engine.strips[&(1, "MON0".into())]
        .columns
        .iter()
        .map(|c| c.members[0])
        .collect();
    h.cmd(Command::Key {
        name: "move_left".into(),
    });
    h.settle();
    let col_order_after: Vec<isize> = h.engine.strips[&(1, "MON0".into())]
        .columns
        .iter()
        .map(|c| c.members[0])
        .collect();
    assert_ne!(col_order_before, col_order_after);

    // Pin toggle sets topmost + removes from strip, then second toggle restores to strip.
    let pin_id = h.desktop.foreground();
    h.cmd(Command::Key {
        name: "toggle_pin".into(),
    });
    h.settle();
    assert!(h.engine.pinned.contains(&pin_id));
    assert!(h.desktop.0.lock().unwrap().windows[&pin_id].topmost);
    h.cmd(Command::Key {
        name: "toggle_pin".into(),
    });
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
    h.engine
        .event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, full_id));
    h.settle();
    assert!(!h.desktop.is_zoomed(full_id));
    assert_eq!(h.rect(full_id).w, 1920);
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&full_id).unwrap().zoomed = true;
    }
    h.engine
        .event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, full_id));
    h.settle();
    assert!(h.rect(full_id).w < 1920);

    {
        let mut g = h.desktop.0.lock().unwrap();
        let w = g.windows.get_mut(&full_id).unwrap();
        w.rect = Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1200,
        };
        w.caption = false;
    }
    h.engine
        .event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, full_id));
    h.settle();
    assert_eq!(
        h.engine.strips[&(1, "MON0".into())].fullscreen_id(),
        Some(full_id)
    );
    assert_eq!(
        h.rect(full_id),
        Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1200
        }
    );
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&full_id).unwrap().caption = true;
    }
    h.engine
        .event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, full_id));
    h.settle();
    assert_eq!(h.engine.strips[&(1, "MON0".into())].fullscreen_id(), None);

    // Focusing an off-screen/parked window via EVENT_SYSTEM_FOREGROUND reveals it onto the monitor.
    h.desktop.0.lock().unwrap().foreground = 1;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 1));
    h.settle();
    assert!(h.rect(1).x >= 0 && h.rect(1).x + h.rect(1).w <= 1920);

    // Dynamic new window open, minimize, restore & destroy.
    let new_id = h.desktop.0.lock().unwrap().add_window(SimWindow::new(
        50,
        Rect {
            x: 200,
            y: 200,
            w: 500,
            h: 350,
        },
    ));
    h.engine.event(Event::Window(EVENT_OBJECT_SHOW, new_id));
    h.settle();
    assert!(
        h.engine.strips[&(1, "MON0".into())]
            .locate(new_id)
            .is_some()
    );
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&new_id).unwrap().iconic = true;
    }
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MINIMIZESTART, new_id));
    h.settle();
    assert!(
        h.engine.strips[&(1, "MON0".into())]
            .locate(new_id)
            .is_none()
    );
    {
        let mut g = h.desktop.0.lock().unwrap();
        let w = g.windows.get_mut(&new_id).unwrap();
        w.iconic = false;
        w.visible = false;
    }
    h.engine.event(Event::Window(EVENT_OBJECT_HIDE, new_id));
    h.settle();
    assert!(
        h.engine.strips[&(1, "MON0".into())]
            .locate(new_id)
            .is_none()
    );
    h.engine.event(Event::Window(EVENT_OBJECT_DESTROY, new_id));
    h.settle();

    // Virtual desktop switch discovers window 800 on desktop 2.
    h.desktop.0.lock().unwrap().current_desktop = Some(2);
    h.tick_now();
    assert_eq!(h.engine.desktop, Some(2));
    assert_eq!(
        h.rect(800),
        Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1152
        }
    );

    // Switch back to desktop 1 & trigger panic_exit -> journal restores all captured rects & deletes journal.
    h.desktop.0.lock().unwrap().current_desktop = Some(1);
    h.tick_now();
    let jpath = h.engine.journal.path.clone();
    h.cmd(Command::Key {
        name: "panic_exit".into(),
    });
    assert!(!jpath.exists());
    assert_eq!(
        h.rect(1),
        Rect {
            x: 130,
            y: 80,
            w: 600,
            h: 400
        }
    );
    assert_eq!(
        h.rect(800),
        Rect {
            x: 50,
            y: 50,
            w: 700,
            h: 500
        }
    );
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
        sim.add_window(SimWindow::new(
            101 + i,
            Rect {
                x: 100 + 300 * i as i32,
                y: 100,
                w: 600,
                h: 400,
            },
        ));
        sim.add_window(SimWindow::new(
            201 + i,
            Rect {
                x: 2100 + 300 * i as i32,
                y: 100,
                w: 600,
                h: 400,
            },
        ));
        sim.add_window(SimWindow::new(
            301 + i,
            Rect {
                x: -1300,
                y: 100 + 500 * i as i32,
                w: 900,
                h: 400,
            },
        ));
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
    assert_eq!(
        h.engine.strips[&(1, mon.to_string())].offset,
        0,
        "{mon} scrolled to its start"
    );
}

fn zones(h: &SimHarness) -> Vec<crate::cue::GestureCue> {
    h.desktop
        .0
        .lock()
        .unwrap()
        .scenes
        .last()
        .unwrap()
        .cue
        .clone()
        .into_iter()
        .collect()
}
/// Visible (strip-area clipped) rect of a window in its strip's layout: where a drag aims at it now that
/// drop zones are gone (#112). `work` is unused beyond documenting the monitor.
fn visible_rect(h: &SimHarness, id: isize, _work: Rect) -> Rect {
    let (key, s) = h
        .engine
        .strips
        .iter()
        .find(|(_, s)| s.locate(id).is_some())
        .expect("window is managed");
    let _ = key;
    let r = s.rects(96).into_iter().find(|x| x.0 == id).unwrap().1;
    crate::daemon::clip(r, s.area).expect("window is visible in its strip")
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
    assert!(
        after > before,
        "landscape: fingers left (natural) scroll the strip on: {before} -> {after}"
    );
    // Portrait: the same gesture turned with the monitor, fingers up scroll it on, continuously.
    to_start(&mut h, "MON2");
    let before = h.engine.strips[&port].offset;
    let mut seen = vec![before];
    for i in 0..=10 {
        h.engine
            .event(Event::Contacts(vec![(200., 300. - 10. * i as f64); 3]));
        h.settle();
        seen.push(h.engine.strips[&port].offset);
    }
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    let distinct = seen.windows(2).filter(|w| w[1] != w[0]).count();
    assert!(
        seen.last() > seen.first(),
        "portrait: fingers up scroll the strip on: {seen:?}"
    );
    assert!(
        distinct >= 5,
        "portrait: the strip follows the fingers step by step (smooth), not one jump: {seen:?}"
    );
    assert_eq!(
        h.engine.active, "MON2",
        "portrait: a vertical swipe no longer hops focus between monitors"
    );
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
    assert_eq!(
        h.engine.active, "MON0",
        "portrait: a sideways swipe hops right to MON0"
    );
    // Landscape MON0: vertical fingers are across its strip; with no monitor above or below nothing hops
    // and nothing scrolls.
    let before = h.engine.strips[&(1, "MON0".to_string())].offset;
    swipe(&mut h, 3, (200., 400.), (200., 100.), 12);
    settle_now(&mut h);
    assert_eq!(h.engine.active, "MON0");
    assert_eq!(
        h.engine.strips[&(1, "MON0".to_string())].offset,
        before,
        "vertical fingers never scroll a landscape strip"
    );
    // A long sideways swipe from the portrait hops once; it does not run on into MON0's strip.
    focus(&mut h, 301);
    assert_eq!(h.engine.active, "MON2");
    let land_before = h.engine.strips[&(1, "MON0".to_string())].offset;
    swipe(&mut h, 3, (900., 200.), (0., 200.), 40);
    assert_eq!(h.engine.active, "MON0", "hopped right");
    assert_eq!(
        h.engine.strips[&(1, "MON0".to_string())].offset,
        land_before,
        "the rest of the gesture was ignored"
    );
    h.stop_clean();
}

/// #108 review L1: a swipe that hops monitors ignores the rest of the gesture, so lifting changes no layout.
/// The renderer must still get a final non-live scene, or it keeps polling every live stand-in at the
/// refresh rate forever (CPU and GPU spent on an idle desktop).
#[test]
fn a_swipe_that_hops_monitors_still_ends_with_a_non_live_scene() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    focus(&mut h, 301);
    assert_eq!(h.engine.active, "MON2");
    let first = h.desktop.0.lock().unwrap().scenes.len();
    // Long sideways swipe on the portrait monitor: hops to MON0 once, the rest is ignored, then lift.
    swipe(&mut h, 3, (900., 200.), (0., 200.), 40);
    settle_now(&mut h);
    h.settle();
    assert_eq!(h.engine.active, "MON0", "the swipe hopped");
    let g = h.desktop.0.lock().unwrap();
    let scenes = &g.scenes[first..];
    assert!(
        scenes.iter().any(|s| s.live),
        "the gesture sent live scenes: {}",
        scenes.len()
    );
    let last = scenes.last().unwrap();
    assert!(
        !last.live,
        "after the lift the last scene must be non-live (epoch {}, action {}), or live stand-ins poll forever",
        last.epoch, last.action
    );
    drop(g);
    assert!(
        !h.engine.last_scene_live,
        "the engine knows the renderer left the live state"
    );
    h.stop_clean();
}

/// #108 re-review N1: an ordinary slow lift queues a settle, and that settle sends the gesture's final scene.
/// The lift must not send an extra non-live scene first: it would put the real windows at the mid-scroll
/// position (`live` off, the scene finished) only for the snap to move them again a moment later.
#[test]
fn an_ordinary_slow_lift_sends_no_scene_before_its_settle() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    focus(&mut h, 101);
    assert_eq!(h.engine.active, "MON0");
    // Move along the strip, then rest the fingers long enough that no recent motion is left (no fling).
    let mut x = 900.0;
    for _ in 0..12 {
        x -= 15.0;
        h.engine.event(Event::Contacts(vec![(x, 300.0); 3]));
        h.settle();
    }
    thread::sleep(Duration::from_millis(130));
    for _ in 0..3 {
        h.engine.event(Event::Contacts(vec![(x, 300.0); 3]));
        h.settle();
    }
    let before_lift = h.desktop.0.lock().unwrap().scenes.len();
    assert!(
        h.desktop.0.lock().unwrap().scenes.last().unwrap().live,
        "the resting gesture is live"
    );
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    h.tick_now();
    h.settle();
    assert!(
        h.engine.settle.is_some() && h.engine.inertia.is_none(),
        "a slow lift queues a settle, no momentum"
    );
    {
        let g = h.desktop.0.lock().unwrap();
        let extra: Vec<_> = g.scenes[before_lift..]
            .iter()
            .map(|s| (s.epoch, s.action, s.live))
            .collect();
        assert!(
            extra.is_empty(),
            "no scene between the lift and its settle, got {extra:?}"
        );
    }
    settle_now(&mut h);
    h.settle();
    let g = h.desktop.0.lock().unwrap();
    let after: Vec<_> = g.scenes[before_lift..]
        .iter()
        .map(|s| (s.action, s.live))
        .collect();
    assert!(
        !after.is_empty() && after.iter().all(|s| !s.1),
        "the settle ends the gesture non-live: {after:?}"
    );
    drop(g);
    assert!(
        !h.engine.last_scene_live,
        "the renderer left the live state"
    );
    h.stop_clean();
}

/// #108 re-review F1: the scenario is reachable. Between a slow lift and its queued settle the daemon sends a
/// holes change as an `OverlayUpdate` on the live scene's epoch (no new scene, the settle is still pending), so
/// the renderer must re-arm that patch without leaving the live state (`render::overlay_patched_settled_scene`).
#[test]
fn a_holes_change_between_a_slow_lift_and_its_settle_is_an_overlay_patch_on_the_live_scene() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
        three_per_monitor(sim);
        let mut pip = SimWindow::new(
            930,
            Rect {
                x: 1400,
                y: 700,
                w: 400,
                h: 225,
            },
        );
        pip.class = "Chrome_WidgetWin_1".into();
        pip.topmost = true;
        pip.toolwindow = true;
        sim.add_window(pip);
    });
    focus(&mut h, 101);
    let mut x = 900.0;
    for _ in 0..12 {
        x -= 15.0;
        h.engine.event(Event::Contacts(vec![(x, 300.0); 3]));
        h.settle();
    }
    thread::sleep(Duration::from_millis(130));
    for _ in 0..3 {
        h.engine.event(Event::Contacts(vec![(x, 300.0); 3]));
        h.settle();
    }
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert!(
        h.engine.settle.is_some() && h.engine.last_scene_live,
        "lifted, settle queued, renderer still live"
    );
    let (scenes, ov) = {
        let g = h.desktop.0.lock().unwrap();
        (g.scenes.len(), g.overlay_updates)
    };
    let seq = h.engine.seq;
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&930)
        .unwrap()
        .rect = Rect {
        x: 1300,
        y: 600,
        w: 400,
        h: 225,
    };
    h.engine.holes_stale = true;
    h.engine.dirty = true;
    h.settle_quiet();
    let g = h.desktop.0.lock().unwrap();
    assert_eq!(
        g.overlay_updates,
        ov + 1,
        "the holes change goes out as an overlay patch"
    );
    assert_eq!(
        (g.scenes.len(), h.engine.seq),
        (scenes, seq),
        "no new scene before the settle (N1)"
    );
    assert!(
        g.scenes.last().unwrap().live,
        "the patch lands on the live scene's epoch"
    );
    drop(g);
    assert!(h.engine.settle.is_some(), "the settle is still pending");
    settle_now(&mut h);
    h.settle();
    assert!(
        !h.desktop.0.lock().unwrap().scenes.last().unwrap().live,
        "the settle ends the live state"
    );
    h.stop_clean();
}
#[test]
fn n023_win_wheel_scrolls_the_strip_under_the_cursor_on_both_orientations() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let (land, port) = ((1, "MON0".to_string()), (1, "MON2".to_string()));
    let wheel = |h: &mut SimHarness, delta: i32, tilt: bool, shift: bool, x: i32, y: i32| {
        h.engine.event(Event::Wheel {
            delta,
            tilt,
            shift,
            x,
            y,
        });
        h.settle();
    };
    // (Win+Shift+wheel rolls one whole member per notch, so the deck never rests half rolled.)
    to_start(&mut h, "MON2");
    to_start(&mut h, "MON0");
    // Landscape: wheel down scrolls the strip on (sideways).
    let before = h.engine.strips[&land].offset;
    wheel(&mut h, -120, false, false, 900, 500);
    assert!(
        h.engine.strips[&land].offset > before,
        "landscape wheel down scrolls on"
    );
    // Portrait: the cursor decides the monitor; wheel down scrolls the vertical strip on.
    let before = h.engine.strips[&port].offset;
    wheel(&mut h, -120, false, false, -700, 1200);
    assert_eq!(
        h.engine.active, "MON2",
        "the strip under the cursor is the one that scrolls"
    );
    assert!(
        h.engine.strips[&port].offset > before,
        "portrait wheel down scrolls the strip down"
    );
    wheel(&mut h, 240, false, false, -700, 1200);
    assert!(
        h.engine.strips[&port].offset <= before,
        "wheel up scrolls back"
    );
    settle_now(&mut h);
    let s = &h.engine.strips[&port];
    assert_eq!(
        h.desktop.foreground(),
        s.active().unwrap(),
        "focus settles on the window the wheel brought in"
    );
    // Win + wheel rolls the focused group (#35); Win + Shift + wheel scrolls the strip instead.
    focus(&mut h, 101);
    h.cmd(Command::Key {
        name: "stack_toggle".into(),
    });
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
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, id));
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
fn n012_mouse_drop_tiles_beside_past_three_quarters_and_stacks_only_with_the_modifier() {
    for (mon, ids) in [("MON0", [101, 102, 103]), ("MON2", [301, 302, 303])] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        h.engine.hot.drag_dwell_ms = 0;
        let key = (1, mon.to_string());
        focus(&mut h, ids[0]);
        let visible: Vec<isize> = ids
            .iter()
            .copied()
            .filter(|&i| h.engine.strips[&key].in_view(i))
            .collect();
        assert!(
            visible.len() >= 2,
            "{mon}: need two visible windows, got {visible:?}"
        );
        let (a, b) = (visible[0], visible[1]);
        let order = |h: &SimHarness| {
            h.engine.strips[&key]
                .columns
                .iter()
                .map(|c| c.members[0])
                .collect::<Vec<_>>()
        };
        let before = order(&h);
        // Past 3/4 of b from its leading edge: it tiles after b, no drop zone, no group.
        let rb = h.rect(b);
        mouse_drag(&mut h, a, (rb.x + rb.w * 19 / 25, rb.y + rb.h * 19 / 25));
        assert!(
            matches!(
                h.engine.drag.as_ref().map(|d| d.phase),
                Some(crate::daemon::DragPhase::PastThreeQuarters { target, after: true }) if target == b
            ),
            "{mon}: past 3/4 targets the far side of b"
        );
        assert_eq!(h.rect(b), rb, "{mon}: the target does not dodge");
        drop_now(&mut h, a);
        let after = order(&h);
        let (ia, ib) = (
            before.iter().position(|&x| x == a).unwrap(),
            before.iter().position(|&x| x == b).unwrap(),
        );
        assert!(
            after.iter().position(|&x| x == a).unwrap()
                > after.iter().position(|&x| x == b).unwrap()
                && ia < ib,
            "{mon}: a moved after b: {before:?} -> {after:?}"
        );
        assert!(zones(&h).is_empty(), "{mon}: cue gone after the drop");
        // Middle of b without the modifier: tiles on the midpoint side, never stacks.
        let ra = h.rect(a);
        let rb = h.rect(b);
        mouse_drag(&mut h, b, (ra.x + ra.w / 2, ra.y + ra.h / 2));
        drop_now(&mut h, b);
        let s = &h.engine.strips[&key];
        let (i, _) = s.locate(b).unwrap();
        assert!(
            !s.columns[i].members.contains(&a),
            "{mon}: no modifier -> no stack"
        );
        let _ = rb;
        // Middle of the neighbour with the modifier held: stack.
        let ra = h.rect(a);
        let rb = h.rect(b);
        let (mover, target) = if ra.x + ra.y <= rb.x + rb.y {
            (b, a)
        } else {
            (a, b)
        };
        let rt = h.rect(target);
        h.desktop.0.lock().unwrap().keys_down.insert(0x10);
        mouse_drag(&mut h, mover, (rt.x + rt.w / 2, rt.y + rt.h / 2));
        h.engine.tick().unwrap();
        // A key held since before the drag does not count (edge semantics): release, then press afresh.
        h.desktop.0.lock().unwrap().keys_down.remove(&0x10);
        h.engine.tick().unwrap();
        h.desktop.0.lock().unwrap().keys_down.insert(0x10);
        h.engine.tick().unwrap();
        h.engine.tick().unwrap();
        assert!(
            matches!(
                h.engine.drag.as_ref().map(|d| d.phase),
                Some(crate::daemon::DragPhase::MiddleGroup { .. })
            ),
            "{mon}: modifier after dwell activates group mode: {:?}",
            h.engine.drag.as_ref().map(|d| d.phase)
        );
        drop_now(&mut h, mover);
        h.desktop.0.lock().unwrap().keys_down.remove(&0x10);
        let s = &h.engine.strips[&key];
        let (i, _) = s.locate(mover).unwrap();
        assert!(
            s.columns[i].members.contains(&target),
            "{mon}: mover joined the target's group"
        );
        h.stop_clean();
    }
}

#[test]
fn n018_dropping_a_group_member_on_its_sibling_reorders_the_group_on_both_orientations() {
    for (mon, ids) in [("MON0", [101, 102, 103]), ("MON2", [301, 302, 303])] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        let key = (1, mon.to_string());
        focus(&mut h, ids[1]);
        h.cmd(Command::Key {
            name: "stack_toggle".into(),
        });
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
        let (x1, y1) = (
            (r.x + r.w).min(area.x + area.w),
            (r.y + r.h).min(area.y + area.h),
        );
        assert!(x1 > x0 && y1 > y0, "{mon}: sibling peek visible: {r:?}");
        mouse_drag(&mut h, active, ((x0 + x1) / 2, (y0 + y1) / 2));
        assert_eq!(
            h.engine.drag.as_ref().map(|d| d.mode),
            Some(planor::Drag::Swap),
            "{mon}: onto the sibling = reorder"
        );
        drop_now(&mut h, active);
        let s = &h.engine.strips[&key];
        let (i, _) = s.locate(active).unwrap();
        let now = s.columns[i].members.clone();
        assert_eq!(
            now.len(),
            2,
            "{mon}: still one pair (it used to pull the window out)"
        );
        assert_eq!(
            now,
            col.iter().rev().copied().collect::<Vec<_>>(),
            "{mon}: order swapped"
        );
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
    assert_eq!(
        h.engine.drag.as_ref().and_then(|d| d.destination.clone()),
        Some(port.clone()),
        "the drag follows the cursor onto the portrait monitor"
    );
    assert!(
        zones(&h).iter().all(|c| c.monitor == "MON2"),
        "any cue is on MON2"
    );
    assert!(
        matches!(
            h.engine.drag.as_ref().map(|d| d.phase),
            Some(crate::daemon::DragPhase::BeforeQuarter { .. })
        ),
        "from another monitor: still before 1/4, no swap, no stack"
    );
    drop_now(&mut h, 101);
    assert_eq!(
        h.engine.strips[&port].locate(101),
        Some((0, 0)),
        "101 went in before the target on the portrait monitor"
    );
    assert!(
        h.engine.strips[&port].locate(target).is_some(),
        "the target stays on the portrait monitor"
    );
    assert!(
        h.engine.strips[&(1, "MON0".to_string())]
            .locate(101)
            .is_none()
    );
    h.stop_clean();
}

#[test]
fn n020_a_new_window_that_moves_itself_is_put_back() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let id = 150;
    h.desktop.0.lock().unwrap().add_window(SimWindow::new(
        id,
        Rect {
            x: 300,
            y: 300,
            w: 500,
            h: 400,
        },
    ));
    h.engine.event(Event::Window(EVENT_OBJECT_SHOW, id));
    h.settle();
    let slot = h.rect(id);
    // The app restores its own saved bounds a moment after showing.
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&id)
        .unwrap()
        .rect = Rect {
        x: 40,
        y: 60,
        w: 700,
        h: 500,
    };
    h.engine
        .event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, id));
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
        (
            f.iter().map(|p| p.0).sum::<f64>() / n,
            f.iter().map(|p| p.1).sum::<f64>() / n,
        )
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
/// What `Gesture::sample` sees at a four-finger start: the centroid offset (along, cross) from the first
/// four-finger frame at the first frame past the dead zone. `None` when fewer fingers came first (the
/// recogniser may re-base) or the gesture never leaves the dead zone.
fn start_offset(
    frames: &[Vec<(f64, f64)>],
    horizontal_strip: bool,
    deadzone: f64,
) -> Option<(f64, f64)> {
    let mut first = frames.iter().position(|f| f.len() == 4)?;
    let c = |f: &Vec<(f64, f64)>| {
        let n = f.len().max(1) as f64;
        (
            f.iter().map(|p| p.0).sum::<f64>() / n,
            f.iter().map(|p| p.1).sum::<f64>() / n,
        )
    };
    let threes: Vec<_> = frames[..first].iter().filter(|f| f.len() == 3).collect();
    if let (Some(a), Some(b)) = (threes.first(), threes.last()) {
        let (a, b) = (c(a), c(b));
        if (b.0 - a.0).hypot(b.1 - a.1) >= 2. * deadzone {
            return None; // a real three-finger scroll: the fourth finger does not turn it into a drag
        }
        // Three fingers that barely moved become the drag. Before the scroll axis was picked (under 3 units)
        // the recogniser starts over on the first four-finger frame; after it, at the next frame.
        if (b.0 - a.0).hypot(b.1 - a.1) >= 3. {
            first += 1;
            if first >= frames.len() || frames[first].len() != 4 {
                return None;
            }
        }
    }
    // Only the FIRST four-finger segment decides pick-up (#84): once the dead zone was crossed, 3<->4 finger
    // changes keep the recogniser's verdict (drag or rejected start) until the hand drops below three
    // fingers. A count change BEFORE the crossing restarts the recogniser (start unknown, `None`), and so
    // does a later four-finger touch after the hand dropped below three (a second gesture).
    let o = c(&frames[first]);
    let mut crossed = None;
    for (i, f) in frames.iter().enumerate().skip(first + 1) {
        if f.len() != 4 {
            if crossed.is_none() {
                return None;
            }
            if f.len() < 3 {
                // The first segment is over: a later four-finger touch is a different gesture.
                return if frames[i..].iter().any(|g| g.len() == 4) {
                    None
                } else {
                    crossed
                };
            }
            continue;
        }
        if crossed.is_none() {
            let p = c(f);
            let (dx, dy) = (p.0 - o.0, p.1 - o.1);
            if dx.hypot(dy) >= deadzone {
                crossed = Some(if horizontal_strip { (dx, dy) } else { (dy, dx) });
            }
        }
    }
    crossed
}

/// Four-finger recordings whose pick-up could not be checked (start unknown); reported and capped.
static SKIPPED_PICKUP_CHECKS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Lock releases seen while replaying recordings (the lock lets go once the fingers push towards a monitor).
static LOCK_RELEASES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// What a replay observed frame by frame.
#[derive(Default)]
struct Observed {
    moved: bool,
    rolled: bool,
    dragged: bool,
    lock_released: bool,
}

/// Put the strip into a state where a three-finger gesture can do something visible.
fn prepare_three_finger_replay(h: &mut SimHarness, key: &(u128, String), mon: &str, along: bool) {
    if !along {
        // Across the strip: give it a group of three to roll (a pair stops at its ends, N-010, so a swipe
        // towards the end it already shows would rightly do nothing).
        h.cmd(Command::Key {
            name: "stack_toggle".into(),
        });
        h.settle();
        let rest = h.engine.strips[key].columns.get(1).map(|c| c.members[0]);
        if let Some(third) = rest {
            focus(h, third);
            h.cmd(Command::Key {
                name: "stack_toggle".into(),
            });
            h.settle();
        }
        assert!(
            h.engine.strips[key]
                .columns
                .iter()
                .any(|c| c.members.len() >= 3),
            "setup: a group of three on {mon}"
        );
    } else {
        // Along the strip: start in the middle so either direction can scroll.
        let s = h.engine.strips.get_mut(key).unwrap();
        s.offset = i32::MAX / 4;
        s.scroll(0);
        s.offset /= 2;
        h.engine.dirty = true;
        h.settle();
    }
}

/// Whether the carried window's cursor is over the visible rect of its own group column.
fn cursor_in_own_group(h: &SimHarness, key: &(u128, String), p: &crate::daemon::PadDrag) -> bool {
    let Some(st) = h.engine.strips.get(key) else {
        return false;
    };
    let Some((ci, _)) = st
        .locate(p.id)
        .filter(|(ci, _)| st.columns[*ci].members.len() > 1)
    else {
        return false;
    };
    let own = &st.columns[ci].members;
    let col = st
        .rects(28)
        .into_iter()
        .filter(|(id, _)| own.contains(id))
        .filter_map(|(_, r)| r.intersect(st.area))
        .reduce(|a, b| a.union(b));
    col.is_some_and(|r| {
        p.cursor.0 >= r.x as f64
            && p.cursor.0 < (r.x + r.w) as f64
            && p.cursor.1 >= r.y as f64
            && p.cursor.1 < (r.y + r.h) as f64
    })
}

/// While the lock holds before and after a frame, that frame must not move the cursor further from the
/// pick-up line across the strip axis. Updates `carry_cross` (the cross coordinate after the frame).
fn check_carry_on_axis(
    h: &SimHarness,
    key: &(u128, String),
    mon: &str,
    horizontal_strip: bool,
    before: Option<bool>,
    carry_cross: &mut Option<f64>,
) {
    let Some(p) = h.engine.pad.as_ref() else {
        *carry_cross = None;
        return;
    };
    // The lock follows the axis of the strip under the cursor (#84): that of the home strip until the carry
    // reaches a monitor of the other orientation, where the lock switches (and its line is re-based).
    let axis_h = p.lock.unwrap_or_else(|| {
        p.home
            .as_ref()
            .and_then(|k| h.engine.strips.get(k))
            .map_or(horizontal_strip, |st| st.area.horizontal())
    });
    let cur = if axis_h { p.cursor.1 } else { p.cursor.0 };
    // Over the window's own group column the lock is off by design (members are reordered across the axis).
    let in_group = cursor_in_own_group(h, key, p);
    // (A grab point outside the monitors is clamped into them by the first move; that is not the lock.)
    let (lo, hi) = if axis_h {
        (p.limits.1, p.limits.3)
    } else {
        (p.limits.0, p.limits.2)
    };
    let base = carry_cross.unwrap_or(cur);
    if before.is_some() && before == p.lock && !in_group && base >= lo && base <= hi {
        // Easing back onto the line (#84) is allowed; moving away from it across the strip axis is not.
        assert!(
            (cur - p.line).abs() <= (base - p.line).abs() + 1e-6,
            "{mon}: carry left the strip axis ({base} -> {cur}, line {})",
            p.line
        );
    }
    *carry_cross = Some(cur);
}

/// Feed every frame and record what the layout and the pad drag did.
fn feed_replay_frames(
    h: &mut SimHarness,
    key: &(u128, String),
    mon: &str,
    horizontal_strip: bool,
    frames: &[Vec<(f64, f64)>],
) -> Observed {
    let start = h.engine.strips[key].clone();
    let mut o = Observed::default();
    let mut carry_cross: Option<f64> = None;
    for f in frames {
        let before = h.engine.pad.as_ref().and_then(|p| p.lock);
        h.engine.event(Event::Contacts(f.clone()));
        h.settle_quiet();
        if before.is_some() && h.engine.pad.as_ref().is_some_and(|p| p.lock.is_none()) {
            o.lock_released = true;
        }
        check_carry_on_axis(h, key, mon, horizontal_strip, before, &mut carry_cross);
        let s = &h.engine.strips[key];
        o.moved |= s.offset != start.offset;
        o.rolled |= s.columns.iter().any(|c| c.frac != 0.)
            || s.columns
                .iter()
                .zip(&start.columns)
                .any(|(a, b)| a.active != b.active);
        o.dragged |= h.engine.pad.is_some();
    }
    o
}

/// Verdict for a four-finger gesture: pick-up must follow the axis lock, and nothing may be left over.
fn judge_four_fingers(
    what: &str,
    mon: &str,
    frames: &[Vec<(f64, f64)>],
    offset: Option<(f64, f64)>,
    o: &Observed,
) -> String {
    // Drag axis lock (#83, on by default): a start with 1.5x more travel along the strip axis than
    // across it picks the window up; one with 1.5x more across is ignored.
    match offset {
        Some((along, cross)) if along.abs() >= 1.5 * cross.abs() => {
            assert!(
                o.dragged,
                "{what}: a start along the strip ({:.0} vs {:.0}) must pick the window up",
                along.abs(),
                cross.abs()
            );
        }
        // (MON0 lies right of the portrait MON2, so a rightward cross start there is a carry to it, #87.)
        Some((along, cross))
            if cross.abs() >= 1.5 * along.abs() && !(mon == "MON2" && cross > 0.) =>
        {
            assert!(
                !o.dragged,
                "{what}: a start across the strip ({:.0} vs {:.0}) must be ignored; counts {:?}",
                along.abs(),
                cross.abs(),
                frames
                    .iter()
                    .take(12)
                    .map(|f| (
                        f.len(),
                        f.iter().map(|p| p.0).sum::<f64>() as i64 / f.len().max(1) as i64,
                        f.iter().map(|p| p.1).sum::<f64>() as i64 / f.len().max(1) as i64
                    ))
                    .collect::<Vec<_>>()
            );
        }
        Some(_) => {}
        None => {
            SKIPPED_PICKUP_CHECKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    if o.lock_released {
        LOCK_RELEASES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    if mon == "MON0" {
        // Nothing lies above or below MON0, so the lock must never release there (#84).
        assert!(
            !o.lock_released,
            "{what}: the lock released with no monitor across the axis"
        );
    }
    if o.lock_released {
        format!("{what}: carried (lock released towards a monitor) and dropped the window")
    } else if o.dragged {
        format!("{what}: carried and dropped the window")
    } else {
        format!("{what}: cross-axis start ignored (axis lock)")
    }
}

fn replay_on(frames: &[Vec<(f64, f64)>], mon: &str) -> String {
    let (fingers, dx, dy) = describe_gesture(frames);
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let key = (1, mon.to_string());
    let horizontal_strip = h.engine.strips[&key].area.horizontal();
    let deadzone = h.engine.cfg["touchpad"]["axis_lock_deadzone"]
        .as_integer()
        .unwrap_or(12) as f64;
    let travel = dx.hypot(dy);
    let along = (dx.abs() >= dy.abs()) == horizontal_strip;
    let first = h.engine.strips[&key].columns[0].members[0];
    focus(&mut h, first);
    if fingers == 3 {
        prepare_three_finger_replay(&mut h, &key, mon, along);
    }
    let start = h.engine.strips[&key].clone();
    let offset = if fingers == 4 {
        start_offset(frames, horizontal_strip, deadzone)
    } else {
        None
    };
    let o = feed_replay_frames(&mut h, &key, mon, horizontal_strip, frames);
    h.engine.event(Event::Contacts(vec![]));
    h.settle_quiet();
    settle_now(&mut h);
    h.settle(); // invariants and os_rect == expected_rect after the gesture
    let what = format!("{fingers} fingers, travel ({dx:.0}, {dy:.0}) on {mon}");
    assert!(
        h.engine.pad.is_none() && h.engine.drag.is_none() && !h.engine.pad_scrolling,
        "{what}: gesture left state behind"
    );
    if fingers < 3 {
        // Cursor moves, taps, clicks, two-finger scroll and pinch: Planor must stay out of it.
        assert!(
            !o.moved && !o.dragged && !o.rolled,
            "{what}: Planor reacted to a {fingers}-finger gesture"
        );
        return format!("{what}: ignored, as it should be");
    }
    if fingers >= 5 {
        // The whole hand lands and lifts in stages, so the first fingers may briefly start something; the
        // fifth cancels it. What counts is that the layout ends exactly as it was.
        let end = &h.engine.strips[&key];
        let order = |s: &planor::Strip| {
            s.columns
                .iter()
                .map(|c| (c.members.clone(), c.active))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            order(end),
            order(&start),
            "{what}: a five-finger gesture changed the layout"
        );
        return format!("{what}: vetoed, layout unchanged");
    }
    if travel < deadzone * 2. {
        return format!("{what}: too short to mean anything, only checked it leaves no state");
    }
    match fingers {
        3 if along => {
            assert!(o.moved, "{what}: fingers along the strip must scroll it");
            format!("{what}: scrolled the strip")
        }
        3 => {
            assert!(
                o.rolled,
                "{what}: fingers across the strip must roll the group"
            );
            assert_eq!(
                h.engine.strips[&key].offset, start.offset,
                "{what}: rolling must not scroll the strip"
            );
            format!("{what}: rolled the group")
        }
        4 => {
            assert!(
                !o.moved || !h.engine.pad_scrolling,
                "{what}: no stray scroll after the drop"
            );
            judge_four_fingers(&what, mon, frames, offset, &o)
        }
        _ => format!("{what}: no gesture for this many fingers"),
    }
}

#[test]
fn recorded_gestures_replay_on_landscape_and_portrait() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gestures/recorded");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    assert!(!files.is_empty(), "no recordings in {}", dir.display());
    let mut report = vec![];
    let mut carried = [0usize; 2];
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
            // MON2 is also replayed with x and y swapped, so a recording along the landscape strip is along the
            // portrait strip too (and the lock's carry is checked on both orientations).
            let swapped: Vec<Vec<(f64, f64)>> = g
                .iter()
                .map(|f| f.iter().map(|p| (p.1, p.0)).collect())
                .collect();
            for (mon, frames) in [("MON0", g), ("MON2", g), ("MON2", &swapped)] {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    replay_on(frames, mon)
                }));
                match r {
                    Ok(line) => {
                        if line.contains("carried") && line.contains("dropped the window") {
                            carried[usize::from(mon == "MON2")] += 1;
                        }
                        report.push(format!(
                            "{} #{n} [{label}]: {line}",
                            f.file_name().unwrap().to_string_lossy()
                        ));
                    }
                    Err(e) => {
                        let msg = e
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_default();
                        panic!(
                            "{} gesture #{n} [{label}] on {mon}: {msg}\nso far:\n{}",
                            f.display(),
                            report.join("\n")
                        );
                    }
                }
            }
        }
    }
    assert!(
        carried[0] >= 5 && carried[1] >= 5,
        "too few carried drags (landscape, portrait): {carried:?}"
    );
    let skipped = SKIPPED_PICKUP_CHECKS.load(std::sync::atomic::Ordering::Relaxed);
    let releases = LOCK_RELEASES.load(std::sync::atomic::Ordering::Relaxed);
    const MAX_SKIPPED_PICKUP_CHECKS: usize = 40;
    report.push(format!("pick-up unchecked (start unknown) for {skipped}/{MAX_SKIPPED_PICKUP_CHECKS} four-finger replays; {releases} lock releases (none on MON0, asserted per replay)"));
    eprintln!(
        "recorded_gestures_replay: skipped_pickup_checks={skipped}/{MAX_SKIPPED_PICKUP_CHECKS} lock_releases={releases} carried={carried:?}"
    );
    // Skipped checks are capped so a parser change cannot silently stop verifying pick-up.
    assert!(
        skipped <= MAX_SKIPPED_PICKUP_CHECKS,
        "too many four-finger replays with an unchecked pick-up: {skipped}"
    );
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
        h.engine.event(Event::Wheel {
            delta,
            tilt: false,
            shift: false,
            x: 900,
            y: 500,
        });
        h.settle();
    }
    assert_eq!(
        h.engine.strips[&land].offset, 0,
        "touchpad scroll steps are not Win+wheel"
    );
    let step = h.engine.cfg["mouse"]["win_wheel_step_px"]
        .as_integer()
        .unwrap() as i32;
    h.engine.event(Event::Wheel {
        delta: -120,
        tilt: false,
        shift: false,
        x: 900,
        y: 500,
    });
    h.settle();
    let one = h.engine.strips[&land].offset;
    assert!(
        one > 0 && one <= step,
        "one mouse notch scrolls one step: {one}"
    );
    h.engine.event(Event::Wheel {
        delta: -1200,
        tilt: false,
        shift: false,
        x: 900,
        y: 500,
    });
    h.settle();
    let more = h.engine.strips[&land].offset - one;
    assert!(
        more > 0 && more <= 2 * step,
        "a fast spin (10 notches in one event) scrolls at most two: {more}"
    );
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
        assert!(
            !h.engine.dirty,
            "{mon}: sub-deadzone contact does not dirty layout"
        );
        assert!(
            h.engine.input_qpc_us.is_none(),
            "{mon}: non-dirtying 3-finger contact must not leave stale input_qpc_us"
        );
        h.engine.event(Event::Contacts(vec![]));
        h.settle_quiet();

        // Start a 4-finger pad drag, wait past the 8 ms move gate, then nudge without changing target/mode.
        // (Along the strip axis: sideways on landscape, up and down on portrait; drag axis lock is on.)
        let (nudge, tiny) = if mon == "MON2" {
            ((200., 216.), (200., 216.2))
        } else {
            ((216., 200.), (216.2, 200.))
        };
        h.engine.event(Event::Contacts(vec![(200., 200.); 4]));
        h.engine.event(Event::Contacts(vec![nudge; 4]));
        h.settle_quiet();
        assert!(h.engine.pad.is_some(), "{mon}: 4-finger drag started");
        thread::sleep(Duration::from_millis(12));
        h.engine.event(Event::Contacts(vec![tiny; 4]));
        if !h.engine.dirty {
            assert!(
                h.engine.input_qpc_us.is_none(),
                "{mon}: pad_move without scene must not leave stale input_qpc_us"
            );
        }
        h.engine.event(Event::Contacts(vec![]));
        h.settle_quiet();
        settle_now(&mut h);
        h.stop_clean();
    }
}

/// #67: an unmanaged occluder moving (EVENT_OBJECT_LOCATIONCHANGE) refreshes the overlay holes on the next tick,
/// not after the 200 ms poll; a managed window's location change does not force a holes re-read.
#[test]
fn unmanaged_window_move_refreshes_holes_on_next_tick() {
    let hole_rect = Rect {
        x: 200,
        y: 200,
        w: 300,
        h: 300,
    };
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
        three_per_monitor(sim);
        let mut ext = SimWindow::new(901, hole_rect);
        ext.elevated = true;
        sim.add_window(ext);
    });
    focus(&mut h, 101);
    let holes = |h: &SimHarness| {
        h.desktop
            .0
            .lock()
            .unwrap()
            .scenes
            .last()
            .unwrap()
            .holes
            .clone()
    };
    assert!(
        holes(&h).contains(&hole_rect),
        "initial scene includes hole {hole_rect:?}"
    );

    // A managed window moving does not mark holes stale.
    h.engine
        .event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, 101));
    assert!(
        !h.engine.holes_stale,
        "a managed window's move must not force a holes re-read"
    );

    // The unmanaged window moves right after a holes poll: without the event the cache would hold for 200 ms.
    let moved = Rect {
        x: 260,
        y: 220,
        w: 360,
        h: 300,
    };
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&901)
        .unwrap()
        .rect = moved;
    h.engine.holes_checked = Instant::now();
    h.engine
        .event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, 901));
    assert!(
        h.engine.holes_stale,
        "an unmanaged occluder's move marks holes stale"
    );
    h.engine.tick().unwrap();
    h.settle();
    let now = holes(&h);
    assert!(
        now.contains(&moved),
        "next tick must carve the moved occluder {moved:?}: {now:?}"
    );
    assert!(
        !now.contains(&hole_rect),
        "stale hole {hole_rect:?} must be gone: {now:?}"
    );
    assert!(!h.engine.holes_stale);
    h.stop_clean();
}
#[test]
fn live_scroll_caches_holes_during_continuous_motion_on_both_orientations() {
    for (mon, id, hole_id, hole_rect, p0, p1, p2) in [
        (
            "MON0",
            101,
            901,
            Rect {
                x: 200,
                y: 200,
                w: 300,
                h: 300,
            },
            (400., 200.),
            (360., 200.),
            (320., 200.),
        ),
        (
            "MON2",
            301,
            902,
            Rect {
                x: -1200,
                y: 200,
                w: 300,
                h: 300,
            },
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
        let initial_holes = h
            .desktop
            .0
            .lock()
            .unwrap()
            .scenes
            .last()
            .unwrap()
            .holes
            .clone();
        assert!(
            initial_holes.contains(&hole_rect),
            "{mon}: initial scene includes hole {hole_rect:?}"
        );

        // Enter live scroll along the monitor's strip axis.
        h.engine.event(Event::Contacts(vec![p0; 3]));
        h.settle_quiet();
        h.engine.event(Event::Contacts(vec![p1; 3]));
        h.settle_quiet();
        assert!(h.engine.pad_scrolling, "{mon}: should be live scrolling");

        // Move the unmanaged window right after holes were checked: live frames reuse cached holes.
        let mut moved = hole_rect;
        moved.w = 360;
        h.desktop
            .0
            .lock()
            .unwrap()
            .windows
            .get_mut(&hole_id)
            .unwrap()
            .rect = moved;
        h.engine.holes_checked = Instant::now();
        h.engine.event(Event::Contacts(vec![p2; 3]));
        h.settle_quiet();
        let cached = h
            .desktop
            .0
            .lock()
            .unwrap()
            .scenes
            .last()
            .unwrap()
            .holes
            .clone();
        assert!(
            cached.contains(&hole_rect),
            "{mon}: live scroll should reuse cached holes within 150 ms: {cached:?}"
        );

        // Settling (live = false) refreshes holes immediately to the moved rect.
        h.engine.event(Event::Contacts(vec![]));
        h.settle_quiet();
        settle_now(&mut h);
        let refreshed = h
            .desktop
            .0
            .lock()
            .unwrap()
            .scenes
            .last()
            .unwrap()
            .holes
            .clone();
        assert!(
            refreshed.contains(&moved),
            "{mon}: settled scene must refresh holes to {moved:?}: {refreshed:?}"
        );
        h.stop_clean();
    }
}

#[test]
fn newborn_window_gets_cover_while_scan_and_neighbours_do_not_on_both_orientations() {
    for (mon, cursor, new_rect) in [
        (
            "MON0",
            (400, 400),
            Rect {
                x: 200,
                y: 200,
                w: 600,
                h: 400,
            },
        ),
        (
            "MON2",
            (-700, 800),
            Rect {
                x: -1200,
                y: 500,
                w: 700,
                h: 500,
            },
        ),
    ] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        // Existing windows tiled via `scan()` never receive a newborn cover.
        assert!(
            h.desktop
                .0
                .lock()
                .unwrap()
                .scenes
                .iter()
                .all(|s| s.covered.is_empty()),
            "{mon}: scan must not mark covers"
        );

        h.desktop.0.lock().unwrap().cursor = cursor;
        let id = h
            .desktop
            .0
            .lock()
            .unwrap()
            .add_window(SimWindow::new(0, new_rect));
        h.engine.event(Event::Window(EVENT_OBJECT_SHOW, id));
        h.settle();

        let last_scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
        assert_eq!(
            last_scene.covered,
            vec![id],
            "{mon}: only newborn {id} covered, not neighbours"
        );

        // Hiding/destroying removes any cover state cleanly.
        h.desktop
            .0
            .lock()
            .unwrap()
            .windows
            .get_mut(&id)
            .unwrap()
            .visible = false;
        h.engine.event(Event::Window(EVENT_OBJECT_HIDE, id));
        h.settle();
        assert!(
            !h.engine.covering.contains_key(&id),
            "{mon}: hide clears cover state"
        );
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
        assert!(
            h.engine.strips[&(1, "MON2".into())].locate(101).is_some(),
            "101 transferred to MON2"
        );
        let covered = h
            .desktop
            .0
            .lock()
            .unwrap()
            .scenes
            .last()
            .unwrap()
            .covered
            .clone();
        assert!(
            covered.contains(&101),
            "landscape->portrait transfer must cover 101: {covered:?}"
        );
        assert!(
            !covered.contains(&301),
            "existing destination neighbour 301 must not be covered"
        );
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
        assert!(
            h.engine.strips[&(1, "MON0".into())].locate(301).is_some(),
            "301 transferred to MON0"
        );
        let covered = h
            .desktop
            .0
            .lock()
            .unwrap()
            .scenes
            .last()
            .unwrap()
            .covered
            .clone();
        assert!(
            covered.contains(&301),
            "portrait->landscape transfer must cover 301: {covered:?}"
        );
        assert!(
            !covered.contains(&101),
            "existing destination neighbour 101 must not be covered"
        );
        h.stop_clean();
    }
}

#[test]
fn placeholder_config_false_disables_cover_on_both_orientations() {
    for (mon, cursor, new_rect) in [
        (
            "MON0",
            (400, 400),
            Rect {
                x: 200,
                y: 200,
                w: 600,
                h: 400,
            },
        ),
        (
            "MON2",
            (-700, 800),
            Rect {
                x: -1200,
                y: 500,
                w: 700,
                h: 500,
            },
        ),
    ] {
        let mut cfg = note_cfg();
        cfg["animation"]["placeholder"] = toml::Value::Boolean(false);
        let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
        h.desktop.0.lock().unwrap().cursor = cursor;
        let id = h
            .desktop
            .0
            .lock()
            .unwrap()
            .add_window(SimWindow::new(0, new_rect));
        h.engine.event(Event::Window(EVENT_OBJECT_SHOW, id));
        h.settle();
        let covered = h
            .desktop
            .0
            .lock()
            .unwrap()
            .scenes
            .last()
            .unwrap()
            .covered
            .clone();
        assert!(
            covered.is_empty(),
            "{mon}: placeholder=false disables cover: {covered:?}"
        );
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
            h.engine.inertia = Some((
                vertical,
                -1200.0,
                Instant::now() - Duration::from_millis(8),
                Some(96.0),
            ));
            assert!(
                h.engine.wants_fast_tick(),
                "{mon}@{hz}Hz wants fast tick during inertia"
            );
            for _ in 0..4 {
                h.engine.fast_tick();
                h.settle_quiet();
                if let Some((v, s, _, d)) = h.engine.inertia {
                    h.engine.inertia = Some((
                        v,
                        s,
                        Instant::now() - Duration::from_millis(planor::pacing::fast_tick_ms(hz)),
                        d,
                    ));
                }
            }
            let after = h.engine.strips[&key].offset;
            assert_ne!(
                after, before,
                "{mon}@{hz}Hz: momentum fast_tick advanced strip offset (dx/dy {axis_delta:?})"
            );
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
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, id));
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
            mouse_resize(
                &mut h,
                a,
                Rect {
                    y: 40,
                    h: ra.h - 40,
                    ..ra
                },
                (ra.x + ra.w / 2, 40),
            );
            assert_eq!(
                h.rect(a),
                ra,
                "{mon}: top-edge cross-axis resize snaps back"
            );
            mouse_resize(&mut h, a, Rect { h: 1190, ..ra }, (ra.x + ra.w / 2, 1185));
            assert_eq!(h.rect(a), ra, "{mon}: bottom-edge into taskbar snaps back");
        } else {
            mouse_resize(
                &mut h,
                a,
                Rect {
                    x: ra.x + 80,
                    w: ra.w - 80,
                    ..ra
                },
                (ra.x + 80, ra.y + ra.h / 2),
            );
            assert_eq!(
                h.rect(a),
                ra,
                "{mon}: left-edge cross-axis resize snaps back"
            );
            mouse_resize(
                &mut h,
                a,
                Rect {
                    w: ra.w - 150,
                    ..ra
                },
                (ra.x + ra.w - 150, ra.y + ra.h / 2),
            );
            assert_eq!(
                h.rect(a),
                ra,
                "{mon}: right-edge cross-axis resize snaps back"
            );
        }
        assert_eq!(
            h.rect(b),
            rb,
            "{mon}: neighbour untouched after cross-axis snap-back"
        );

        // 2. Column 0 leading-edge drag (inward & outward) is ignored and snapped back.
        let (grow_lead, shrink_lead, lead_cursor) = if mon == "MON0" {
            (
                Rect {
                    x: ra.x - 120,
                    w: ra.w + 120,
                    ..ra
                },
                Rect {
                    x: ra.x + 120,
                    w: ra.w - 120,
                    ..ra
                },
                (ra.x + 120, ra.y + ra.h / 2),
            )
        } else {
            (
                Rect {
                    y: ra.y - 120,
                    h: ra.h + 120,
                    ..ra
                },
                Rect {
                    y: ra.y + 120,
                    h: ra.h - 120,
                    ..ra
                },
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
        let paused_try = if mon == "MON0" {
            Rect {
                w: ra.w + 100,
                ..ra
            }
        } else {
            Rect {
                h: ra.h + 100,
                ..ra
            }
        };
        mouse_resize(&mut h, a, paused_try, (ra.x + 200, ra.y + 200));
        h.cmd(Command::Resume);
        h.settle();
        assert_eq!(h.rect(a), ra, "{mon}: resize while paused is ignored");
        h.stop_clean();
    }

    // 4. Lone window shrink snaps back to full work area, and sub-min / pair squeeze clamp to min.
    for (mon, origin, single_w, single_h, pair_b_rect) in [
        (
            "MON0",
            (100, 100),
            600,
            400,
            Rect {
                x: 800,
                y: 100,
                w: 600,
                h: 400,
            },
        ),
        (
            "MON2",
            (-1300, 200),
            900,
            500,
            Rect {
                x: -1300,
                y: 1200,
                w: 900,
                h: 500,
            },
        ),
    ] {
        // Lone window shrink.
        let mut solo = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
            let mut w = SimWindow::new(
                501,
                Rect {
                    x: origin.0,
                    y: origin.1,
                    w: single_w,
                    h: single_h,
                },
            );
            w.min_w = 400;
            w.min_h = 400;
            sim.add_window(w);
            sim.foreground = 501;
        });
        let full = solo.rect(501);
        let shrunk = if mon == "MON0" {
            Rect {
                w: full.w - 400,
                ..full
            }
        } else {
            Rect {
                h: full.h - 400,
                ..full
            }
        };
        mouse_resize(&mut solo, 501, shrunk, (full.x + 300, full.y + 300));
        assert_eq!(
            solo.rect(501),
            full,
            "{mon}: lone window shrink snaps back to full monitor"
        );
        solo.stop_clean();

        // Pair sub-minimum shrink + squeezing neighbour below its minimum.
        let mut pair_h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
            for (wid, r) in [
                (
                    501,
                    Rect {
                        x: origin.0,
                        y: origin.1,
                        w: single_w,
                        h: single_h,
                    },
                ),
                (502, pair_b_rect),
            ] {
                let mut w = SimWindow::new(wid, r);
                w.min_w = 400;
                w.min_h = 400;
                sim.add_window(w);
            }
            sim.foreground = 501;
        });
        let r1 = pair_h.rect(501);
        let tiny = if mon == "MON0" {
            Rect { w: 50, ..r1 }
        } else {
            Rect { h: 50, ..r1 }
        };
        mouse_resize(&mut pair_h, 501, tiny, (r1.x + 200, r1.y + 200));
        let len1 = if mon == "MON0" {
            pair_h.rect(501).w
        } else {
            pair_h.rect(501).h
        };
        assert!(
            len1 >= 400,
            "{mon}: sub-minimum shrink clamped to >= 400, got {len1}"
        );

        // Squeeze neighbour 501 down to ~90 px by dragging 502's leading edge far toward origin.
        let r2 = pair_h.rect(502);
        let squeeze = if mon == "MON0" {
            let new_x = 100;
            Rect {
                x: new_x,
                w: (r2.x + r2.w) - new_x,
                ..r2
            }
        } else {
            let new_y = 100;
            Rect {
                y: new_y,
                h: (r2.y + r2.h) - new_y,
                ..r2
            }
        };
        mouse_resize(&mut pair_h, 502, squeeze, (r1.x + 200, r1.y + 200));
        let len1_after = if mon == "MON0" {
            pair_h.rect(501).w
        } else {
            pair_h.rect(501).h
        };
        assert!(
            len1_after >= 400,
            "{mon}: squeezed neighbour stayed >= 400 min, got {len1_after}"
        );
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
        let grow = if mon == "MON0" {
            Rect {
                w: ra.w + 120,
                ..ra
            }
        } else {
            Rect {
                h: ra.h + 120,
                ..ra
            }
        };
        mouse_resize(&mut h, a, grow, (ra.x + 300, ra.y + 300));
        assert_eq!(h.rect(a), grow, "{mon}: trailing-edge grow honoured");

        focus(&mut h, b);
        let rb = h.rect(b);
        let lead_shrink = if mon == "MON0" {
            Rect {
                x: rb.x + 80,
                w: rb.w - 80,
                ..rb
            }
        } else {
            Rect {
                y: rb.y + 80,
                h: rb.h - 80,
                ..rb
            }
        };
        mouse_resize(&mut h, b, lead_shrink, (rb.x + 200, rb.y + 200));
        let rb_now = h.rect(b);
        if mon == "MON0" {
            assert_eq!(rb_now.w, rb.w - 80);
            assert_eq!(
                rb_now.x + rb_now.w - rb.x - rb.w,
                0,
                "{mon}: leading resize keeps trailing X edge"
            );
        } else {
            assert_eq!(rb_now.h, rb.h - 80);
            assert_eq!(
                rb_now.y + rb_now.h - rb.y - rb.h,
                0,
                "{mon}: leading resize keeps trailing Y edge"
            );
        }

        // 2. Corner drags: col 1 top-left keeps strip change & snaps cross-axis; col 0 top-left snaps all back.
        let rb2 = h.rect(b);
        let corner_b = if mon == "MON0" {
            Rect {
                x: rb2.x - 60,
                y: 40,
                w: rb2.w + 60,
                h: rb2.h - 40,
            }
        } else {
            Rect {
                x: rb2.x + 90,
                y: rb2.y - 60,
                w: rb2.w - 90,
                h: rb2.h + 60,
            }
        };
        mouse_resize(&mut h, b, corner_b, (rb2.x + 200, rb2.y + 200));
        let rb2_after = h.rect(b);
        if mon == "MON0" {
            assert_eq!(
                (rb2_after.w, rb2_after.y, rb2_after.h),
                (rb2.w + 60, rb2.y, rb2.h)
            );
        } else {
            assert_eq!(
                (rb2_after.h, rb2_after.x, rb2_after.w),
                (rb2.h + 60, rb2.x, rb2.w)
            );
        }

        to_start(&mut h, mon);
        let ra0 = h.rect(a);
        let corner_a0 = Rect {
            x: ra0.x + 50,
            y: ra0.y + 50,
            w: ra0.w - 50,
            h: ra0.h - 50,
        };
        mouse_resize(&mut h, a, corner_a0, (ra0.x + 200, ra0.y + 200));
        assert_eq!(
            h.rect(a),
            ra0,
            "{mon}: col 0 top-left corner disallowed on both axes -> full snap back"
        );

        // 3. Edge drag with cursor crossing onto neighbouring monitor while window stays mostly on home monitor.
        let (cross_rect, cross_cursor) = if mon == "MON0" {
            (Rect { w: 1200, ..ra0 }, (2100, 500))
        } else {
            (Rect { h: 1400, ..ra0 }, (200, 500))
        };
        mouse_resize(&mut h, a, cross_rect, cross_cursor);
        assert!(
            h.engine.strips[&key].locate(a).is_some(),
            "{mon}: must stay on home monitor as a resize, not transfer"
        );
        if mon == "MON0" {
            assert_eq!(h.rect(a).w, 1200);
        } else {
            assert_eq!(h.rect(a).h, 1400);
        }

        // 4. Stacked/grouped column resize: cross-axis snaps back; strip-axis resizes all group members together.
        focus(&mut h, a);
        h.cmd(Command::Key {
            name: "stack_toggle".into(),
        });
        h.settle();
        let active_id = h.engine.strips[&key].active().unwrap();
        let col_idx = h.engine.strips[&key].locate(active_id).unwrap().0;
        let sibling_id = h.engine.strips[&key].columns[col_idx]
            .members
            .iter()
            .copied()
            .find(|&m| m != active_id)
            .unwrap();
        let r_stack = h.rect(active_id);
        let cross_stack = if mon == "MON0" {
            Rect {
                h: r_stack.h - 90,
                ..r_stack
            }
        } else {
            Rect {
                w: r_stack.w - 90,
                ..r_stack
            }
        };
        mouse_resize(
            &mut h,
            active_id,
            cross_stack,
            (r_stack.x + 200, r_stack.y + 200),
        );
        assert_eq!(
            h.rect(active_id),
            r_stack,
            "{mon}: stacked cross-axis resize snaps back"
        );

        let strip_stack = if mon == "MON0" {
            Rect {
                w: r_stack.w - 100,
                ..r_stack
            }
        } else {
            Rect {
                h: r_stack.h - 100,
                ..r_stack
            }
        };
        mouse_resize(
            &mut h,
            active_id,
            strip_stack,
            (r_stack.x + 200, r_stack.y + 200),
        );
        assert_eq!(
            h.engine.strips[&key].columns[col_idx].length,
            if mon == "MON0" {
                strip_stack.w
            } else {
                strip_stack.h
            },
            "{mon}: stacked column resizes along strip axis for all members {sibling_id}"
        );

        // 5. Pinned window free resize persists without snapping back or disturbing the strip.
        focus(&mut h, c);
        h.cmd(Command::Key {
            name: "toggle_pin".into(),
        });
        h.settle();
        assert!(h.engine.pinned.contains(&c));
        let pinned_rect = if mon == "MON0" {
            Rect {
                x: 220,
                y: 220,
                w: 520,
                h: 340,
            }
        } else {
            Rect {
                x: -1200,
                y: 300,
                w: 520,
                h: 340,
            }
        };
        mouse_resize(
            &mut h,
            c,
            pinned_rect,
            (pinned_rect.x + 100, pinned_rect.y + 100),
        );
        assert_eq!(
            h.rect(c),
            pinned_rect,
            "{mon}: pinned window keeps free resize on both axes"
        );
        h.stop_clean();
    }
}

#[test]
fn sim_app_rejected_resize_enforces_minimum_on_landscape_and_portrait() {
    // 1. Landscape MON0: app rejects width < 1100 px after user shrinks column.
    {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
            let mut w1 = SimWindow::new(
                101,
                Rect {
                    x: 100,
                    y: 100,
                    w: 600,
                    h: 400,
                },
            );
            w1.min_enforced_w = 1100;
            sim.add_window(w1);
            sim.add_window(SimWindow::new(
                102,
                Rect {
                    x: 800,
                    y: 100,
                    w: 600,
                    h: 400,
                },
            ));
            sim.foreground = 101;
        });
        // Settle initial scan already triggered rejection >= 1100; grow first, then user drags inward to 500.
        let r = h.rect(101);
        assert!(r.w >= 1100);
        mouse_resize(&mut h, 101, Rect { w: 500, ..r }, (500, 400));
        assert!(
            h.rect(101).w >= 1100,
            "landscape app rejection restores >= 1100 px width: {:?}",
            h.rect(101)
        );
        h.stop_clean();
    }
    // 2. Portrait MON2: app rejects height < 1300 px after user shrinks column.
    {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
            let mut w1 = SimWindow::new(
                301,
                Rect {
                    x: -1300,
                    y: 100,
                    w: 900,
                    h: 500,
                },
            );
            w1.min_enforced_h = 1300;
            sim.add_window(w1);
            sim.add_window(SimWindow::new(
                302,
                Rect {
                    x: -1300,
                    y: 900,
                    w: 900,
                    h: 500,
                },
            ));
            sim.foreground = 301;
        });
        let r = h.rect(301);
        assert!(r.h >= 1300);
        mouse_resize(&mut h, 301, Rect { h: 500, ..r }, (-700, 500));
        assert!(
            h.rect(301).h >= 1300,
            "portrait app rejection restores >= 1300 px height: {:?}",
            h.rect(301)
        );
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
            Rect {
                x: 100,
                y: 100,
                w: 300,
                h: 300,
            },
            Rect {
                x: 180,
                y: 140,
                w: 340,
                h: 280,
            },
        ),
        (
            "MON2",
            301,
            302,
            902,
            Rect {
                x: -1200,
                y: 200,
                w: 300,
                h: 300,
            },
            Rect {
                x: -1100,
                y: 320,
                w: 320,
                h: 360,
            },
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
        assert_eq!(
            h.engine.hot.focused_color,
            Some(0x3B82F6),
            "{mon}: initial focused_color"
        );
        assert_eq!(h.engine.hot.gap_px, 6, "{mon}: initial gap_px");
        assert_eq!(
            h.engine.hot.stack_peek_px,
            Some(96),
            "{mon}: initial stack_peek_px"
        );
        assert!(h.engine.hot.anim_placeholder, "{mon}: initial placeholder");
        assert!(
            (h.engine.hot.swipe_scroll_gain - 4.0).abs() < 1e-9,
            "{mon}: initial swipe_scroll_gain"
        );
        assert!(
            (h.engine.hot.win_wheel_step_px - 240.0).abs() < 1e-9,
            "{mon}: initial win_wheel_step_px"
        );
        assert_eq!(
            h.engine.displays_arc.as_ref(),
            h.engine.displays.as_slice(),
            "{mon}: displays Arc synced"
        );
        assert!(
            h.engine.last_holes_arc.contains(&initial_hole),
            "{mon}: initial holes Arc synced"
        );

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

        assert_eq!(
            h.engine.hot.focused_color,
            Some(0xFF00AA),
            "{mon}: reloaded focused_color"
        );
        assert_eq!(h.engine.hot.gap_px, 2, "{mon}: reloaded gap_px");
        assert_eq!(
            h.engine.hot.stack_peek_px,
            Some(42),
            "{mon}: reloaded stack_peek_px"
        );
        assert!(!h.engine.hot.anim_enabled, "{mon}: reloaded anim.enabled");
        assert!(
            !h.engine.hot.anim_placeholder,
            "{mon}: reloaded anim.placeholder"
        );
        assert!(
            (h.engine.hot.swipe_scroll_gain - 2.5).abs() < 1e-9,
            "{mon}: reloaded swipe_scroll_gain"
        );
        assert!(
            (h.engine.hot.win_wheel_step_px - 360.0).abs() < 1e-9,
            "{mon}: reloaded win_wheel_step_px"
        );

        // Strip layout & Scene Arc holes reflect the reloaded gap and updated unmanaged window rect.
        let r1 = h.rect(first_id);
        let r2 = h.rect(second_id);
        let gap = if mon == "MON0" {
            r2.x - (r1.x + r1.w)
        } else {
            r2.y - (r1.y + r1.h)
        };
        assert_eq!(gap, 2, "{mon}: reloaded gap_px applied along strip axis");
        let last_holes = h
            .desktop
            .0
            .lock()
            .unwrap()
            .scenes
            .last()
            .unwrap()
            .holes
            .clone();
        assert!(
            last_holes.contains(&moved_hole),
            "{mon}: Scene holes Arc refreshed to moved rect"
        );
        assert!(
            !last_holes.contains(&initial_hole),
            "{mon}: stale hole rect evicted"
        );
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
    assert_eq!(
        h.engine.config_version,
        before + 1,
        "one reload, one version bump"
    );
    assert_eq!(h.engine.hot.gap_px, 11);
    h.stop_clean();
}

#[test]
fn edge_resize_suppresses_merge_boxes_and_borders_while_move_shows_them() {
    use crate::daemon::is_edge_resize;

    // Unit-level classification checks: 4 edges, 4 corners, cross-DPI move, snapped drag-restore.
    let orig = Rect {
        x: 100,
        y: 50,
        w: 1200,
        h: 900,
    };
    assert!(
        is_edge_resize(
            orig,
            Rect {
                x: 100,
                y: 50,
                w: 900,
                h: 900
            }
        ),
        "right edge resize"
    );
    assert!(
        is_edge_resize(
            orig,
            Rect {
                x: 250,
                y: 50,
                w: 1050,
                h: 900
            }
        ),
        "left edge resize"
    );
    assert!(
        is_edge_resize(
            orig,
            Rect {
                x: 100,
                y: 50,
                w: 1200,
                h: 700
            }
        ),
        "bottom edge resize"
    );
    assert!(
        is_edge_resize(
            orig,
            Rect {
                x: 100,
                y: 150,
                w: 1200,
                h: 800
            }
        ),
        "top edge resize"
    );
    assert!(
        is_edge_resize(
            orig,
            Rect {
                x: 200,
                y: 150,
                w: 1100,
                h: 800
            }
        ),
        "top-left corner resize"
    );
    assert!(
        !is_edge_resize(
            orig,
            Rect {
                x: 1600,
                y: 60,
                w: 960,
                h: 720
            }
        ),
        "cross-DPI move shifts both horizontal edges"
    );
    assert!(
        !is_edge_resize(
            orig,
            Rect {
                x: 220,
                y: 50,
                w: 900,
                h: 700
            }
        ),
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
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, first_id));
        h.desktop
            .0
            .lock()
            .unwrap()
            .windows
            .get_mut(&first_id)
            .unwrap()
            .rect = resize_fn(r1);
        h.engine.tick().unwrap();
        h.settle_quiet();

        let last = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
        assert!(
            last.cue.is_none(),
            "{mon}: resize must not emit merge zones"
        );
        assert!(
            h.engine.drag.as_ref().is_some_and(|d| d.resizing),
            "{mon}: drag session should latch resizing=true"
        );

        // Complete resize -> column shrinks, does NOT group/swap with second_id.
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, first_id));
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
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, first_id));
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
        assert!(
            move_scene.cue.is_none()
                && h.engine
                    .drag
                    .as_ref()
                    .is_some_and(|d| d.target == Some(second_id)),
            "{mon}: real move aims at the neighbour; the cue only appears after the dwell"
        );
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, first_id));
        h.settle();
        let s = &h.engine.strips[&key];
        assert_eq!(
            s.locate(first_id)
                .unwrap()
                .0
                .abs_diff(s.locate(second_id).unwrap().0),
            1,
            "{mon}: dropping mid-neighbour without the modifier tiles beside it, never stacks"
        );
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
            sim.add_window(SimWindow::new(
                101,
                Rect {
                    x: 0,
                    y: 0,
                    w: 960,
                    h: 1152,
                },
            ));
            sim.add_window(SimWindow::new(
                102,
                Rect {
                    x: 960,
                    y: 0,
                    w: 960,
                    h: 1152,
                },
            ));
            sim.add_window(SimWindow::new(
                103,
                Rect {
                    x: 1920,
                    y: 0,
                    w: 960,
                    h: 1152,
                },
            ));
        });
        focus(&mut h, 103);
        // Scroll partially so 101 peeks on the left, 102 in middle, 103 on right.
        let key = (h.engine.desktop.unwrap(), "MON0".to_string());
        h.engine.strips.get_mut(&key).unwrap().offset = 400;
        h.engine.dirty = true;
        h.settle();

        let r103 = h.rect(103);
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 103));
        // Move window 103 toward the left edge to trigger edge-scroll.
        {
            let mut d = h.desktop.0.lock().unwrap();
            d.windows.get_mut(&103).unwrap().rect = Rect {
                x: r103.x - 300,
                y: 40,
                ..r103
            };
            d.cursor = (10, 500);
        }
        let off_before = h.engine.strips[&key].offset;
        h.engine.tick().unwrap();
        let off_after_one = h.engine.strips[&key].offset;
        assert!(
            off_after_one < off_before,
            "edge-scroll should scroll left toward left windows"
        );
        assert!(
            h.engine.quick && h.engine.action == "scroll",
            "edge-scroll animates with the short trackpad-style ease (drop zones no longer exist)"
        );
        // Calling tick() again immediately (<30ms) must be rate-limited so left windows aren't flung away.
        h.engine.tick().unwrap();
        assert_eq!(
            h.engine.strips[&key].offset, off_after_one,
            "edge-scroll must be rate-limited against 120Hz bursts"
        );
        h.settle_quiet();

        // The left window (101) is partly visible; aim at its visible rect (no drop zones, #112).
        let left_zone = visible_rect(
            &h,
            101,
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1140,
            },
        );
        assert!(
            left_zone.x >= 0 && left_zone.x + left_zone.w <= 1920,
            "101 visible rect {left_zone:?} rect {:?} offset {}",
            h.rect(101),
            h.engine.strips[&key].offset
        );
        h.desktop.0.lock().unwrap().cursor =
            (left_zone.x + left_zone.w / 2, left_zone.y + left_zone.h / 2);
        h.engine.tick().unwrap();
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 103));
        h.settle();
        let s = &h.engine.strips[&key];
        assert_eq!(
            s.locate(103).unwrap().0.abs_diff(s.locate(101).unwrap().0),
            1,
            "dropping 103 over scrolled left window 101 (no modifier) must tile it beside 101, never stack"
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
        let bottom_zone = visible_rect(
            &h,
            303,
            Rect {
                x: -1440,
                y: 0,
                w: 1440,
                h: 2512,
            },
        );
        assert!(
            bottom_zone.y >= 0 && bottom_zone.y + bottom_zone.h <= 2512,
            "portrait bottom window must stay within monitor work area"
        );

        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 301));
        {
            let mut d = h.desktop.0.lock().unwrap();
            d.windows.get_mut(&301).unwrap().rect = Rect {
                x: r301.x + 80,
                y: r301.y + 400,
                ..r301
            };
            d.cursor = (
                bottom_zone.x + bottom_zone.w / 2,
                bottom_zone.y + bottom_zone.h / 2,
            );
        }
        h.engine.tick().unwrap();
        h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 301));
        h.settle();
        let s = &h.engine.strips[&key];
        assert_eq!(
            s.locate(301).unwrap().0.abs_diff(s.locate(303).unwrap().0),
            1,
            "dropping 301 over bottom window 303 (no modifier) tiles beside it on the portrait monitor"
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
        Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1140,
        },
        Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1200,
        },
    )];
    let mut cfg = note_cfg();
    cfg["animation"]["enabled"] = toml::Value::Boolean(true);
    cfg["animation"]["duration_ms"] = toml::Value::Integer(180);
    cfg["layout"]["stack_peek_px"] = toml::Value::Integer(28);

    let mut h = SimHarness::with_setup(rig, cfg, |sim| {
        // Add in reverse z-order so scan() builds columns [1376768, 11339466, 1049722, 26216776].
        sim.add_window(SimWindow::new(
            26216776,
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1140,
            },
        ));
        sim.add_window(SimWindow::new(
            1049722,
            Rect {
                x: 0,
                y: 0,
                w: 957,
                h: 1140,
            },
        ));
        sim.add_window(SimWindow::new(
            11339466,
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1140,
            },
        ));
        sim.add_window(SimWindow::new(
            1376768,
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1140,
            },
        ));
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
        h.engine
            .drag
            .as_ref()
            .is_some_and(|d| d.pad_drag && d.moved),
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
    assert_eq!(
        drag_scene.duration, 0,
        "drag hover scene must be instant so zones match tiles"
    );
    assert!(
        drag_scene.cue.is_none(),
        "before 1/4 of the neighbour no cue is drawn"
    );

    // Assert EVERY zone against its own tile's clipped rect and stack_zone(...).
    let tile_rect = |id: isize| {
        drag_scene
            .tiles
            .iter()
            .find(|&&(tid, _)| tid == id)
            .map(|&(_, r)| r)
            .unwrap()
    };
    assert_eq!(
        tile_rect(1376768),
        Rect {
            x: -1455,
            y: 0,
            w: 1920,
            h: 1140
        }
    );
    assert_eq!(
        tile_rect(11339466),
        Rect {
            x: 471,
            y: 0,
            w: 1920,
            h: 1140
        }
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
        assert_eq!(
            s.locate(26216776).unwrap().0,
            2,
            "recorded drop at (1401,633): without a perpendicular swipe it tiles after 11339466, never stacks"
        );
        assert_eq!(s.locate(11339466).unwrap().0, 1);
        assert_eq!(
            s.offset, 3852,
            "tiled full-width 1920px window at col 2 is revealed at offset 3852"
        );
        assert_eq!(
            s.locate(1376768).unwrap().0,
            0,
            "left window 1376768 remains intact at col 0"
        );
    }
    // Scrolling/focusing left immediately restores the left window at x = 0.
    for _ in 0..2 {
        h.cmd(Command::Key {
            name: "focus_left".into(),
        });
        h.settle();
    }
    assert_eq!(h.engine.strips[&key].offset, 0);
    assert_eq!(
        h.rect(1376768),
        Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1140
        }
    );
    h.stop_clean();
}

#[test]
fn fast_tick_drives_smooth_refresh_paced_edge_scroll() {
    // Finding #3: edge scrolling runs via wants_fast_tick / fast_tick at display refresh rate,
    // without requiring 32 ms tick() or pad_move() calls.
    let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            103,
            Rect {
                x: 1920,
                y: 0,
                w: 960,
                h: 1152,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 960,
                y: 0,
                w: 960,
                h: 1152,
            },
        ));
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 960,
                h: 1152,
            },
        ));
    });
    let key = (h.engine.desktop.unwrap(), "MON0".to_string());
    focus(&mut h, 103);
    h.engine.strips.get_mut(&key).unwrap().offset = 600;
    h.engine.dirty = true;
    h.settle();

    let r103 = h.rect(103);
    h.desktop.0.lock().unwrap().cursor = (r103.x + 100, 500);
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 103));
    {
        let mut d = h.desktop.0.lock().unwrap();
        d.windows.get_mut(&103).unwrap().rect = Rect {
            x: r103.x - 200,
            y: 40,
            ..r103
        };
        d.cursor = (10, 500); // within 48px of left edge
    }
    // First tick enters edge-scroll and arms wants_fast_tick().
    h.engine.tick().unwrap();
    assert!(
        h.engine.wants_fast_tick(),
        "active drag at strip edge must request fast_tick"
    );

    // Drive edge scrolling exclusively via fast_tick() at ~60 Hz (17 ms intervals).
    let start_offset = h.engine.strips[&key].offset;
    let start_t = Instant::now();
    let mut steps = 0;
    for _ in 0..4 {
        thread::sleep(Duration::from_millis(18));
        let before = h.engine.strips[&key].offset;
        h.engine.fast_tick();
        let after = h.engine.strips[&key].offset;
        assert!(
            after < before,
            "fast_tick must advance edge scroll smoothly"
        );
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
        (150.0..=1300.0).contains(&speed),
        "edge-scroll eases in: slower than the 1400 px/s cap over the first ~80 ms (got {speed:.1} px/s over {elapsed_s:.3}s)"
    );

    // Moving cursor away from the edge disarms wants_fast_tick().
    h.desktop.0.lock().unwrap().cursor = (500, 500);
    h.engine.fast_tick();
    assert!(
        !h.engine.wants_fast_tick(),
        "leaving edge zone must disarm fast_tick"
    );
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
            sim.add_window(SimWindow::new(
                104,
                Rect {
                    x: 1000,
                    y: 100,
                    w: 600,
                    h: 400,
                },
            ));
            sim.add_window(SimWindow::new(
                304,
                Rect {
                    x: -1300,
                    y: 1600,
                    w: 900,
                    h: 400,
                },
            ));
        });
        let key = (h.engine.desktop.unwrap(), mon.to_string());
        focus(&mut h, w1);
        h.cmd(Command::Key {
            name: "stack_toggle".into(),
        });
        h.settle();
        let s = &h.engine.strips[&key];
        assert_eq!(
            s.columns[s.focus].members.len(),
            2,
            "{mon}: col 0 is a 2-window pair"
        );
        let first_active = s.active().unwrap();
        let first_idx = s.columns[s.focus].active;
        let step_delta = if first_idx == 0 { -120 } else { 120 };

        // 1. Plain Win+wheel on focused 2-window pair cycles to the other member.
        h.engine.event(Event::Wheel {
            delta: step_delta,
            tilt: false,
            shift: false,
            x: cursor.0,
            y: cursor.1,
        });
        h.settle();
        let second_active = h.engine.strips[&key].active().unwrap();
        assert_ne!(
            second_active, first_active,
            "{mon}: Win+Scroll cycles pair member"
        );

        // 2. Another notch in the same direction hits the pair end: no wrap, no dead scene.
        let scenes_at_end = h.desktop.0.lock().unwrap().scenes.len();
        h.engine.event(Event::Wheel {
            delta: step_delta,
            tilt: false,
            shift: false,
            x: cursor.0,
            y: cursor.1,
        });
        h.settle();
        assert_eq!(
            h.engine.strips[&key].active().unwrap(),
            second_active,
            "{mon}: pair clamps at end"
        );
        assert_eq!(
            h.desktop.0.lock().unwrap().scenes.len(),
            scenes_at_end,
            "{mon}: clamped pair end emits no dead scene"
        );

        // 3. Opposite notch returns to first member.
        h.engine.event(Event::Wheel {
            delta: -step_delta,
            tilt: false,
            shift: false,
            x: cursor.0,
            y: cursor.1,
        });
        h.settle();
        assert_eq!(
            h.engine.strips[&key].active().unwrap(),
            first_active,
            "{mon}: opposite Win+Scroll returns"
        );

        // 4. Win+Shift+wheel on a focused group does the opposite: scrolls the strip!
        let off_before = h.engine.strips[&key].offset;
        h.engine.event(Event::Wheel {
            delta: -120,
            tilt: false,
            shift: true,
            x: cursor.0,
            y: cursor.1,
        });
        h.settle();
        assert!(
            h.engine.strips[&key].offset > off_before,
            "{mon}: Win+Shift+wheel scrolls strip away from focused group"
        );
        settle_now(&mut h);

        // 5. Focused ungrouped column (w3): plain Win+wheel scrolls the strip, and a burst scrolling back
        //    across the group at col 0 keeps scrolling the strip without getting trapped rolling the group.
        focus(&mut h, w3);
        let group_active_before = h.engine.strips[&key].columns[0].active;
        let off_start = h.engine.strips[&key].offset;
        // Scroll back toward col 0 in a multi-notch burst (without settling between notches).
        for _ in 0..6 {
            h.engine.event(Event::Wheel {
                delta: 120,
                tilt: false,
                shift: false,
                x: cursor.0,
                y: cursor.1,
            });
        }
        h.settle();
        assert!(
            h.engine.strips[&key].offset < off_start,
            "{mon}: ungrouped Win+wheel scrolls strip"
        );
        assert_eq!(
            h.engine.strips[&key].columns[0].active, group_active_before,
            "{mon}: strip-scroll burst across group must not get trapped rolling the group"
        );
        settle_now(&mut h);

        // 6. 3-member group: plain Win+wheel wraps around all 3 members.
        focus(&mut h, w3);
        h.cmd(Command::Key {
            name: "stack_toggle".into(),
        });
        h.settle();
        let group_col = h.engine.strips[&key].focus;
        assert_eq!(
            h.engine.strips[&key].columns[group_col].members.len(),
            3,
            "{mon}: 3-member group"
        );
        let start_id = h.engine.strips[&key].active().unwrap();
        let mut seen = vec![start_id];
        for _ in 0..3 {
            h.engine.event(Event::Wheel {
                delta: -120,
                tilt: false,
                shift: false,
                x: cursor.0,
                y: cursor.1,
            });
            h.settle();
            seen.push(h.engine.strips[&key].active().unwrap());
        }
        assert_eq!(
            seen.last().copied(),
            Some(start_id),
            "{mon}: 3-member group wraps back to start after 3 notches"
        );
        assert_eq!(
            seen[0..3]
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3,
            "{mon}: visits all 3 members"
        );
        h.stop_clean();
    }
}

#[test]
fn touchpad_fling_velocity_edges_shipped_inertia_one_motion_and_wrapping() {
    // Covers #16, #45, and Findings #1, #2, #3, #4, #8, #9.
    use crate::daemon::{fling_advance_direction, scroll_fling_velocity};

    // Finding #3: velocity edge cases (pause-then-lift, single report, slow drag, short flick, symmetry).
    assert_eq!(
        scroll_fling_velocity(&[(0.0, 25.0)]),
        0.0,
        "single report must not be a fling"
    );
    assert_eq!(
        scroll_fling_velocity(&[(0.09, 30.0), (0.06, 30.0)]),
        0.0,
        "pause >40ms before lift must not be a fling"
    );
    let v_slow = scroll_fling_velocity(&[(0.09, 2.0), (0.06, 2.0), (0.03, 2.0), (0.0, 2.0)]);
    assert_eq!(
        fling_advance_direction(v_slow, 8.0, true, 250.0, 18.0),
        None,
        "slow drag must not fling"
    );
    assert_eq!(
        fling_advance_direction(500.0, 10.0, true, 250.0, 18.0),
        None,
        "tiny travel below min_travel must not fling"
    );
    let v_right = scroll_fling_velocity(&[(0.06, 30.0), (0.03, 30.0), (0.0, 30.0)]);
    let v_left = scroll_fling_velocity(&[(0.06, -30.0), (0.03, -30.0), (0.0, -30.0)]);
    assert!(
        (v_right + v_left).abs() < 1e-9,
        "velocity must be exactly symmetric left vs right"
    );
    assert!(
        (v_right - 1000.0).abs() < 1e-6,
        "n-1 deltas over n-1 intervals: 60 / 0.06 = 1000"
    );
    assert_eq!(
        fling_advance_direction(v_right, 90.0, true, 250.0, 18.0),
        Some(-1)
    );
    assert_eq!(
        fling_advance_direction(v_left, -90.0, true, 250.0, 18.0),
        Some(1)
    );
    assert_eq!(
        fling_advance_direction(v_right, 90.0, false, 250.0, 18.0),
        Some(1)
    );
    assert_eq!(
        fling_advance_direction(v_left, -90.0, false, 250.0, 18.0),
        Some(-1)
    );

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
    assert_eq!(
        h.engine.strips[&k0].focus, 1,
        "shipped inertia=true fling advances directly to col 1, never skipping to col 0"
    );
    assert!(
        h.engine.inertia.is_none(),
        "one-motion fling must not leave decaying inertia"
    );
    assert!(
        h.engine.settle.is_none(),
        "one-motion fling must not wait on a settle pause"
    );

    // Symmetric fast flick left (natural -> toward col 2).
    h.engine.event(Event::Contacts(pts(200.0)));
    h.engine.event(Event::Contacts(pts(185.0)));
    h.engine.event(Event::Contacts(pts(165.0)));
    h.engine.event(Event::Contacts(pts(145.0)));
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(
        h.engine.strips[&k0].focus, 2,
        "symmetric fling advances directly back to col 2"
    );
    assert!(h.engine.inertia.is_none());
    assert!(h.engine.settle.is_none());

    // Boundary-clamped inertia stops immediately without emitting redundant no-op scenes.
    focus(&mut h, 103);
    let scenes_before = h.desktop.0.lock().unwrap().scenes.len();
    h.engine.inertia = Some((
        false,
        -800.0,
        Instant::now() - Duration::from_millis(16),
        Some(96.0),
    ));
    h.engine.fast_tick();
    assert!(
        h.engine.inertia.is_none(),
        "inertia clamped at strip boundary must stop immediately"
    );
    assert_eq!(
        h.desktop.0.lock().unwrap().scenes.len(),
        scenes_before,
        "clamped inertia emits no redundant scenes"
    );

    // Finding #2: focus key and Win+wheel during inertia clear inertia/scroll state immediately.
    h.engine.inertia = Some((
        false,
        600.0,
        Instant::now() - Duration::from_millis(16),
        Some(96.0),
    ));
    h.cmd(Command::Key {
        name: "focus_left".into(),
    });
    h.settle();
    assert!(
        h.engine.inertia.is_none(),
        "focus key must cancel active inertia"
    );
    assert!(
        h.engine.scroll_start_focus.is_none(),
        "focus key must clear scroll_start_focus"
    );

    h.engine.inertia = Some((
        false,
        600.0,
        Instant::now() - Duration::from_millis(16),
        Some(96.0),
    ));
    h.engine.event(Event::Wheel {
        delta: -120,
        tilt: false,
        shift: false,
        x: 500,
        y: 500,
    });
    h.settle();
    assert!(
        h.engine.inertia.is_none(),
        "Win+wheel must cancel active inertia"
    );
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
    assert_eq!(
        h.engine.active, "MON0",
        "fast cross-axis swipe from MON2 hops to MON0"
    );
    assert_eq!(
        h.engine.strips[&k0].active().unwrap(),
        mon0_active_before,
        "hop with fast lift must land on MON0's active window without an extra column advance"
    );

    // Finding #9: 3-member wrapping group fling past last member wraps to member 0 (and 0 -> 2).
    focus(&mut h, 101);
    h.cmd(Command::Key {
        name: "stack_toggle".into(),
    });
    h.settle();
    focus(&mut h, 103);
    h.cmd(Command::Key {
        name: "stack_toggle".into(),
    });
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
    assert_eq!(
        h.engine.strips[&k0].columns[0].active, 2,
        "still member 2 before lift"
    );
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(
        h.engine.strips[&k0].columns[0].active, 0,
        "3-member group fling wraps 2 -> 0"
    );

    // Fast downward flick under 50% (natural -> dir = -1) wraps from member 0 -> 2 in one motion.
    h.engine.event(Event::Contacts(vpts(200.0)));
    h.engine.event(Event::Contacts(vpts(215.0)));
    h.engine.event(Event::Contacts(vpts(235.0)));
    h.engine.event(Event::Contacts(vpts(255.0)));
    assert_eq!(
        h.engine.strips[&k0].columns[0].active, 0,
        "still member 0 before lift"
    );
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(
        h.engine.strips[&k0].columns[0].active, 2,
        "3-member group fling wraps 0 -> 2"
    );
    h.stop_clean();

    // Finding #8: fitting 2- and 3-column strip (max_offset == 0) flings symmetrically left and right.
    for n_cols in [2usize, 3] {
        let mut cfg = test_cfg();
        cfg["touchpad"]["swipe_inertia"] = toml::Value::Boolean(true);
        cfg["touchpad"]["swipe_natural_direction"] = toml::Value::Boolean(true);
        let mut hf = SimHarness::with_setup(primary_only_rig(), cfg, |sim| {
            for i in 0..n_cols {
                sim.add_window(SimWindow::new(
                    501 + i as isize,
                    Rect {
                        x: 100 * i as i32,
                        y: 100,
                        w: 500,
                        h: 400,
                    },
                ));
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
        assert_eq!(
            hf.engine.strips[&kf].focus, 1,
            "fitting {n_cols}-col strip: fling +1 advances 0 -> 1"
        );
        if n_cols == 3 {
            // Fling +1 again: 1 -> 2.
            hf.engine.event(Event::Contacts(pts(200.0)));
            hf.engine.event(Event::Contacts(pts(175.0)));
            hf.engine.event(Event::Contacts(pts(145.0)));
            hf.engine.event(Event::Contacts(pts(115.0)));
            hf.engine.event(Event::Contacts(vec![]));
            hf.settle();
            assert_eq!(
                hf.engine.strips[&kf].focus, 2,
                "fitting 3-col strip: fling +1 advances 1 -> 2"
            );
            // Fling -1: 2 -> 1.
            hf.engine.event(Event::Contacts(pts(100.0)));
            hf.engine.event(Event::Contacts(pts(125.0)));
            hf.engine.event(Event::Contacts(pts(155.0)));
            hf.engine.event(Event::Contacts(pts(185.0)));
            hf.engine.event(Event::Contacts(vec![]));
            hf.settle();
            assert_eq!(
                hf.engine.strips[&kf].focus, 1,
                "fitting 3-col strip: fling -1 advances 2 -> 1"
            );
        }
        // Fling -1: 1 -> 0.
        hf.engine.event(Event::Contacts(pts(100.0)));
        hf.engine.event(Event::Contacts(pts(125.0)));
        hf.engine.event(Event::Contacts(pts(155.0)));
        hf.engine.event(Event::Contacts(pts(185.0)));
        hf.engine.event(Event::Contacts(vec![]));
        hf.settle();
        assert_eq!(
            hf.engine.strips[&kf].focus, 0,
            "fitting {n_cols}-col strip: fling -1 advances 1 -> 0"
        );
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
    assert!(
        (ext_px - 384.0).abs() < 1e-6,
        "2560px @ 96 DPI preserves 384 px/in external feel (got {ext_px})"
    );
    assert!((ext_px / 2560.0 - 0.15).abs() < 1e-6);
    assert!(
        (lap_px / 1920.0 - 0.15).abs() < 1e-6,
        "1920px @ 144 DPI matches 15% viewport/in"
    );
    assert!(
        (lap_cursor_on_ext_px / 1920.0 - 0.15).abs() < 1e-6,
        "cursor monitor DPI must not skew laptop scroll"
    );
    assert!(
        (uhd_px / 3840.0 - 0.15).abs() < 1e-6,
        "universal across arbitrary resolutions/DPIs (4K @ 200%)"
    );
    assert!(
        (raw_px - 384.0).abs() < 1e-6,
        "raw-unit digitizer (None) is independent of cursor DPI"
    );

    // Round 2 Finding #5: group roll normalized to physical inches across DPIs.
    let roll_96 = group_roll_delta(96.0 * 4.0, Some(96.0));
    let roll_144 = group_roll_delta(144.0 * 4.0, Some(144.0));
    let roll_192 = group_roll_delta(192.0 * 4.0, Some(192.0));
    assert!(
        (roll_96 - roll_144).abs() < 1e-9 && (roll_96 - roll_192).abs() < 1e-9,
        "group roll identical across DPIs"
    );
    assert!(
        (roll_96 - 0.552).abs() < 0.002,
        "matches external landscape reference (~0.552 steps/in)"
    );

    // Round 2 Finding #4: same physical flick (in/s and inches) makes the exact same fling decision at 96/144/192 DPI.
    for dpi in [96.0, 144.0, 192.0] {
        // Below threshold: 2.0 in/s, 0.25 in -> None at every DPI.
        let slow_speed = 2.0 * dpi;
        let travel = 0.25 * dpi;
        assert_eq!(
            fling_advance_direction(
                slow_speed / dpi,
                travel / dpi,
                true,
                250.0 / 96.0,
                18.0 / 96.0
            ),
            None,
            "2.0 in/s must not fling at {dpi} DPI"
        );
        // Above threshold: 3.0 in/s, 0.25 in -> Some(-1) at every DPI.
        let fast_speed = 3.0 * dpi;
        assert_eq!(
            fling_advance_direction(
                fast_speed / dpi,
                travel / dpi,
                true,
                250.0 / 96.0,
                18.0 / 96.0
            ),
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
    h.engine.inertia = Some((
        false,
        -600.0,
        Instant::now() - Duration::from_millis(16),
        Some(144.0),
    ));
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
    assert!(
        h.engine.pad.is_some(),
        "4-finger drag must still trigger normally at 144 DPI"
    );
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
    h.cmd(Command::Key {
        name: "stack_toggle".into(),
    });
    h.settle();
    focus(&mut h, 103);
    h.cmd(Command::Key {
        name: "stack_toggle".into(),
    });
    h.settle();
    assert_eq!(h.engine.strips[&k0].columns[0].members.len(), 3);

    // 1a. Half-rolled (frac = 0.4) with settle pending -> focus key clears settle AND settles frac to 0.
    h.engine.strips.get_mut(&k0).unwrap().columns[0].frac = 0.4;
    h.engine.settle = Some(Instant::now() + Duration::from_millis(200));
    h.cmd(Command::Key {
        name: "focus_right".into(),
    });
    h.settle();
    assert!(
        h.engine.strips[&k0].columns.iter().all(|c| c.frac == 0.0),
        "focus key during pending settle must settle half-rolled frac to 0"
    );

    // 1b. Half-rolled during active inertia -> focus key settles frac to 0.
    focus(&mut h, 101);
    h.engine.strips.get_mut(&k0).unwrap().columns[0].frac = 0.4;
    h.engine.inertia = Some((
        true,
        300.0,
        Instant::now() - Duration::from_millis(16),
        Some(96.0),
    ));
    h.cmd(Command::Key {
        name: "focus_right".into(),
    });
    h.settle();
    assert!(
        h.engine.strips[&k0].columns.iter().all(|c| c.frac == 0.0),
        "focus key during inertia must settle half-rolled frac to 0"
    );

    // 1c. Half-rolled with settle pending -> Win+wheel settles frac to 0.
    focus(&mut h, 101);
    h.engine.strips.get_mut(&k0).unwrap().columns[0].frac = 0.4;
    h.engine.settle = Some(Instant::now() + Duration::from_millis(200));
    h.engine.event(Event::Wheel {
        delta: -120,
        tilt: false,
        shift: true,
        x: 500,
        y: 500,
    });
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
    assert!(
        h.engine.strips[&k0].columns[0].frac > 0.5,
        "live roll crossed 0.5 before lift"
    );
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(
        h.engine.strips[&k0].columns[0].active, 1,
        "3-deck forward fling at frac>0.5 lands on 1 (no skip to 2)"
    );
    assert_eq!(h.engine.strips[&k0].columns[0].frac, 0.0);

    // Backward (-1) from member 1 with live travel reaching frac < -0.5 before lift -> lands on 0 (no skip to 2):
    h.engine.event(Event::Contacts(vpts(200.0)));
    h.engine.event(Event::Contacts(vpts(240.0)));
    h.engine.event(Event::Contacts(vpts(285.0)));
    h.engine.event(Event::Contacts(vpts(325.0)));
    assert!(
        h.engine.strips[&k0].columns[0].frac < -0.5,
        "live backward roll crossed -0.5 before lift"
    );
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(
        h.engine.strips[&k0].columns[0].active, 0,
        "3-deck backward fling at frac<-0.5 lands on 0 (no skip to 2)"
    );
    assert_eq!(h.engine.strips[&k0].columns[0].frac, 0.0);

    // Round 2 Finding #2b: 2-member pair fling 0->1 and 1->0, both under 50% and over 50%.
    h.cmd(Command::Key {
        name: "stack_toggle".into(),
    }); // ungroup m0 -> remaining 2-member pair in col 0
    h.settle();
    let pair_col = h.engine.strips[&k0]
        .columns
        .iter()
        .position(|c| c.members.len() == 2)
        .unwrap();
    let p0 = h.engine.strips[&k0].columns[pair_col].members[0];
    let p1 = h.engine.strips[&k0].columns[pair_col].members[1];
    // Pair 0 -> 1 under 50%:
    focus(&mut h, p0);
    h.engine.event(Event::Contacts(vpts(300.0)));
    h.engine.event(Event::Contacts(vpts(280.0)));
    h.engine.event(Event::Contacts(vpts(255.0)));
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(
        h.engine.strips[&k0].columns[pair_col].active, 1,
        "pair 0->1 under 50% flings to 1"
    );
    // Pair 1 -> 0 under 50% (backward roll sets active=0, frac~0.75 before lift; must still fling cleanly to 0!):
    focus(&mut h, p1);
    h.engine.event(Event::Contacts(vpts(200.0)));
    h.engine.event(Event::Contacts(vpts(220.0)));
    h.engine.event(Event::Contacts(vpts(245.0)));
    assert!(
        h.engine.strips[&k0].columns[pair_col].frac > 0.5,
        "pair backward roll under 50% has frac > 0.5"
    );
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(
        h.engine.strips[&k0].columns[pair_col].active, 0,
        "pair 1->0 under 50% flings cleanly to 0"
    );
    assert!(
        h.engine.inertia.is_none(),
        "pair 1->0 fling is one motion, no inertia fallback"
    );
    // Pair 0 -> 1 over 50%:
    h.engine.event(Event::Contacts(vpts(400.0)));
    h.engine.event(Event::Contacts(vpts(355.0)));
    h.engine.event(Event::Contacts(vpts(305.0)));
    h.engine.event(Event::Contacts(vpts(265.0)));
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(
        h.engine.strips[&k0].columns[pair_col].active, 1,
        "pair 0->1 over 50% lands on 1"
    );
    // Pair 1 -> 0 over 50%:
    h.engine.event(Event::Contacts(vpts(200.0)));
    h.engine.event(Event::Contacts(vpts(245.0)));
    h.engine.event(Event::Contacts(vpts(295.0)));
    h.engine.event(Event::Contacts(vpts(335.0)));
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(
        h.engine.strips[&k0].columns[pair_col].active, 0,
        "pair 1->0 over 50% lands on 0"
    );

    // Round 2 Finding #3: wheel_mode must not go stale across monitors, after pair ends, or on Shift+wheel ungrouped.
    // 3a. Hit pair end on MON0, then immediately wheel over MON1 (ungrouped): MON1 must scroll!
    focus(&mut h, p0);
    h.engine.event(Event::Wheel {
        delta: 120,
        tilt: false,
        shift: false,
        x: 500,
        y: 500,
    }); // clamped at pair start 0
    to_start(&mut h, "MON1");
    let mon1_before = h.engine.strips[&k1].offset;
    h.engine.event(Event::Wheel {
        delta: -120,
        tilt: false,
        shift: false,
        x: 2500,
        y: 500,
    });
    h.settle();
    assert!(
        h.engine.strips[&k1].offset > mon1_before,
        "wheel over MON1 after hitting pair end on MON0 must scroll MON1 strip, not stay stuck in roll mode"
    );
    settle_now(&mut h);

    // 3b. Shift+wheel on an ungrouped column must not leave a stale roll mode behind.
    to_start(&mut h, "MON1");
    let mon1_before2 = h.engine.strips[&k1].offset;
    h.engine.event(Event::Wheel {
        delta: -120,
        tilt: false,
        shift: true,
        x: 2500,
        y: 500,
    }); // ungrouped Shift+wheel
    assert!(
        h.engine.wheel_mode.is_none(),
        "inapplicable roll mode must not be stored"
    );
    h.engine.event(Event::Wheel {
        delta: -120,
        tilt: false,
        shift: false,
        x: 2500,
        y: 500,
    });
    h.settle();
    assert!(
        h.engine.strips[&k1].offset > mon1_before2,
        "subsequent plain wheel scrolls MON1"
    );
    settle_now(&mut h);

    // 3c. Pair end on MON0, wait past 260ms window, switch focus to ungrouped col without FOREGROUND -> plain wheel scrolls.
    focus(&mut h, p0);
    h.engine.event(Event::Wheel {
        delta: 120,
        tilt: false,
        shift: false,
        x: 500,
        y: 500,
    });
    if let Some((_, ref mut at, _, _)) = h.engine.wheel_mode {
        *at = Instant::now() - Duration::from_millis(300);
    }
    let ungrouped_col = h.engine.strips[&k0]
        .columns
        .iter()
        .position(|c| c.members.len() == 1)
        .unwrap();
    h.engine.strips.get_mut(&k0).unwrap().focus = ungrouped_col;
    h.engine.event(Event::Wheel {
        delta: -120,
        tilt: false,
        shift: false,
        x: 500,
        y: 500,
    });
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
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
    {
        let mut d = h.desktop.0.lock().unwrap();
        d.windows.get_mut(&101).unwrap().rect = Rect {
            x: r1.x + 250,
            y: r1.y,
            w: r1.w - 180,
            h: r1.h - 200,
        };
        d.cursor = (r2.x + r2.w * 19 / 20, r2.y + r2.h / 2);
    }
    h.engine.tick().unwrap();
    assert!(
        h.engine
            .drag
            .as_ref()
            .is_some_and(|d| d.moved && !d.resizing),
        "snapped drag-restore must latch moved=true, resizing=false"
    );
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
    h.settle();
    let s = &h.engine.strips[&key0];
    assert_eq!(
        s.locate(102).unwrap().0 + 1,
        s.locate(101).unwrap().0,
        "snapped drag-restore dropped past 3/4 of 102 must tile after it, not resize"
    );
    h.stop_clean();

    // 2. Cross-DPI-and-back: move 101 to MON1 (DPI changes w/h), then return to MON0 and drop on 102.
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let key0 = (h.engine.desktop.unwrap(), "MON0".to_string());
    focus(&mut h, 101);
    let r1 = h.rect(101);
    let r2 = h.rect(102);
    h.desktop.0.lock().unwrap().cursor = (r1.x + 100, r1.y + 10);
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
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
    assert!(
        h.engine
            .drag
            .as_ref()
            .is_some_and(|d| d.moved && !d.resizing)
    );
    // Return to MON0 over 102's stack zone while still keeping a different size.
    {
        let mut d = h.desktop.0.lock().unwrap();
        d.windows.get_mut(&101).unwrap().rect = Rect {
            x: r2.x + 40,
            y: r2.y + 40,
            w: r1.w + 240,
            h: r1.h + 100,
        };
        d.cursor = (r2.x + r2.w * 19 / 20, r2.y + r2.h / 2);
    }
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
    h.settle();
    let s = &h.engine.strips[&key0];
    assert_eq!(
        s.locate(101).unwrap().0 + 1,
        s.locate(102).unwrap().0,
        "cross-DPI-and-back drop must tile before 102 (entered from the trailing edge), not resize"
    );
    h.stop_clean();
}

#[test]
fn same_monitor_drop_animates_and_release_near_edge_does_not_scroll() {
    // Finding #6 (same-monitor drop keeps animation duration even if final drag_update changes target),
    // Finding #7 (release within 48px of edge does not scroll), and Finding #8 (smooth edge_scroll_step).
    use crate::daemon::edge_scroll_step;

    // Finding #8 unit checks:
    assert_eq!(
        edge_scroll_step(0.005, 60.0, -1, 0.0),
        None,
        "sub-refresh interval skipped"
    );
    let (px60, rem60) = edge_scroll_step(1.0 / 60.0, 60.0, 1, 0.0).unwrap();
    assert_eq!(px60, 15, "900 px/s * 1/60s = 15 px");
    assert!(rem60.abs() < 1e-6);
    let (px_clamped, _) = edge_scroll_step(0.5, 60.0, -1, 0.0).unwrap();
    assert_eq!(px_clamped, -45, "stalls clamped to 50 ms -> 45 px max");
    assert_eq!(
        edge_scroll_step(0.02, 60.0, 0, 0.0),
        None,
        "dir=0 returns None"
    );

    let mut anim_cfg = note_cfg();
    anim_cfg["animation"]["enabled"] = toml::Value::Boolean(true);
    anim_cfg["animation"]["duration_ms"] = toml::Value::Integer(180);

    // Finding #6: final drag_update inside drag_end changes target/mode -> drop must still animate!
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), anim_cfg.clone(), |sim| {
            sim.add_window(SimWindow::new(
                101,
                Rect {
                    x: 0,
                    y: 0,
                    w: 960,
                    h: 1152,
                },
            ));
            sim.add_window(SimWindow::new(
                102,
                Rect {
                    x: 960,
                    y: 0,
                    w: 960,
                    h: 1152,
                },
            ));
        });
        focus(&mut h, 101);
        let r1 = h.rect(101);
        let r2 = h.rect(102);
        h.desktop.0.lock().unwrap().cursor = (r1.x + 50, r1.y + 50);
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.desktop
            .0
            .lock()
            .unwrap()
            .windows
            .get_mut(&101)
            .unwrap()
            .rect = Rect {
            x: r1.x + 40,
            y: r1.y + 40,
            ..r1
        };
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
            sim.add_window(SimWindow::new(
                103,
                Rect {
                    x: 1920,
                    y: 0,
                    w: 960,
                    h: 1152,
                },
            ));
            sim.add_window(SimWindow::new(
                102,
                Rect {
                    x: 960,
                    y: 0,
                    w: 960,
                    h: 1152,
                },
            ));
            sim.add_window(SimWindow::new(
                101,
                Rect {
                    x: 0,
                    y: 0,
                    w: 960,
                    h: 1152,
                },
            ));
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
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 102));
        h.desktop
            .0
            .lock()
            .unwrap()
            .windows
            .get_mut(&102)
            .unwrap()
            .rect = Rect {
            x: r102.x - 200,
            y: 40,
            ..r102
        };
        h.engine.tick().unwrap();
        // Place cursor in the leading 10% edge zone but below every tile (no drop target) and release immediately.
        {
            let mut d = h.desktop.0.lock().unwrap();
            d.cursor = (10, 1175);
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

fn tile_color(h: &SimHarness, id: isize) -> Option<u32> {
    h.desktop.0.lock().unwrap().scenes.last().and_then(|s| {
        s.tile_colors
            .iter()
            .find(|(tid, _)| *tid == id)
            .and_then(|(_, c)| *c)
    })
}

/// The border colour `planor state --json` reports for tile `id` (#107 parity with the drawn border).
fn state_color(h: &SimHarness, id: isize) -> Option<u32> {
    let snap = h.state();
    let tile = snap["tiles"]
        .as_array()?
        .iter()
        .find(|t| t["id"].as_i64() == Some(id as i64))?;
    tile["color"]
        .as_str()
        .and_then(|c| u32::from_str_radix(c.trim_start_matches('#'), 16).ok())
}

#[test]
fn issue_48_focus_highlight_removed_when_focus_on_unmanaged_app_dialog_or_system_ui() {
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 10,
                y: 10,
                w: 800,
                h: 600,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 900,
                y: 10,
                w: 800,
                h: 600,
            },
        ));
        // Elevated Task Manager (unmanaged external)
        let mut tm = SimWindow::new(
            901,
            Rect {
                x: 200,
                y: 200,
                w: 600,
                h: 500,
            },
        );
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        sim.add_window(tm);
        // Owned floating dialog (external() == false, not managed)
        let mut dlg = SimWindow::new(
            902,
            Rect {
                x: 300,
                y: 300,
                w: 400,
                h: 300,
            },
        );
        dlg.owner = 901;
        sim.add_window(dlg);
        // Planor.Note (Win+W note box: toolwindow + Planor class, external() == false)
        let mut note = SimWindow::new(
            903,
            Rect {
                x: 400,
                y: 200,
                w: 560,
                h: 112,
            },
        );
        note.class = "Planor.Note".into();
        note.toolwindow = true;
        note.topmost = true;
        sim.add_window(note);
        // Start menu (Windows.UI.Core.CoreWindow, external() == false)
        let mut start = SimWindow::new(
            904,
            Rect {
                x: 600,
                y: 400,
                w: 600,
                h: 600,
            },
        );
        start.class = "Windows.UI.Core.CoreWindow".into();
        start.topmost = true;
        sim.add_window(start);
        sim.foreground = 101;
    });

    assert_eq!(
        tile_color(&h, 101),
        Some(0x3B82F6),
        "managed foreground tile shows focus highlight"
    );
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
        assert_eq!(
            tile_color(&h, 101),
            None,
            "{label}: focus highlight must be removed"
        );
        assert_eq!(
            tile_color(&h, 102),
            None,
            "{label}: no other tile should gain highlight"
        );
    }

    // Focus back on managed tile 102 -> highlight returns on 102.
    h.desktop.0.lock().unwrap().foreground = 102;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 102));
    h.settle_quiet();
    assert_eq!(
        tile_color(&h, 102),
        Some(0x3B82F6),
        "highlight restores when managed tile regains focus"
    );
    assert_eq!(tile_color(&h, 101), None);
    h.stop_clean();
}

#[test]
fn issue_52_task_manager_behind_managed_windows_does_not_erase_highlight() {
    let tm_rect = Rect {
        x: 200,
        y: 150,
        w: 1200,
        h: 700,
    };
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        // Put Task Manager BEHIND both managed windows in Z-order.
        let mut tm = SimWindow::new(901, tm_rect);
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        sim.add_window(tm);
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 960,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
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
    let holes_behind = h
        .desktop
        .0
        .lock()
        .unwrap()
        .scenes
        .last()
        .unwrap()
        .holes
        .clone();
    for hole in &holes_behind {
        let ov1 = (hole.x + hole.w).min(r101.x + r101.w) - hole.x.max(r101.x);
        let ov2 = (hole.x + hole.w).min(r102.x + r102.w) - hole.x.max(r102.x);
        assert!(
            ov1 <= 0,
            "Task Manager behind 101 must not punch a hole over 101: hole={hole:?} r101={r101:?}"
        );
        assert!(
            ov2 <= 0,
            "Task Manager behind 102 must not punch a hole over 102: hole={hole:?} r102={r102:?}"
        );
    }
    assert_eq!(
        tile_color(&h, 101),
        Some(0x3B82F6),
        "focus highlight remains intact on 101"
    );

    // Raise Task Manager in front of both managed windows -> full rect is now carved as a hole.
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.z_order.retain(|&id| id != 901);
        g.z_order.insert(0, 901);
    }
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();
    let holes_front = h
        .desktop
        .0
        .lock()
        .unwrap()
        .scenes
        .last()
        .unwrap()
        .holes
        .clone();
    assert_eq!(
        holes_front,
        vec![tm_rect],
        "Task Manager in front of managed tiles cuts its full rect"
    );
    h.stop_clean();
}

#[test]
fn issue_17_system_flyouts_and_note_box_punch_holes_only_when_visible_and_uncloaked() {
    let start_rect = Rect {
        x: 640,
        y: 300,
        w: 640,
        h: 700,
    };
    let note_rect = Rect {
        x: 680,
        y: 340,
        w: 560,
        h: 112,
    };
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
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
        h.desktop
            .0
            .lock()
            .unwrap()
            .scenes
            .last()
            .unwrap()
            .holes
            .is_empty(),
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
    let holes = h
        .desktop
        .0
        .lock()
        .unwrap()
        .scenes
        .last()
        .unwrap()
        .holes
        .clone();
    assert!(
        holes.contains(&start_rect),
        "visible Start menu must punch a hole: {holes:?}"
    );
    assert!(
        holes.contains(&note_rect),
        "visible Win+W note box must punch a hole: {holes:?}"
    );

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
        h.desktop
            .0
            .lock()
            .unwrap()
            .scenes
            .last()
            .unwrap()
            .holes
            .is_empty(),
        "holes must clear when Start menu and note box close"
    );
    h.stop_clean();
}

/// #106: a Picture-in-Picture window (small, WS_EX_TOPMOST | WS_EX_TOOLWINDOW, not owned) is never tiled, and the
/// overlay leaves it alone: it punches a hole even while the strip scrolls under it, keeps its rect, and does not
/// stop the strip from scrolling. The taskbars and Planor's own topmost overlay hosts never punch holes.
#[test]
fn issue_106_pip_topmost_window_punches_hole_is_not_tiled_and_scroll_still_works() {
    let pip_rect = Rect {
        x: 1500,
        y: 800,
        w: 400,
        h: 225,
    };
    let tray_rect = Rect {
        x: 0,
        y: 1140,
        w: 1920,
        h: 60,
    };
    let host_rect = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1200,
    };
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        for (i, id) in [101, 102, 103].into_iter().enumerate() {
            sim.add_window(SimWindow::new(
                id,
                Rect {
                    x: 100 + 300 * i as i32,
                    y: 100,
                    w: 800,
                    h: 700,
                },
            ));
        }
        let mut tray = SimWindow::new(920, tray_rect);
        tray.class = "Shell_TrayWnd".into();
        tray.topmost = true;
        tray.toolwindow = true;
        sim.add_window(tray);
        let mut host = SimWindow::new(921, host_rect);
        host.class = "Planor.Composition".into();
        host.topmost = true;
        host.toolwindow = true;
        host.noactivate = true;
        sim.add_window(host);
        let mut pip = SimWindow::new(930, pip_rect);
        pip.class = "Chrome_WidgetWin_1".into();
        pip.topmost = true;
        pip.toolwindow = true;
        sim.add_window(pip);
        sim.foreground = 103;
    });
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();

    let snap = h.state();
    let ids: Vec<i64> = snap["tiles"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["id"].as_i64())
        .collect();
    for unmanaged in [920, 921, 930] {
        assert!(
            !ids.contains(&unmanaged),
            "window {unmanaged} must never be tiled: tiles {ids:?}"
        );
    }
    assert_eq!(ids.len(), 3, "the three normal windows are tiled: {ids:?}");
    let holes = h
        .desktop
        .0
        .lock()
        .unwrap()
        .scenes
        .last()
        .unwrap()
        .holes
        .clone();
    assert_eq!(
        holes,
        vec![pip_rect],
        "only the PiP window punches a hole (not the taskbar, not Planor's host)"
    );

    // Scroll the strip left twice and right twice under the PiP window: it keeps its rect and its hole, and the
    // focus really moves.
    let mut focused = vec![h.state()["strips"][0]["focus"].as_u64().unwrap()];
    for key in ["focus_left", "focus_left", "focus_right", "focus_right"] {
        h.cmd(Command::Key { name: key.into() });
        h.settle();
        h.engine.expire_timers_for_test();
        h.engine.tick().unwrap();
        h.settle_quiet();
        focused.push(h.state()["strips"][0]["focus"].as_u64().unwrap());
        assert_eq!(
            h.rect(930),
            pip_rect,
            "after {key}: the PiP window keeps its rect"
        );
        let holes = h
            .desktop
            .0
            .lock()
            .unwrap()
            .scenes
            .last()
            .unwrap()
            .holes
            .clone();
        assert_eq!(
            holes,
            vec![pip_rect],
            "after {key}: the PiP window still punches its hole"
        );
    }
    assert_eq!(
        focused,
        vec![2, 1, 0, 1, 2],
        "the strip still scrolls with a PiP window on top"
    );
    h.stop_clean();
}

/// #108 review L4: the simulated backend now passes the window's real extended style to
/// `topmost_overlay_occluder`, so the exclusions are covered end to end through the daemon: a click-through
/// (`WS_EX_TRANSPARENT`) topmost HUD and a topmost tooltip never punch overlay holes, while a PiP window does.
#[test]
fn topmost_click_through_hud_and_tooltip_never_punch_holes_but_pip_does() {
    let pip_rect = Rect {
        x: 1500,
        y: 800,
        w: 400,
        h: 225,
    };
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        for (i, id) in [101, 102, 103].into_iter().enumerate() {
            sim.add_window(SimWindow::new(
                id,
                Rect {
                    x: 100 + 300 * i as i32,
                    y: 100,
                    w: 800,
                    h: 700,
                },
            ));
        }
        let mut hud = SimWindow::new(
            940,
            Rect {
                x: 40,
                y: 40,
                w: 600,
                h: 300,
            },
        );
        hud.class = "GameOverlayHud".into();
        hud.topmost = true;
        hud.toolwindow = true;
        hud.transparent = true;
        sim.add_window(hud);
        let mut tip = SimWindow::new(
            941,
            Rect {
                x: 700,
                y: 500,
                w: 240,
                h: 60,
            },
        );
        tip.class = "tooltips_class32".into();
        tip.topmost = true;
        tip.toolwindow = true;
        sim.add_window(tip);
        let mut pip = SimWindow::new(930, pip_rect);
        pip.class = "Chrome_WidgetWin_1".into();
        pip.topmost = true;
        pip.toolwindow = true;
        sim.add_window(pip);
        sim.foreground = 103;
    });
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();
    let holes = h
        .desktop
        .0
        .lock()
        .unwrap()
        .scenes
        .last()
        .unwrap()
        .holes
        .clone();
    assert_eq!(
        holes,
        vec![pip_rect],
        "only the PiP punches a hole: not the click-through HUD, not the tooltip"
    );
    // The same HUD without WS_EX_TRANSPARENT is an interactive always-on-top window: it must punch a hole.
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&940)
        .unwrap()
        .transparent = false;
    h.engine.holes_stale = true;
    h.engine.dirty = true;
    h.settle_quiet();
    let holes = h
        .desktop
        .0
        .lock()
        .unwrap()
        .scenes
        .last()
        .unwrap()
        .holes
        .clone();
    assert!(
        holes.contains(&Rect {
            x: 40,
            y: 40,
            w: 600,
            h: 300
        }),
        "without WS_EX_TRANSPARENT the HUD is an occluder: {holes:?}"
    );
    h.stop_clean();
}
/// #107: a group's border shows only while the group holds the focus. Focus on another monitor, on an unmanaged
/// window, or on nothing (after the 2-tick NULL debounce) hides it; focus back on a member shows it again; rolling
/// between members keeps it. A focused ungrouped window keeps its normal focus border. Landscape and portrait.
#[test]
fn issue_107_group_border_only_while_group_is_focused() {
    const GROUP: u32 = 0xA855F7;
    const FOCUS: u32 = 0x3B82F6;
    let mut h = SimHarness::with_setup(three_monitor_rig(), test_cfg(), |sim| {
        three_per_monitor(sim);
        let mut tm = SimWindow::new(
            901,
            Rect {
                x: 200,
                y: 200,
                w: 600,
                h: 500,
            },
        );
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        sim.add_window(tm);
    });
    let focus = |h: &mut SimHarness, id: isize| {
        h.desktop.0.lock().unwrap().foreground = id;
        h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, id));
        h.settle_quiet();
    };
    // (monitor, first member, other monitor's window) for landscape MON0 and portrait MON2.
    for (mon, first, elsewhere) in [("MON0", 101isize, 201isize), ("MON2", 301, 101)] {
        focus(&mut h, first);
        h.cmd(Command::Key {
            name: "stack_toggle".into(),
        });
        h.settle();
        let key = h
            .engine
            .strips
            .keys()
            .find(|k| k.1 == mon)
            .cloned()
            .unwrap();
        let col = h.engine.strips[&key]
            .columns
            .iter()
            .find(|c| c.members.len() > 1)
            .cloned();
        let col = col.unwrap_or_else(|| panic!("{mon}: stack_toggle made no group"));
        let active = col.members[col.active];
        focus(&mut h, active);
        assert_eq!(
            tile_color(&h, active),
            Some(GROUP),
            "{mon}: focused group shows its border"
        );

        // Focus on another monitor: no group border; that window has its normal focus border.
        focus(&mut h, elsewhere);
        assert_eq!(
            tile_color(&h, active),
            None,
            "{mon}: group border hidden while {elsewhere} is focused"
        );
        assert_eq!(
            tile_color(&h, elsewhere),
            Some(FOCUS),
            "{mon}: ungrouped focused window keeps its focus border"
        );
        assert_eq!(
            state_color(&h, active),
            None,
            "{mon}: state --json agrees: no group border"
        );
        assert_eq!(
            state_color(&h, elsewhere),
            Some(FOCUS),
            "{mon}: state --json agrees: focus border"
        );

        // Focus on an unmanaged window: hidden.
        focus(&mut h, 901);
        assert_eq!(
            tile_color(&h, active),
            None,
            "{mon}: group border hidden while an unmanaged window is focused"
        );

        // Back in the group, then NULL foreground: kept for one tick (debounce), hidden after two.
        focus(&mut h, active);
        assert_eq!(
            tile_color(&h, active),
            Some(GROUP),
            "{mon}: group border back when the group is focused"
        );
        h.desktop.0.lock().unwrap().foreground = 0;
        h.engine.expire_timers_for_test();
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert_eq!(
            tile_color(&h, active),
            Some(GROUP),
            "{mon}: one NULL-foreground tick keeps the border"
        );
        // state --json uses the same debounced foreground: a raw read would see NULL and drop the border.
        assert_eq!(
            state_color(&h, active),
            Some(GROUP),
            "{mon}: state --json keeps the border over one NULL tick"
        );
        h.engine.expire_timers_for_test();
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert_eq!(
            tile_color(&h, active),
            None,
            "{mon}: two NULL-foreground ticks hide the group border"
        );
        assert_eq!(
            state_color(&h, active),
            None,
            "{mon}: state --json drops the border after two NULL ticks"
        );

        // Rolling between members keeps the border on the new active member.
        focus(&mut h, active);
        let roll = if h.engine.strips[&key].area.horizontal() {
            "focus_down"
        } else {
            "focus_right"
        };
        h.cmd(Command::Key { name: roll.into() });
        h.settle();
        let c = h.engine.strips[&key]
            .columns
            .iter()
            .find(|c| c.members.contains(&active))
            .unwrap()
            .clone();
        let now = c.members[c.active];
        assert_ne!(now, active, "{mon}: {roll} rolled the group");
        assert_eq!(
            h.desktop.foreground(),
            now,
            "{mon}: the roll focused the new member"
        );
        assert_eq!(
            tile_color(&h, now),
            Some(GROUP),
            "{mon}: border follows the roll"
        );
        h.cmd(Command::Key {
            name: "stack_toggle".into(),
        });
        h.settle();
    }
    h.stop_clean();
}

#[test]
fn issue_17_edge_resize_does_not_show_green_drop_target_border_or_zones() {
    let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 960,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.foreground = 101;
    });

    let r1 = h.rect(101);
    let r2 = h.rect(102);
    // Start edge resize of 101 (Windows sends EVENT_SYSTEM_MOVESIZESTART for resizes too).
    h.desktop.0.lock().unwrap().cursor = (r1.x + r1.w, r1.y + r1.h / 2);
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
    h.settle_quiet();

    // Drag right edge of 101 into 102's area (width grows from r1.w to r1.w + 200).
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&101).unwrap().rect.w = r1.w + 200;
        g.cursor = (r2.x + 150, r2.y + r2.h / 2);
    }
    h.engine.tick().unwrap();
    h.settle_quiet();

    assert!(
        zones(&h).is_empty(),
        "edge resize must not show green stack drop zones"
    );
    assert_ne!(
        tile_color(&h, 102),
        h.engine.hot.stacked_color,
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
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        let mut tm = SimWindow::new(
            901,
            Rect {
                x: 200,
                y: 200,
                w: 500,
                h: 400,
            },
        );
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        sim.add_window(tm);
        sim.foreground = 101;
    });

    // Open a new window -> triggers an `open_close` scene.
    let w2 = h.desktop.0.lock().unwrap().add_window(SimWindow::new(
        102,
        Rect {
            x: 100,
            y: 100,
            w: 800,
            h: 600,
        },
    ));
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
        g.windows.get_mut(&901).unwrap().rect = Rect {
            x: 320,
            y: 240,
            w: 500,
            h: 400,
        };
    }
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert_eq!(
        h.engine.seq, seq_before,
        "hole refresh must not supersede scene epoch"
    );
    assert_eq!(h.desktop.0.lock().unwrap().overlay_updates, ov_before + 1);
    let hole_scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
    assert_eq!(
        hole_scene.holes,
        vec![Rect {
            x: 320,
            y: 240,
            w: 500,
            h: 400
        }]
    );
    h.stop_clean();
}

#[test]
fn issue_50_51_focus_open_and_hole_change_mid_animation_preserve_motion() {
    let mut cfg = note_cfg();
    cfg["animation"]["enabled"] = toml::Value::Boolean(true);
    cfg["animation"]["duration_ms"] = toml::Value::Integer(180);
    let mut h = SimHarness::with_setup(primary_only_rig(), cfg, |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 960,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            103,
            Rect {
                x: 1920,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        let mut tm = SimWindow::new(
            901,
            Rect {
                x: 200,
                y: 200,
                w: 500,
                h: 400,
            },
        );
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        sim.add_window(tm);
        sim.foreground = 101;
    });

    // 1. Focus key -> 180 ms scroll scene, followed immediately by EVENT_SYSTEM_FOREGROUND:
    //    must NOT bump scene epoch or shorten the 180 ms scroll animation!
    h.cmd(Command::Key {
        name: "focus_right".into(),
    });
    h.settle();
    let scroll_seq = h.engine.seq;
    let scroll_scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
    assert_eq!(scroll_scene.action, "scroll");
    assert_eq!(scroll_scene.duration, 180);

    let fg = h.desktop.0.lock().unwrap().foreground;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, fg));
    h.settle_quiet();
    assert_eq!(
        h.engine.seq, scroll_seq,
        "FOREGROUND after focus key must not cut scroll animation short"
    );

    // 2. Open new window -> 180 ms open_close scene, followed by FOREGROUND:
    //    must NOT bump scene epoch or drop open_close handoff!
    let w4 = h.desktop.0.lock().unwrap().add_window(SimWindow::new(
        104,
        Rect {
            x: 100,
            y: 100,
            w: 800,
            h: 600,
        },
    ));
    h.engine.event(Event::Window(EVENT_OBJECT_SHOW, w4));
    h.settle();
    let open_seq = h.engine.seq;
    let open_scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
    assert_eq!(open_scene.action, "open_close");
    assert_eq!(open_scene.duration, 180);

    h.desktop.0.lock().unwrap().foreground = w4;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, w4));
    h.settle_quiet();
    assert_eq!(
        h.engine.seq, open_seq,
        "FOREGROUND after open must not drop open_close animation/handoff"
    );

    // 3. Hole change mid-animation (e.g. Task Manager exposed by parked tile) -> OverlayUpdate,
    //    NOT a new Scene epoch!
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.z_order.retain(|&id| id != 901);
        g.z_order.insert(0, 901);
        g.windows.get_mut(&901).unwrap().rect = Rect {
            x: 250,
            y: 250,
            w: 500,
            h: 400,
        };
    }
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert_eq!(
        h.engine.seq, open_seq,
        "hole change mid-animation must not supersede active scene"
    );
    assert_eq!(
        h.desktop.0.lock().unwrap().scenes.last().unwrap().holes,
        vec![Rect {
            x: 250,
            y: 250,
            w: 500,
            h: 400
        }]
    );
    h.stop_clean();
}

#[test]
fn issue_52_two_desktop_cloaked_managed_window_does_not_occlude_holes() {
    let tm_rect = Rect {
        x: 200,
        y: 150,
        w: 600,
        h: 500,
    };
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        // Managed window 101 on desktop 1.
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1140,
            },
        ));
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
        let mut w201 = SimWindow::new(
            201,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        );
        w201.desktop = 2;
        g.windows.insert(201, w201);
        g.z_order.push(201);
        g.foreground = 201;
    }
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    h.settle_quiet();

    let holes = h
        .desktop
        .0
        .lock()
        .unwrap()
        .scenes
        .last()
        .unwrap()
        .holes
        .clone();
    assert_eq!(
        holes,
        vec![tm_rect],
        "cloaked window on desktop 1 must not occlude Task Manager hole on desktop 2"
    );
    h.stop_clean();
}

#[test]
fn issue_52_pinned_window_never_punches_hole_and_occludes_unmanaged_behind_it() {
    let tm_rect = Rect {
        x: 200,
        y: 200,
        w: 500,
        h: 400,
    };
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 960,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.foreground = 102;
    });

    // Pin 102 and position it directly over Task Manager 901.
    h.cmd(Command::Key {
        name: "toggle_pin".into(),
    });
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

    let holes = h
        .desktop
        .0
        .lock()
        .unwrap()
        .scenes
        .last()
        .unwrap()
        .holes
        .clone();
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
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 960,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        // Save As dialog owned by managed tile 101.
        let mut managed_dlg = SimWindow::new(
            801,
            Rect {
                x: 200,
                y: 200,
                w: 500,
                h: 400,
            },
        );
        managed_dlg.owner = 101;
        sim.add_window(managed_dlg);
        // Elevated Task Manager + dialog owned by Task Manager.
        let mut tm = SimWindow::new(
            901,
            Rect {
                x: 250,
                y: 250,
                w: 600,
                h: 500,
            },
        );
        tm.class = "TaskManagerWindow".into();
        tm.elevated = true;
        sim.add_window(tm);
        let mut unmanaged_dlg = SimWindow::new(
            902,
            Rect {
                x: 300,
                y: 300,
                w: 400,
                h: 300,
            },
        );
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
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 960,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
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
    assert_eq!(
        tile_color(&h, 102),
        Some(0x3B82F6),
        "tick 1 still debounced"
    );
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert_eq!(
        tile_color(&h, 102),
        None,
        "sustained NULL foreground removes highlight"
    );
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
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.settle_quiet();
        // Move title bar first -> latches `moved = true`.
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect = Rect {
                x: r1.x + 120,
                y: r1.y + 40,
                ..r1
            };
            g.cursor = (r1.x + r1.w / 2 + 120, r1.y + 55);
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert_eq!(
            h.engine.drag.as_ref().map(|d| (d.moved, d.resizing)),
            Some((true, false))
        );
        // Cross onto MON1 and simulate WM_DPICHANGED changing window width/height!
        let r201 = h.rect(201);
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect = Rect {
                x: r201.x + 50,
                y: r201.y + 50,
                w: r1.w + 240,
                h: r1.h + 180,
            };
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
            sim.add_window(SimWindow::new(
                101,
                Rect {
                    x: 0,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.add_window(SimWindow::new(
                102,
                Rect {
                    x: 960,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.foreground = 101;
        });
        let r1 = h.rect(101);
        let r2 = h.rect(102);
        h.desktop.0.lock().unwrap().cursor = (r1.x + r1.w / 2, r1.y + 15);
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.settle_quiet();
        // Both left & right edges shift and size shrinks (drag-restore from snapped/maximized).
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect = Rect {
                x: r2.x + 40,
                y: r2.y + 40,
                w: r1.w - 150,
                h: r1.h - 200,
            };
            g.cursor = (r2.x + r2.w * 7 / 8, r2.y + r2.h / 2);
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert_eq!(
            h.engine.drag.as_ref().map(|d| (d.moved, d.resizing)),
            Some((true, false))
        );
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
            sim.add_window(SimWindow::new(
                101,
                Rect {
                    x: 0,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.add_window(SimWindow::new(
                102,
                Rect {
                    x: 960,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.foreground = 102;
        });
        let r2 = h.rect(102);
        for (label, next_rect) in [
            (
                "top edge",
                Rect {
                    y: r2.y + 40,
                    h: r2.h - 40,
                    ..r2
                },
            ),
            (
                "left edge",
                Rect {
                    x: r2.x + 60,
                    w: r2.w - 60,
                    ..r2
                },
            ),
            (
                "top-left corner",
                Rect {
                    x: r2.x + 60,
                    y: r2.y + 40,
                    w: r2.w - 60,
                    h: r2.h - 40,
                },
            ),
        ] {
            let cur_r2 = h.rect(102);
            h.desktop.0.lock().unwrap().cursor = (cur_r2.x, cur_r2.y);
            h.engine
                .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 102));
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
            assert!(
                zones(&h).is_empty(),
                "{label}: must not show green drop zones"
            );
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
            sim.add_window(SimWindow::new(
                101,
                Rect {
                    x: 0,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.add_window(SimWindow::new(
                102,
                Rect {
                    x: 960,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.foreground = 101;
        });
        let r1 = h.rect(101);
        let r2 = h.rect(102);
        h.desktop.0.lock().unwrap().cursor = (r1.x + 100, r1.y + 15);
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.settle_quiet();
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect = Rect {
                x: r1.x + 40,
                y: r1.y + 20,
                ..r1
            };
            g.cursor = (r2.x + r2.w / 2, r2.y + r2.h / 2);
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert!(h.engine.drag.is_some());

        // New torn-off window 201 receives MOVESIZEEND instead of source 101.
        let new_id = h.desktop.0.lock().unwrap().add_window(SimWindow::new(
            201,
            Rect {
                x: 400,
                y: 200,
                w: 800,
                h: 600,
            },
        ));
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, new_id));
        h.settle_quiet();
        assert!(
            h.engine.drag.is_none(),
            "MOVESIZEEND on torn-off window must clear source drag"
        );
        assert!(
            zones(&h).is_empty(),
            "green drop zones must clear after mismatched MOVESIZEEND"
        );
        assert_ne!(tile_color(&h, 102), h.engine.hot.stacked_color);
        h.stop_clean();
    }

    // 2. Unended drag: mouse button pressed during drag and released without MOVESIZEEND.
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
            sim.add_window(SimWindow::new(
                101,
                Rect {
                    x: 0,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.add_window(SimWindow::new(
                102,
                Rect {
                    x: 960,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.foreground = 101;
        });
        let r1 = h.rect(101);
        let r2 = h.rect(102);
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.keys_down.insert(0x01); // VK_LBUTTON down
            g.cursor = (r1.x + 100, r1.y + 15);
        }
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.settle_quiet();
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect = Rect {
                x: r1.x + 80,
                y: r1.y + 20,
                ..r1
            };
            g.cursor = (r2.x + r2.w / 2, r2.y + r2.h / 2);
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert!(h.engine.drag.is_some());
        assert!(h.engine.drag.as_ref().is_some_and(|d| d.target.is_some()));

        // Release VK_LBUTTON without ever delivering EVENT_SYSTEM_MOVESIZEEND.
        h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
        h.desktop
            .0
            .lock()
            .unwrap()
            .windows
            .get_mut(&101)
            .unwrap()
            .in_movesize = Some(false);
        h.engine.tick().unwrap();
        // Fast-forward the release timestamp past the 250 ms grace period.
        if let Some(d) = h.engine.drag.as_mut() {
            d.lbutton_up_since = Some(Instant::now() - Duration::from_millis(300));
            d.movesize_exit_since = Some(Instant::now() - Duration::from_millis(501));
        }
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert!(
            h.engine.drag.is_none(),
            "unended drag must clear once GetGUIThreadInfo confirms not in movesize"
        );
        assert!(
            zones(&h).is_empty(),
            "green drop zones must clear on unended drag recovery"
        );
        h.stop_clean();
    }

    // 3. Dragged window hidden/destroyed mid-drag clears drag session immediately.
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
            sim.add_window(SimWindow::new(
                101,
                Rect {
                    x: 0,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.add_window(SimWindow::new(
                102,
                Rect {
                    x: 960,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.foreground = 101;
        });
        let r1 = h.rect(101);
        h.desktop.0.lock().unwrap().cursor = (r1.x + 100, r1.y + 15);
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.settle_quiet();
        assert!(h.engine.drag.is_some());
        h.desktop
            .0
            .lock()
            .unwrap()
            .windows
            .get_mut(&101)
            .unwrap()
            .visible = false;
        h.engine.event(Event::Window(EVENT_OBJECT_DESTROY, 101));
        h.settle_quiet();
        assert!(
            h.engine.drag.is_none(),
            "destroying dragged window must clear drag session"
        );
        h.stop_clean();
    }
}

#[test]
fn issue_17_late_movesizeend_after_release_preserves_cross_monitor_transfer() {
    exercise_late_release(None, false, false, false);
}
#[test]
fn cancelled_latched_resize_preserves_frame_mismatched_slot_and_neighbour() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    to_start(&mut h, "MON0");
    let key = (1, "MON0".to_owned());
    let lengths = |h: &SimHarness| {
        h.engine.strips[&key]
            .columns
            .iter()
            .map(|c| c.length)
            .collect::<Vec<_>>()
    };
    let before = lengths(&h);
    let original = Rect {
        w: before[0] - 17,
        ..h.rect(101)
    };
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&101)
        .unwrap()
        .rect = original;
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&101)
        .unwrap()
        .rect
        .w += 85;
    h.engine.tick().unwrap();
    assert!(h.engine.drag.as_ref().unwrap().resizing);
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&101)
        .unwrap()
        .rect = original;
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
    h.settle();
    assert_eq!(lengths(&h), before);
    h.stop_clean();
}

#[test]
fn release_before_first_motion_tick_transfers_to_sampled_monitor() {
    exercise_release_tick(false, HookRelease::None);
}

#[test]
fn release_tick_retargets_from_previous_window_and_stacks_on_final_target() {
    exercise_release_tick(true, HookRelease::None);
}

/// #67: the cursor moves on (back onto the previous target) after the button came up and before the first
/// release tick. The drop must use the hook's button-up point, so the window still stacks on the final target.
#[test]
fn release_point_from_hook_wins_over_cursor_moved_before_tick() {
    exercise_release_tick(false, HookRelease::ThenDrift);
    exercise_release_tick(true, HookRelease::ThenDrift);
}

/// #67: a button release recorded before the drag began (an earlier click) is not this drag's drop point; the
/// release tick samples the cursor as before.
#[test]
fn release_point_from_before_drag_is_ignored() {
    exercise_release_tick(true, HookRelease::Stale);
}

#[derive(Clone, Copy, PartialEq)]
enum HookRelease {
    /// No low-level hook sample (test mode): the release tick samples the cursor.
    None,
    /// The hook records the release on the final target, then the cursor drifts to the previous target.
    ThenDrift,
    /// The hook's only sample predates the drag and points at the previous target.
    Stale,
}

fn exercise_release_tick(sample_previous: bool, hook: HookRelease) {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    to_start(&mut h, "MON0");
    to_start(&mut h, "MON1");
    let home = h.rect(101);
    let a = h.rect(102);
    let key = (1, "MON1".to_owned());
    let zone = h.rect(201);
    let a_centre = (a.x + a.w / 2, a.y + a.h / 2);
    let zone_centre = (zone.x + zone.w / 2, zone.y + zone.h / 2);
    if hook == HookRelease::Stale {
        h.desktop.0.lock().unwrap().release = (7, a_centre);
    }
    h.desktop.0.lock().unwrap().keys_down.insert(0x01);
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&101).unwrap().rect.x += 50;
        g.cursor = a_centre;
    }
    if sample_previous {
        h.engine.tick().unwrap();
        assert_eq!(h.engine.drag.as_ref().unwrap().target, Some(102));
    }
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.keys_down.remove(&0x01);
        g.cursor = zone_centre;
        if hook == HookRelease::ThenDrift {
            g.release = (g.release.0 + 1, zone_centre);
            g.cursor = a_centre;
        }
    }
    h.engine.tick().unwrap();
    if hook == HookRelease::ThenDrift {
        let d = h.engine.drag.as_ref().unwrap();
        assert_eq!(
            (d.target, d.destination.as_ref().map(|k| k.1.as_str())),
            (Some(201), Some("MON1")),
            "release tick must aim at the hook's button-up point {zone_centre:?}, not the drifted cursor {a_centre:?}"
        );
    }
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.cursor = (home.x, home.y);
        g.keys_down.insert(0x01);
    }
    h.engine.tick().unwrap();
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
    h.settle();
    let s = &h.engine.strips[&key];
    let (c1, c2) = (s.locate(101).unwrap().0, s.locate(201).unwrap().0);
    assert_eq!(
        c1.abs_diff(c2),
        1,
        "no modifier: tiles beside the target, never stacks (#113)"
    );
    h.stop_clean();
}

#[test]
fn release_movesizeend_immediately_after_loop_exit() {
    exercise_late_release(Some(true), false, false, false);
}
#[test]
fn release_tick_between_loop_exit_and_movesizeend() {
    exercise_late_release(Some(true), true, false, false);
}
#[test]
fn release_destination_survives_later_cursor_move() {
    exercise_late_release(Some(true), true, true, false);
}
#[test]
fn release_missing_movesizeend_expires_after_confirmed_exit() {
    exercise_late_release(Some(true), true, false, true);
}
fn exercise_late_release(in_loop: Option<bool>, tick: bool, move_cursor: bool, expire: bool) {
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
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
    h.settle_quiet();
    // Drag 101 onto MON1.
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&101).unwrap().rect = Rect {
            x: r201.x + 60,
            y: r201.y + 60,
            ..r1
        };
        g.cursor = (r201.x + r201.w / 8, r201.y + r201.h / 8);
    }
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert!(
        h.engine.drag.as_ref().is_some_and(|d| d.target.is_some()),
        "drag aims at a window while holding the mouse button"
    );

    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&101)
        .unwrap()
        .in_movesize = in_loop;
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.engine.tick().unwrap();
    // Simulate 1 second elapsed after release without MOVESIZEEND yet.
    if let Some(d) = h.engine.drag.as_mut() {
        d.lbutton_up_since = Some(Instant::now() - Duration::from_secs(1));
    }
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert!(
        zones(&h).is_empty(),
        "drop visuals must clear promptly after release"
    );
    assert!(
        h.engine.drag.is_some(),
        "drag session must stay alive for late MOVESIZEEND"
    );

    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&101).unwrap().in_movesize = Some(false);
        if move_cursor {
            g.cursor = (r1.x, r1.y);
        }
    }
    if tick {
        h.engine.tick().unwrap();
        assert!(h.engine.drag.is_some());
    }
    if expire {
        let d = h.engine.drag.as_mut().unwrap();
        assert!(d.movesize_exit_since.is_some());
        d.movesize_exit_since = Some(Instant::now() - Duration::from_millis(501));
        h.engine.tick().unwrap();
        assert!(h.engine.drag.is_none());
        h.stop_clean();
        return;
    }
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
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 960,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        // Dialog owned by managed window 102 (NOT the currently active 101).
        let mut dlg = SimWindow::new(
            802,
            Rect {
                x: 1000,
                y: 200,
                w: 400,
                h: 300,
            },
        );
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
    assert_eq!(
        tile_color(&h, 102),
        Some(0x3B82F6),
        "owner 102 must receive the focus highlight"
    );
    assert_eq!(
        tile_color(&h, 101),
        None,
        "previous active tile 101 must lose the focus highlight"
    );
    h.stop_clean();
}

#[test]
fn issue_17_snapped_drag_restore_with_top_edge_anchored_and_off_monitor_latched_resize() {
    // 1. Drag-restore of a snapped window sampled while top edge is still within 2 px of origin
    //    (cur.y == orig.y, cur.h < orig.h, horizontal edges shifted): must classify as `moved`,
    //    NOT `resizing`, and swap on drop!
    {
        let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
            sim.add_window(SimWindow::new(
                101,
                Rect {
                    x: 0,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.add_window(SimWindow::new(
                102,
                Rect {
                    x: 960,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.foreground = 101;
        });
        let r1 = h.rect(101);
        let r2 = h.rect(102);
        h.desktop.0.lock().unwrap().cursor = (r1.x + r1.w / 2, r1.y + 15);
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
        h.settle_quiet();
        // Top edge stays at r1.y + 1 (within 2 px!), height shrinks, AND both X edges shift.
        {
            let mut g = h.desktop.0.lock().unwrap();
            g.windows.get_mut(&101).unwrap().rect = Rect {
                x: r1.x + 150,
                y: r1.y + 1,
                w: r1.w - 200,
                h: r1.h - 200,
            };
            g.cursor = (r2.x + r2.w * 7 / 8, r2.y + r2.h / 2);
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
            sim.add_window(SimWindow::new(
                101,
                Rect {
                    x: 0,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.add_window(SimWindow::new(
                102,
                Rect {
                    x: 960,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
            sim.foreground = 102;
        });
        let r2 = h.rect(102);
        h.desktop.0.lock().unwrap().cursor = (r2.x + r2.w, r2.y + r2.h / 2);
        h.engine
            .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 102));
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

#[test]
fn sim_pad_drag_releases_immediately_on_tip_up_lift_and_resets_gesture_after_timeout() {
    // Regression test for #43 (bundle 001248110-releasing-the-drag-did-not-initially):
    // 1) A 4-finger tip-up lift (ContactFrame::packet(4, [])) must release the drag immediately
    //    instead of hanging for the 800 ms fallback timeout.
    // 2) Holding 4 fingers still on the pad must refresh `pad.seen` so stationary holds do not
    //    falsely time out.
    // 3) If the fallback timeout ever fires `pad_end()`, `self.gesture` must reset so the next
    //    4-finger drag starts cleanly instead of being swallowed by stale `dragging = true`.
    let mut cfg = test_cfg();
    cfg["touchpad"]["three_finger_enabled"] = toml::Value::Boolean(true);
    cfg["touchpad"]["axis_lock_deadzone"] = toml::Value::Integer(12);
    cfg["touchpad"]["swipe_fling_enabled"] = toml::Value::Boolean(false);

    let mut h = SimHarness::with_setup(primary_only_rig(), cfg, |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 960,
                h: 1152,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 960,
                y: 0,
                w: 960,
                h: 1152,
            },
        ));
    });
    focus(&mut h, 101);

    let mut cf = planor::ContactFrame::default();
    let feed = |h: &mut SimHarness,
                cf: &mut planor::ContactFrame,
                count: u32,
                pts: &[(u32, (f64, f64))]| {
        if let Some(frame) = cf.packet_with_slots(count, 5, pts.iter().copied()) {
            h.engine.event(Event::Contacts(frame));
        }
    };

    // Start 4-finger drag on window 101 (frame 2 emits DragStart; frame 3 consumes the dead-zone
    // travel with DragMove; frame 4 with identical coordinates is a true stationary hold).
    feed(
        &mut h,
        &mut cf,
        4,
        &[
            (1, (100.0, 100.0)),
            (2, (120.0, 100.0)),
            (3, (140.0, 100.0)),
            (4, (160.0, 100.0)),
        ],
    );
    feed(
        &mut h,
        &mut cf,
        4,
        &[
            (1, (130.0, 100.0)),
            (2, (150.0, 100.0)),
            (3, (170.0, 100.0)),
            (4, (190.0, 100.0)),
        ],
    );
    feed(
        &mut h,
        &mut cf,
        4,
        &[
            (1, (130.0, 100.0)),
            (2, (150.0, 100.0)),
            (3, (170.0, 100.0)),
            (4, (190.0, 100.0)),
        ],
    );
    assert!(h.engine.pad.is_some(), "4-finger drag should have started");
    assert!(h.engine.drag.is_some());

    // Stationary 4-finger hold: backdate `last_contact` and `seen`, send identical coordinates ->
    // `last_contact` refreshes (preventing false timeout) while `seen` stays untouched for acceleration dt.
    let stale_seen = Instant::now() - Duration::from_millis(750);
    {
        let pad = h.engine.pad.as_mut().unwrap();
        pad.last_contact = stale_seen;
        pad.seen = stale_seen;
    }
    feed(
        &mut h,
        &mut cf,
        4,
        &[
            (1, (130.0, 100.0)),
            (2, (150.0, 100.0)),
            (3, (170.0, 100.0)),
            (4, (190.0, 100.0)),
        ],
    );
    assert!(
        h.engine.pad.as_ref().unwrap().last_contact.elapsed() < Duration::from_millis(200),
        "stationary 4-finger contacts must refresh pad.last_contact"
    );
    assert_eq!(
        h.engine.pad.as_ref().unwrap().seen,
        stale_seen,
        "stationary 4-finger contacts must not overwrite pad.seen"
    );

    // Release all 4 fingers simultaneously (Precision Touchpad HID: count=4, 5 slots, 0 tip-down).
    feed(&mut h, &mut cf, 4, &[]);
    assert!(
        h.engine.pad.is_none(),
        "tip-up lift must immediately clear pad drag"
    );
    assert!(
        h.engine.drag.is_none(),
        "tip-up lift must immediately clear drag session"
    );
    h.settle();

    // Now test timeout recovery: start a second 4-finger drag, let fallback timeout fire without a lift.
    feed(
        &mut h,
        &mut cf,
        4,
        &[
            (1, (100.0, 100.0)),
            (2, (120.0, 100.0)),
            (3, (140.0, 100.0)),
            (4, (160.0, 100.0)),
        ],
    );
    feed(
        &mut h,
        &mut cf,
        4,
        &[
            (1, (130.0, 100.0)),
            (2, (150.0, 100.0)),
            (3, (170.0, 100.0)),
            (4, (190.0, 100.0)),
        ],
    );
    assert!(h.engine.pad.is_some());
    h.engine.pad.as_mut().unwrap().last_contact = Instant::now() - Duration::from_secs(1);
    h.engine.tick().unwrap();
    assert!(h.engine.pad.is_none(), "fallback timeout must end pad drag");
    assert!(
        h.engine.drag.is_none(),
        "fallback timeout must end drag session"
    );
    h.settle();

    // Immediately start a THIRD 4-finger drag without an intervening 1-finger touch:
    // because `self.gesture` was reset on timeout, this must start cleanly.
    feed(
        &mut h,
        &mut cf,
        4,
        &[
            (1, (200.0, 100.0)),
            (2, (220.0, 100.0)),
            (3, (240.0, 100.0)),
            (4, (260.0, 100.0)),
        ],
    );
    feed(
        &mut h,
        &mut cf,
        4,
        &[
            (1, (230.0, 100.0)),
            (2, (250.0, 100.0)),
            (3, (270.0, 100.0)),
            (4, (290.0, 100.0)),
        ],
    );
    assert!(
        h.engine.pad.is_some() && h.engine.drag.is_some(),
        "new 4-finger drag after timeout must start immediately without stale gesture state"
    );
    feed(&mut h, &mut cf, 4, &[]);
    assert!(h.engine.pad.is_none() && h.engine.drag.is_none());
    h.settle();
    h.stop_clean();
}

#[test]
fn sim_five_finger_cancel_preserves_veto_through_staged_lift() {
    // Regression test for Round 1 Blocking #1:
    // 4-finger drag -> ONE 5-finger frame -> moving 4- and 3-finger frames during staged lift
    // must keep `pad`, `drag`, and `pad_scrolling` all off until fewer than 3 fingers remain.
    let mut cfg = test_cfg();
    cfg["touchpad"]["three_finger_enabled"] = toml::Value::Boolean(true);
    cfg["touchpad"]["axis_lock_deadzone"] = toml::Value::Integer(12);
    let mut h = SimHarness::with_setup(primary_only_rig(), cfg, |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 960,
                h: 1152,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 960,
                y: 0,
                w: 960,
                h: 1152,
            },
        ));
    });
    focus(&mut h, 101);

    // Start 4-finger drag.
    h.engine.event(Event::Contacts(vec![
        (100., 100.),
        (120., 100.),
        (140., 100.),
        (160., 100.),
    ]));
    h.engine.event(Event::Contacts(vec![
        (130., 100.),
        (150., 100.),
        (170., 100.),
        (190., 100.),
    ]));
    assert!(h.engine.pad.is_some() && h.engine.drag.is_some());

    // ONE 5-finger frame cancels the drag and engages the gesture veto.
    h.engine.event(Event::Contacts(vec![
        (130., 100.),
        (150., 100.),
        (170., 100.),
        (190., 100.),
        (210., 100.),
    ]));
    assert!(h.engine.pad.is_none() && h.engine.drag.is_none() && !h.engine.pad_scrolling);

    // Staged lift: moving 4-finger frames, then moving 3-finger frames -> MUST stay vetoed!
    for step in 1..=4 {
        let o = step as f64 * 25.0;
        h.engine.event(Event::Contacts(vec![
            (130. + o, 100.),
            (150. + o, 100.),
            (170. + o, 100.),
            (190. + o, 100.),
        ]));
        assert!(
            h.engine.pad.is_none() && h.engine.drag.is_none() && !h.engine.pad_scrolling,
            "4-finger staged lift frame {step} must stay vetoed"
        );
    }
    for step in 1..=4 {
        let o = step as f64 * 25.0;
        h.engine.event(Event::Contacts(vec![
            (230. + o, 100.),
            (250. + o, 100.),
            (270. + o, 100.),
        ]));
        assert!(
            h.engine.pad.is_none() && h.engine.drag.is_none() && !h.engine.pad_scrolling,
            "3-finger staged lift frame {step} must stay vetoed"
        );
    }

    // Lift clears veto; a subsequent 4-finger drag works again.
    h.engine.event(Event::Contacts(vec![]));
    h.engine.event(Event::Contacts(vec![
        (100., 100.),
        (120., 100.),
        (140., 100.),
        (160., 100.),
    ]));
    h.engine.event(Event::Contacts(vec![
        (130., 100.),
        (150., 100.),
        (170., 100.),
        (190., 100.),
    ]));
    assert!(
        h.engine.pad.is_some() && h.engine.drag.is_some(),
        "new drag after full lift must work"
    );
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    h.stop_clean();
}

#[test]
fn sim_pad_drag_acceleration_preserves_dt_across_50ms_gap() {
    // Regression test for Round 1 Blocking #2:
    // Refreshing `last_contact` before `pad_move` must not overwrite `pad.seen` (which computes `dt`).
    // A 50 ms gap before a 10-unit move must produce speed ≈ 10 / 0.05 = 200 -> smoothed ≈ 40,
    // NOT the 0.008 s clamped speed (10 / 0.008 = 1250 -> smoothed = 250).
    let mut cfg = test_cfg();
    cfg["touchpad"]["three_finger_enabled"] = toml::Value::Boolean(true);
    cfg["touchpad"]["axis_lock_deadzone"] = toml::Value::Integer(12);
    cfg["touchpad"]["swipe_drag_gain"] = toml::Value::Float(1.0);
    cfg["touchpad"]["swipe_drag_accel"] = toml::Value::Float(1.0);
    let mut h = SimHarness::with_setup(primary_only_rig(), cfg, |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 960,
                h: 1152,
            },
        ));
    });
    focus(&mut h, 101);

    // Start drag and consume the initial dead-zone DragMove so `gesture.last` is at (160, 100).
    h.engine.event(Event::Contacts(vec![
        (100., 100.),
        (120., 100.),
        (140., 100.),
        (160., 100.),
    ]));
    h.engine.event(Event::Contacts(vec![
        (130., 100.),
        (150., 100.),
        (170., 100.),
        (190., 100.),
    ]));
    h.engine.event(Event::Contacts(vec![
        (130., 100.),
        (150., 100.),
        (170., 100.),
        (190., 100.),
    ]));
    assert!(h.engine.pad.is_some());

    // Backdate `pad.seen` by 50 ms and reset `speed` to 0, then move by +10 units.
    {
        let pad = h.engine.pad.as_mut().unwrap();
        pad.seen = Instant::now() - Duration::from_millis(50);
        pad.speed = 0.0;
    }
    let x_before = h.engine.pad.as_ref().unwrap().cursor.0;
    h.engine.event(Event::Contacts(vec![
        (140., 100.),
        (160., 100.),
        (180., 100.),
        (200., 100.),
    ]));
    let pad = h.engine.pad.as_ref().unwrap();
    // 10 / 0.05 * 0.2 = 40.0 (with minor scheduling jitter, well below 100; 0.008 clamp would be 250).
    assert!(
        pad.speed > 15.0 && pad.speed < 100.0,
        "expected dt-based smoothed speed ~40 for 50ms gap, got {}",
        pad.speed
    );
    let travel = pad.cursor.0 - x_before;
    assert!(
        travel < 11.2,
        "cursor travel with 50ms dt should be ~10.4 (not 12.5 from 8ms clamp), got {travel}"
    );
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    h.stop_clean();
}

#[test]
fn sim_clicking_rightmost_window_during_scroll_settle_keeps_focus_n002() {
    // Regression test for #19 (N-002): clicking the rightmost window while a post-scroll settle
    // timer is pending must focus it immediately and cancel settle so focus is not reverted.
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 400,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            103,
            Rect {
                x: 800,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
    });
    focus(&mut h, 102);

    // Win+wheel scroll starts a pending settle timer with 102 still selected.
    h.engine.event(Event::Wheel {
        delta: -120,
        tilt: false,
        shift: false,
        x: 500,
        y: 500,
    });
    assert!(
        h.engine.settle.is_some(),
        "wheel scroll should arm settle timer"
    );

    // User clicks the rightmost window (103) before settle expires.
    h.desktop.0.lock().unwrap().foreground = 103;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 103));
    assert!(
        h.engine.settle.is_none(),
        "foreground click must cancel pending scroll settle"
    );
    h.settle();

    // Even after ticks run, focus stays on the rightmost window 103.
    h.engine.tick().unwrap();
    h.settle();
    let desk = h.engine.desktop.unwrap();
    let strip = &h.engine.strips[&(desk, "MON0".to_string())];
    assert_eq!(
        strip.active(),
        Some(103),
        "strip focus must stay on rightmost window 103"
    );
    assert_eq!(
        h.desktop.foreground(),
        103,
        "OS foreground must not be reverted away from 103"
    );
    let snap = h.state();
    let tile_103 = snap["tiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"].as_i64() == Some(103))
        .unwrap();
    assert!(
        !tile_103["color"].is_null(),
        "rightmost window 103 should have focus highlight"
    );
    h.stop_clean();
}

#[test]
fn sim_focus_follows_mouse_stationary_cursor_does_not_revert_rightmost_focus() {
    // Regression test for #19 (N-002): with focus_follows_mouse enabled, a stationary cursor over
    // the left window must not steal focus back after the user focuses the rightmost window.
    let mut cfg = test_cfg();
    cfg["layout"]["focus_follows_mouse"] = toml::Value::Boolean(true);
    let mut h = SimHarness::with_setup(primary_only_rig(), cfg, |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 800,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
    });
    // Place cursor over left window 101 and let tick record it.
    let r101 = h.rect(101);
    h.desktop.0.lock().unwrap().cursor = (r101.x + 50, r101.y + 50);
    h.engine.tick().unwrap();
    h.settle();
    assert_eq!(h.desktop.foreground(), 101);

    // Focus the rightmost window 102 while cursor remains stationary over 101.
    focus(&mut h, 102);
    h.engine.tick().unwrap();
    h.settle();
    assert_eq!(
        h.desktop.foreground(),
        102,
        "stationary cursor must not revert focus from rightmost window"
    );

    // Moving the cursor inside 101 should then follow the mouse to 101.
    h.desktop.0.lock().unwrap().cursor = (r101.x + 60, r101.y + 50);
    h.engine.tick().unwrap();
    h.settle();
    assert_eq!(
        h.desktop.foreground(),
        101,
        "moving cursor onto 101 should focus it"
    );
    h.stop_clean();
}

#[test]
fn sim_tick_reconciles_missed_foreground_on_left_app() {
    // Regression test for #40: if OS foreground moves to the left window while EVENT_SYSTEM_FOREGROUND
    // was missed or suppressed, idle tick() must reconcile strip selection and focus highlight.
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 800,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
    });
    focus(&mut h, 102);

    // Simulate OS foreground switching to the left window 101 without delivering EVENT_SYSTEM_FOREGROUND.
    h.desktop.0.lock().unwrap().foreground = 101;
    h.engine.tick().unwrap();
    h.settle();

    let desk = h.engine.desktop.unwrap();
    let strip = &h.engine.strips[&(desk, "MON0".to_string())];
    assert_eq!(
        strip.active(),
        Some(101),
        "tick() must reconcile strip focus to left app 101"
    );
    let snap = h.state();
    let tile_101 = snap["tiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"].as_i64() == Some(101))
        .unwrap();
    let tile_102 = snap["tiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"].as_i64() == Some(102))
        .unwrap();
    assert!(
        !tile_101["color"].is_null(),
        "left app 101 must receive focus border"
    );
    assert!(
        tile_102["color"].is_null(),
        "right app 102 must lose focus border"
    );
    h.stop_clean();
}

#[test]
fn sim_minimize_autofocus_right_then_left_same_monitor() {
    // Regression test for #41: minimizing the focused window on a monitor with other windows
    // focuses the next window to the right, or nearest to the left when rightmost is minimized.
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 600,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 400,
                y: 0,
                w: 600,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            103,
            Rect {
                x: 800,
                y: 0,
                w: 600,
                h: 1000,
            },
        ));
    });
    focus(&mut h, 102);

    // Minimize middle window 102 -> must autofocus 103 (to the right).
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&102)
        .unwrap()
        .iconic = true;
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MINIMIZESTART, 102));
    h.settle();
    let desk = h.engine.desktop.unwrap();
    assert_eq!(
        h.engine.strips[&(desk, "MON0".to_string())].active(),
        Some(103)
    );
    assert_eq!(
        h.desktop.foreground(),
        103,
        "minimizing 102 must autofocus right neighbour 103"
    );

    // Minimize rightmost window 103 -> must autofocus 101 (to the left).
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&103)
        .unwrap()
        .iconic = true;
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MINIMIZESTART, 103));
    h.settle();
    assert_eq!(
        h.engine.strips[&(desk, "MON0".to_string())].active(),
        Some(101)
    );
    assert_eq!(
        h.desktop.foreground(),
        101,
        "minimizing rightmost 103 must autofocus left neighbour 101"
    );
    h.stop_clean();
}

#[test]
fn sim_minimize_only_window_autofocuses_other_monitor_and_handles_empty() {
    // Regression test for #41: minimizing the only active window on a monitor autofocuses an active
    // window on another monitor; when no other monitor has windows, it leaves focus empty cleanly.
    let mut h = SimHarness::with_setup(three_monitor_rig(), test_cfg(), |sim| {
        // MON0: 0..1920, MON1: 1920..3840
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 960,
                h: 1040,
            },
        ));
        sim.add_window(SimWindow::new(
            201,
            Rect {
                x: 1920,
                y: 0,
                w: 960,
                h: 1040,
            },
        ));
        sim.add_window(SimWindow::new(
            202,
            Rect {
                x: 2400,
                y: 0,
                w: 960,
                h: 1040,
            },
        ));
    });
    // Ensure 202 is active on MON1, then focus 101 (only window) on MON0.
    focus(&mut h, 202);
    focus(&mut h, 101);
    assert_eq!(h.engine.active, "MON0");

    // Minimize 101 (only active window on MON0) -> must autofocus 202 on MON1.
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&101)
        .unwrap()
        .iconic = true;
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MINIMIZESTART, 101));
    h.settle();
    assert_eq!(
        h.engine.active, "MON1",
        "active monitor must switch to MON1"
    );
    assert_eq!(
        h.desktop.foreground(),
        202,
        "must autofocus active window 202 on MON1"
    );
    let snap = h.state();
    let tile_202 = snap["tiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"].as_i64() == Some(202))
        .unwrap();
    assert!(
        !tile_202["color"].is_null(),
        "autofocused window 202 on MON1 must have focus border"
    );

    // Minimize 202 (rightmost on MON1) -> must autofocus 201 (left on MON1).
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&202)
        .unwrap()
        .iconic = true;
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MINIMIZESTART, 202));
    h.settle();
    assert_eq!(
        h.desktop.foreground(),
        201,
        "minimizing 202 must autofocus 201 on MON1"
    );

    // Minimize 201 (last active window across all monitors) -> clean empty state, no panic.
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&201)
        .unwrap()
        .iconic = true;
    h.desktop.0.lock().unwrap().foreground = 0;
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MINIMIZESTART, 201));
    h.settle();
    let desk = h.engine.desktop.unwrap();
    assert_eq!(h.engine.strips[&(desk, "MON0".to_string())].active(), None);
    assert_eq!(h.engine.strips[&(desk, "MON1".to_string())].active(), None);
    h.stop_clean();
}

#[test]
fn sim_wheel_and_inertia_survive_foreground_echo_for_already_active_window() {
    // Review regression test (#19/#40): a foreground event for the already-active window (or a
    // self-activated echo) must NOT cancel active wheel settle or touchpad inertia.
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 400,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            103,
            Rect {
                x: 800,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
    });
    focus(&mut h, 102);

    // Wheel scroll arms `settle`; also arm `inertia` to verify both survive.
    h.engine.event(Event::Wheel {
        delta: -120,
        tilt: false,
        shift: false,
        x: 500,
        y: 500,
    });
    h.engine.inertia = Some((false, 300.0, Instant::now(), Some(96.0)));
    assert!(h.engine.settle.is_some());
    assert!(h.engine.inertia.is_some());

    let desk = h.engine.desktop.unwrap();
    let active_id = h.engine.strips[&(desk, "MON0".to_string())]
        .active()
        .unwrap();
    h.desktop.0.lock().unwrap().foreground = active_id;
    h.engine
        .event(Event::Window(EVENT_SYSTEM_FOREGROUND, active_id));

    assert!(
        h.engine.settle.is_some(),
        "foreground echo for already-active window must not cancel settle"
    );
    assert!(
        h.engine.inertia.is_some(),
        "foreground echo for already-active window must not cancel inertia"
    );

    // Self-activated target echo also preserves scroll state.
    h.engine.self_activated = Some((103, Instant::now()));
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 103));
    assert!(h.engine.self_activated.is_none());
    assert!(
        h.engine.settle.is_some(),
        "self-activated echo must not cancel settle"
    );
    assert!(
        h.engine.inertia.is_some(),
        "self-activated echo must not cancel inertia"
    );
    h.stop_clean();
}

#[test]
fn sim_hide_on_parked_focused_window_does_not_remove_or_steal_focus() {
    // Review regression test (#41): EVENT_OBJECT_HIDE arriving for a window that Planor itself
    // has parked off-screen (while it is still OS foreground) must not evict it or steal focus.
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 400,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            103,
            Rect {
                x: 800,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
    });
    // Focus 103 so 101 is clipped/parked off-screen at park_x, while OS foreground still points at 101.
    focus(&mut h, 103);
    let desk = h.engine.desktop.unwrap();
    assert!(!h.engine.strips[&(desk, "MON0".to_string())].in_view(101));

    {
        let mut sim = h.desktop.0.lock().unwrap();
        sim.foreground = 101;
        sim.windows.get_mut(&101).unwrap().visible = false;
    }
    h.engine.event(Event::Window(EVENT_OBJECT_HIDE, 101));
    h.settle();

    assert!(
        h.engine.strips[&(desk, "MON0".to_string())]
            .locate(101)
            .is_some(),
        "EVENT_OBJECT_HIDE on a parked window must not remove it from its strip"
    );
    assert_eq!(
        h.engine.strips[&(desk, "MON0".to_string())].active(),
        Some(103),
        "EVENT_OBJECT_HIDE on a parked window must not alter strip focus"
    );
    assert_eq!(
        h.desktop.foreground(),
        101,
        "EVENT_OBJECT_HIDE on a parked window must not steal OS focus"
    );

    // A genuine close (EVENT_OBJECT_DESTROY) still removes the parked window cleanly.
    h.engine.event(Event::Window(EVENT_OBJECT_DESTROY, 101));
    h.settle();
    assert!(
        h.engine.strips[&(desk, "MON0".to_string())]
            .locate(101)
            .is_none(),
        "EVENT_OBJECT_DESTROY must still remove a parked window"
    );
    h.stop_clean();
}

#[test]
fn sim_minimize_only_window_autofocuses_spatially_nearest_monitor_and_reveals() {
    // Review regression test (#41): when multiple other monitors have active windows, minimizing
    // the only window on MON0 picks the spatially nearest monitor (MON2 over MON1, even though
    // MON1 appears earlier in `displays`), updates `last_foreground`, and reveals an off-view target.
    let mut h = SimHarness::with_setup(three_monitor_rig(), test_cfg(), |sim| {
        // MON0: center (960, 576); MON1: center (3200, 696); MON2: center (-720, 1256).
        // Distance(MON0, MON2)^2 < Distance(MON0, MON1)^2.
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 960,
                h: 1040,
            },
        ));
        sim.add_window(SimWindow::new(
            201,
            Rect {
                x: 1920,
                y: 0,
                w: 960,
                h: 1040,
            },
        ));
        sim.add_window(SimWindow::new(
            301,
            Rect {
                x: -1440,
                y: 0,
                w: 1440,
                h: 1200,
            },
        ));
        sim.add_window(SimWindow::new(
            302,
            Rect {
                x: -1440,
                y: 400,
                w: 1440,
                h: 1200,
            },
        ));
        sim.add_window(SimWindow::new(
            303,
            Rect {
                x: -1440,
                y: 800,
                w: 1440,
                h: 1200,
            },
        ));
    });
    // Select 301 on MON2, then scroll MON2's strip so 301 is off-view/parked.
    focus(&mut h, 301);
    let desk = h.engine.desktop.unwrap();
    {
        let s = h
            .engine
            .strips
            .get_mut(&(desk, "MON2".to_string()))
            .unwrap();
        s.scroll(2000);
        assert!(
            !s.in_view(301),
            "301 should be scrolled off-view before minimize"
        );
    }
    focus(&mut h, 101);
    assert_eq!(h.engine.active, "MON0");

    // Minimize 101 (only window on MON0) -> must pick spatially nearest MON2 and reveal 301.
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&101)
        .unwrap()
        .iconic = true;
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MINIMIZESTART, 101));
    assert_eq!(
        h.engine.active, "MON2",
        "must pick spatially nearest monitor MON2 over MON1"
    );
    assert_eq!(
        h.engine.last_foreground, 301,
        "last_foreground must update to autofocused window"
    );
    assert_eq!(
        h.desktop.foreground(),
        301,
        "OS foreground must activate MON2's active window 301"
    );
    assert!(
        h.engine.strips[&(desk, "MON2".to_string())].in_view(301),
        "autofocused off-view window 301 on MON2 must be revealed immediately"
    );
    h.settle();
    h.stop_clean();
}

#[test]
fn sim_minimize_grouped_window_autofocuses_sibling_then_right_column() {
    // Review regression test (#41): minimizing an active window in a stacked group autofocuses
    // its sibling inside the group; minimizing the last member autofocuses the column to the right.
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 400,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            103,
            Rect {
                x: 800,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
    });
    let desk = h.engine.desktop.unwrap();
    {
        let s = h
            .engine
            .strips
            .get_mut(&(desk, "MON0".to_string()))
            .unwrap();
        s.group(102, 101);
        assert_eq!(
            s.columns.len(),
            2,
            "101 and 102 should share column 0 with 103 in column 1"
        );
    }
    focus(&mut h, 101);

    // Minimize 101 -> autofocuses sibling 102 in the same group.
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&101)
        .unwrap()
        .iconic = true;
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MINIMIZESTART, 101));
    h.settle();
    assert_eq!(
        h.engine.strips[&(desk, "MON0".to_string())].active(),
        Some(102)
    );
    assert_eq!(
        h.desktop.foreground(),
        102,
        "minimizing grouped 101 must autofocus sibling 102"
    );
    assert_eq!(h.engine.last_foreground, 102);

    // Minimize 102 (now alone in column 0) -> autofocuses column 1 to the right (103).
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&102)
        .unwrap()
        .iconic = true;
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MINIMIZESTART, 102));
    h.settle();
    assert_eq!(
        h.engine.strips[&(desk, "MON0".to_string())].active(),
        Some(103)
    );
    assert_eq!(
        h.desktop.foreground(),
        103,
        "minimizing last group member 102 must autofocus right column 103"
    );
    h.stop_clean();
}

#[test]
fn sim_rapid_hotkey_walk_survives_async_os_foreground_lag() {
    // Regression test for live scn_focus failures: during a rapid hotkey walk (e.g. Alt+Left),
    // if OS GetForegroundWindow() temporarily lags behind activate_window() across a tick() or
    // subsequent hotkey press, neither tick() nor keypress() may revert strip.active() back to
    // the stale window; once the pending activation TTL expires, genuine external foreground
    // divergence still reconciles cleanly.
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 400,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            103,
            Rect {
                x: 800,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
    });
    focus(&mut h, 103);
    let desk = h.engine.desktop.unwrap();

    // Step 1: Alt+Left moves strip focus 103 -> 102, then OS foreground lags on 103 when tick() runs.
    h.cmd(Command::Key {
        name: "focus_left".into(),
    });
    assert_eq!(
        h.engine.strips[&(desk, "MON0".to_string())].active(),
        Some(102)
    );
    assert!(h.engine.self_activated.is_some_and(|(id, _)| id == 102));
    h.desktop.0.lock().unwrap().foreground = 103;
    h.engine.tick().unwrap();
    assert_eq!(
        h.engine.strips[&(desk, "MON0".to_string())].active(),
        Some(102),
        "tick() must not revert strip focus back to stale lagging OS foreground 103"
    );

    // Step 2: Rapid second Alt+Left while OS foreground still lags on 103 must continue 102 -> 101.
    h.cmd(Command::Key {
        name: "focus_left".into(),
    });
    assert_eq!(
        h.engine.strips[&(desk, "MON0".to_string())].active(),
        Some(101),
        "rapid second focus_left must walk 102 -> 101 instead of reverting to lagging 103"
    );
    h.settle();

    // Step 3: Once the pending self-activation TTL expires, tick() reconciles genuine OS divergence.
    h.engine.self_activated = Some((101, Instant::now() - Duration::from_millis(300)));
    h.desktop.0.lock().unwrap().foreground = 103;
    h.engine.tick().unwrap();
    assert!(
        h.engine.self_activated.is_none(),
        "stale self_activated must expire after TTL"
    );
    assert_eq!(
        h.engine.strips[&(desk, "MON0".to_string())].active(),
        Some(103),
        "after self_activated TTL expires, tick() must reconcile OS foreground 103"
    );
    h.settle();
    h.stop_clean();
}

#[test]
fn sim_pull_orphans_keeps_both_orphans_in_view_across_foreground_reconcile() {
    // Regression test for #73 (scn_display::offline_15s_round_trip / short_blip_3s):
    // When MON0 has 3 windows [101, 102 (focused), 103] and MON1/MON2 go offline leaving
    // two orphans (201, 301), PullOrphans must append them at the end of MON0's strip (so
    // no trailing peek clips the first orphan off-screen) AND activate the newly focused
    // rescued orphan (so tick() foreground reconciliation does not yank the camera back to 102).
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 100,
                y: 100,
                w: 300,
                h: 300,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 600,
                y: 100,
                w: 300,
                h: 300,
            },
        ));
        sim.add_window(SimWindow::new(
            103,
            Rect {
                x: 1000,
                y: 100,
                w: 300,
                h: 300,
            },
        ));
        sim.add_window(SimWindow::new(
            201,
            Rect {
                x: 2000,
                y: 100,
                w: 300,
                h: 300,
            },
        ));
        sim.add_window(SimWindow::new(
            301,
            Rect {
                x: -1300,
                y: 100,
                w: 300,
                h: 300,
            },
        ));
    });
    let desk = h.engine.desktop.unwrap();
    let mon0 = (desk, "MON0".to_string());

    // Disconnect MON1 & MON2, then focus middle primary window 102 (matching scn_display).
    h.desktop.0.lock().unwrap().monitors = primary_only_rig();
    h.engine.event(Event::Topology);
    h.tick_now();
    focus(&mut h, 102);

    // Pull orphans onto MON0.
    h.cmd(Command::PullOrphans);
    h.settle();
    // Run tick() so foreground reconciliation executes with the post-PullOrphans state.
    h.engine.tick().unwrap();
    h.settle();

    let strip = &h.engine.strips[&mon0];
    assert!(
        strip.in_view(201),
        "orphan 201 must be fully in-view on MON0 after PullOrphans + tick()"
    );
    assert!(
        strip.in_view(301),
        "orphan 301 must be fully in-view on MON0 after PullOrphans + tick()"
    );
    assert_eq!(
        strip.active(),
        Some(301),
        "last pulled orphan 301 must remain active"
    );
    assert_eq!(
        h.desktop.foreground(),
        301,
        "PullOrphans must activate the focused rescued orphan"
    );
    h.stop_clean();
}

#[test]
fn sim_scanned_window_not_snapped_back_by_newborn_guard_while_shown_newborn_is() {
    // Regression test for #73 (scn_scroll::resize_then_scroll):
    // Pre-existing windows tiled during scan() are not newborns and must not be snapped back
    // by hold_newborn when resized via SetWindowPos shortly after startup, whereas genuinely
    // newly shown windows (EVENT_OBJECT_SHOW) still receive newborn-position enforcement.
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 400,
                h: 400,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 500,
                y: 0,
                w: 400,
                h: 400,
            },
        ));
    });
    h.settle();

    // External resize of pre-existing scanned window 102 immediately after startup.
    let resized = Rect {
        x: 0,
        y: 0,
        w: 1344,
        h: 1040,
    };
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&102)
        .unwrap()
        .rect = resized;
    h.engine.dirty = false;
    h.engine.force_place = false;
    h.engine
        .event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, 102));
    assert!(
        !h.engine.force_place && !h.engine.dirty,
        "pre-existing window 102 from scan() must not trigger newborn snap-back"
    );

    // Genuinely newly shown window 201 via EVENT_OBJECT_SHOW MUST still trigger hold_newborn.
    h.desktop.0.lock().unwrap().add_window(SimWindow::new(
        201,
        Rect {
            x: 200,
            y: 200,
            w: 400,
            h: 400,
        },
    ));
    h.engine.event(Event::Window(EVENT_OBJECT_SHOW, 201));
    h.settle();
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&201)
        .unwrap()
        .rect = Rect {
        x: 100,
        y: 100,
        w: 500,
        h: 500,
    };
    h.engine.dirty = false;
    h.engine.force_place = false;
    h.engine
        .event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, 201));
    assert!(
        h.engine.force_place && h.engine.dirty,
        "newly shown window 201 must still trigger newborn snap-back"
    );
    h.stop_clean();
}

#[test]
fn sim_three_finger_scroll_autofocus_default_off_and_opt_in() {
    // Verify planor.example.toml / settings_model exposes [touchpad].scroll_autofocus = false by default (#76).
    let mut model = crate::settings_model::Model::from_text(
        std::path::Path::new("planor.toml"),
        crate::EXAMPLE,
    )
    .expect("load settings model");
    let idx = model
        .entries
        .iter()
        .position(|e| e.section == "touchpad" && e.key == "scroll_autofocus")
        .expect("touchpad.scroll_autofocus must exist in settings model");
    assert_eq!(model.value(idx), toml::Value::Boolean(false));
    assert!(
        !model.entries[idx].help.is_empty(),
        "scroll_autofocus must have help text"
    );
    assert!(model.set(idx, toml::Value::Boolean(true)).is_ok());
    assert_eq!(model.value(idx), toml::Value::Boolean(true));

    // Helper: perform a 3-finger horizontal scroll leftwards (natural_direction = true -> advances strip right).
    let do_three_finger_scroll = |h: &mut SimHarness, steps: usize, step_dx: f64| {
        let mut x = 500.0;
        h.engine.event(Event::Contacts(vec![
            (x, 100.0),
            (x + 20.0, 100.0),
            (x + 40.0, 100.0),
        ]));
        for _ in 0..steps {
            x += step_dx;
            h.engine.event(Event::Contacts(vec![
                (x, 100.0),
                (x + 20.0, 100.0),
                (x + 40.0, 100.0),
            ]));
            h.settle();
        }
        // Lift fingers -> Swipe::ScrollEnd.
        h.engine.event(Event::Contacts(vec![]));
        // Force any settle deadline to expire and tick several times.
        h.engine.inertia = None;
        if h.engine.settle.is_some() {
            h.engine.settle = Some(Instant::now() - Duration::from_millis(1));
        }
        for _ in 0..4 {
            h.engine.tick().unwrap();
        }
    };

    // (a) Default config: scroll_autofocus is false -> 3-finger scroll moves view without stealing OS focus.
    let mut cfg_default = test_cfg();
    cfg_default["touchpad"]["three_finger_enabled"] = toml::Value::Boolean(true);
    cfg_default["touchpad"]["swipe_natural_direction"] = toml::Value::Boolean(true);
    cfg_default["touchpad"]["swipe_scroll_gain"] = toml::Value::Float(4.0);
    cfg_default["touchpad"]["swipe_inertia"] = toml::Value::Boolean(false);
    let mut h = SimHarness::with_setup(primary_only_rig(), cfg_default, |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 400,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            103,
            Rect {
                x: 800,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
    });
    assert!(
        !h.engine.hot.scroll_autofocus,
        "scroll_autofocus must default to false"
    );
    focus(&mut h, 101);
    let desk = h.engine.desktop.unwrap();
    let key = (desk, "MON0".to_string());
    assert_eq!(h.desktop.foreground(), 101);

    do_three_finger_scroll(&mut h, 10, -25.0);
    let scrolled_active = h.engine.strips[&key].active();
    assert_ne!(
        scrolled_active,
        Some(101),
        "3-finger scroll should advance the strip view"
    );
    assert_eq!(
        h.desktop.foreground(),
        101,
        "default scroll_autofocus=false must NOT steal OS foreground on 3-finger scroll"
    );
    // Subsequent ticks must NOT snap the strip back to 101.
    for _ in 0..5 {
        h.engine.tick().unwrap();
    }
    assert_eq!(
        h.engine.strips[&key].active(),
        scrolled_active,
        "tick() foreground reconciliation must hold the scrolled view until OS focus actually changes"
    );

    // Genuine OS foreground change to 103 clears scroll_hold_fg and reconciles normally.
    h.desktop.0.lock().unwrap().foreground = 103;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 103));
    h.settle();
    assert_eq!(h.engine.strips[&key].active(), Some(103));
    assert_eq!(h.engine.scroll_hold_fg, None);
    h.stop_clean();

    // (b) Opt-in config: scroll_autofocus = true -> 3-finger scroll focuses the settled window.
    let mut cfg_auto = test_cfg();
    cfg_auto["touchpad"]["three_finger_enabled"] = toml::Value::Boolean(true);
    cfg_auto["touchpad"]["swipe_natural_direction"] = toml::Value::Boolean(true);
    cfg_auto["touchpad"]["swipe_scroll_gain"] = toml::Value::Float(4.0);
    cfg_auto["touchpad"]["swipe_inertia"] = toml::Value::Boolean(false);
    cfg_auto["touchpad"]["scroll_autofocus"] = toml::Value::Boolean(true);
    let mut h2 = SimHarness::with_setup(primary_only_rig(), cfg_auto, |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 400,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            103,
            Rect {
                x: 800,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
    });
    assert!(h2.engine.hot.scroll_autofocus);
    focus(&mut h2, 101);
    let desk2 = h2.engine.desktop.unwrap();
    let key2 = (desk2, "MON0".to_string());
    do_three_finger_scroll(&mut h2, 10, -25.0);
    let settled = h2.engine.strips[&key2].active().unwrap();
    assert_ne!(settled, 101);
    assert_eq!(
        h2.desktop.foreground(),
        settled,
        "scroll_autofocus=true must activate the settled window"
    );
    h2.stop_clean();
}

/// Four fingers at `(x, y)` (centroid-ish: a row of four points 20 apart).
fn four(x: f64, y: f64) -> Vec<(f64, f64)> {
    (0..4).map(|i| (x + 20. * i as f64, y)).collect()
}

/// Run a 4-finger drag from the origin by `steps` of `(dx, dy)`; returns (picked up, cursor travel) and lifts.
fn axis_lock_drag(lock: Option<bool>, focus_id: isize, step: (f64, f64)) -> (bool, (f64, f64)) {
    let mut cfg = note_cfg();
    cfg["touchpad"]["swipe_fling_enabled"] = toml::Value::Boolean(false);
    if let Some(v) = lock {
        cfg["touchpad"]["drag_axis_lock"] = toml::Value::Boolean(v);
    }
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
    focus(&mut h, focus_id);
    h.engine.event(Event::Contacts(four(500., 500.)));
    let mut start = None;
    let mut picked = false;
    for i in 1..=8 {
        h.engine.event(Event::Contacts(four(
            500. + step.0 * i as f64,
            500. + step.1 * i as f64,
        )));
        if let Some(p) = h.engine.pad.as_ref() {
            if !picked {
                picked = true;
                start = Some(p.cursor);
            }
        }
    }
    let travel = match (start, h.engine.pad.as_ref()) {
        (Some(s), Some(p)) => (p.cursor.0 - s.0, p.cursor.1 - s.1),
        _ => (0., 0.),
    };
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    (picked, travel)
}

#[test]
fn axis_lock_is_on_by_default_and_listed_in_settings() {
    let cfg = test_cfg();
    assert_eq!(cfg["touchpad"]["drag_axis_lock"].as_bool(), Some(true));
    assert!(EXAMPLE.contains("drag_axis_lock = true"));
}

#[test]
fn axis_lock_ignores_cross_axis_start_and_keeps_carry_on_axis() {
    // Landscape (MON0, window 101): the strip runs sideways. Portrait (MON2, window 301): up and down.
    for (win, along, cross, name) in [
        (101, (30., 0.), (0., 30.), "landscape"),
        (301, (0., 30.), (30., 0.), "portrait"),
    ] {
        let (picked, _) = axis_lock_drag(None, win, cross);
        assert!(
            !picked,
            "{name}: a cross-axis start must not pick the window up"
        );
        let (picked, t) = axis_lock_drag(None, win, along);
        assert!(picked, "{name}: an along-axis start picks the window up");
        let (cx, cy) = (t.0.abs(), t.1.abs());
        if along.0 != 0. {
            assert!(
                cx > 0. && cy == 0.,
                "{name}: carry stays on the axis, moved ({cx}, {cy})"
            );
        } else {
            assert!(
                cy > 0. && cx == 0.,
                "{name}: carry stays on the axis, moved ({cx}, {cy})"
            );
        }
        // Along start, then a diagonal wobble: still no cross-axis movement.
        let (picked, t) =
            axis_lock_drag(None, win, (along.0 + cross.0 / 3., along.1 + cross.1 / 3.));
        assert!(picked, "{name}: a start within the 1.5x lead picks up");
        let across_travel = if along.0 != 0. { t.1 } else { t.0 };
        assert_eq!(
            across_travel, 0.,
            "{name}: the diagonal wobble leaves no cross-axis travel"
        );
    }
}

#[test]
fn axis_lock_off_restores_free_drag() {
    for (win, cross, name) in [(101, (0., 30.), "landscape"), (301, (30., 0.), "portrait")] {
        let (picked, t) = axis_lock_drag(Some(false), win, cross);
        assert!(
            picked,
            "{name}: with the lock off a cross-axis start picks the window up"
        );
        assert!(t.0.abs() + t.1.abs() > 0., "{name}: and it moves freely");
    }
}

/// Feed four-finger reports along a list of per-report `(dx, dy)` finger steps from (500, 500).
fn feed_steps(h: &mut SimHarness, steps: &[(f64, f64)]) {
    let (mut x, mut y) = (500., 500.);
    h.engine.event(Event::Contacts(four(x, y)));
    for s in steps {
        x += s.0;
        y += s.1;
        h.engine.event(Event::Contacts(four(x, y)));
    }
}

#[test]
fn axis_lock_still_lets_a_sideways_throw_leave_the_portrait_monitor() {
    let mut cfg = note_cfg();
    cfg["touchpad"]["swipe_fling_enabled"] = toml::Value::Boolean(true);
    cfg["touchpad"]["swipe_fling_speed"] = toml::Value::Integer(100);
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
    focus(&mut h, 301);
    let mut steps = vec![(0., 5.); 4];
    steps.extend(vec![(10., 0.); 6]);
    feed_steps(&mut h, &mut steps);
    let samples_x: f64 = h
        .engine
        .pad
        .as_ref()
        .expect("carrying")
        .samples
        .iter()
        .map(|s| s.1)
        .sum();
    assert!(
        samples_x > 0.,
        "fling samples keep the raw sideways finger travel ({samples_x})"
    );
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert_eq!(
        h.engine
            .strips
            .iter()
            .find(|(_, s)| s.locate(301).is_some())
            .map(|(k, _)| k.clone())
            .map(|k| k.1),
        Some("MON0".to_string()),
        "a sideways throw crosses to MON0"
    );
}

#[test]
fn axis_lock_does_not_apply_over_the_windows_own_group() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    for (id, mon) in [(102, "MON0"), (302, "MON2")] {
        focus(&mut h, id);
        h.cmd(Command::Key {
            name: "stack_toggle".into(),
        });
        h.settle();
        let key = (1, mon.to_string());
        let st = &h.engine.strips[&key];
        assert!(
            st.locate(id)
                .is_some_and(|(c, _)| st.columns[c].members.len() > 1),
            "{mon}: {id} is in a group"
        );
        let horizontal = st.area.horizontal();
        let along = if horizontal { (6., 0.) } else { (0., 6.) };
        let across = if horizontal { (0., 2.) } else { (2., 0.) };
        // Start along the strip, then move across it a little (below the release threshold).
        let (mut x, mut y) = (500., 500.);
        h.engine.event(Event::Contacts(four(x, y)));
        for _ in 0..4 {
            x += along.0;
            y += along.1;
            h.engine.event(Event::Contacts(four(x, y)));
        }
        let before = h.engine.pad.as_ref().expect("carrying").cursor;
        for _ in 0..4 {
            x += across.0;
            y += across.1;
            h.engine.event(Event::Contacts(four(x, y)));
        }
        let p = h.engine.pad.as_ref().expect("carrying");
        assert!(
            p.lock.is_some(),
            "{mon}: a small cross movement does not release the lock"
        );
        let moved = if horizontal {
            p.cursor.1 - before.1
        } else {
            p.cursor.0 - before.0
        };
        assert!(
            moved > 0.,
            "{mon}: over its own group the window follows the fingers across the axis ({moved})"
        );
        h.engine.event(Event::Contacts(vec![]));
        h.settle();
    }
}

/// Group `ids` into one column on the monitor `mon` (focus the first, then stack each next one onto it).
fn make_group(h: &mut SimHarness, mon: &str, ids: &[isize]) {
    focus(h, ids[0]);
    for _ in 1..ids.len() {
        h.cmd(Command::Key {
            name: "stack_toggle".into(),
        });
        h.settle();
    }
    let st = &h.engine.strips[&(1, mon.to_string())];
    assert!(
        st.locate(ids[0])
            .is_some_and(|(c, _)| st.columns[c].members.len() >= 2),
        "{mon}: grouped"
    );
}

fn cursor_of(h: &SimHarness) -> (f64, f64) {
    h.engine.pad.as_ref().expect("carrying").cursor
}

#[test]
fn axis_lock_holds_outside_the_group_column_and_returns_to_the_line() {
    for (mon, id, along, across) in [
        ("MON0", 101, (6., 0.), (0., 6.)),
        ("MON2", 301, (0., 6.), (6., 0.)),
    ] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        make_group(&mut h, mon, &[id, id + 1]);
        let horizontal = mon == "MON0";
        let cross_of = |c: (f64, f64)| if horizontal { c.1 } else { c.0 };
        let (mut x, mut y) = (500., 500.);
        h.engine.event(Event::Contacts(four(x, y)));
        for _ in 0..3 {
            x += along.0;
            y += along.1;
            h.engine.event(Event::Contacts(four(x, y)));
        }
        let line = h.engine.pad.as_ref().expect("carrying").line;
        // Far along the strip: well outside the column; wobble across with every step.
        for _ in 0..12 {
            x += along.0;
            y += along.1;
            h.engine.event(Event::Contacts(four(x, y)));
            x += across.0 / 3.;
            y += across.1 / 3.;
            h.engine.event(Event::Contacts(four(x, y)));
        }
        // The return onto the line eases over a few frames (#84); a few more steps along the strip finish it.
        for _ in 0..8 {
            x += along.0;
            y += along.1;
            h.engine.event(Event::Contacts(four(x, y)));
        }
        let p = h.engine.pad.as_ref().expect("carrying");
        assert!(p.lock.is_some(), "{mon}: lock still on");
        assert_eq!(
            cross_of(p.cursor),
            line,
            "{mon}: outside its column the carry is back on the pick-up line"
        );
        h.engine.event(Event::Contacts(vec![]));
        h.settle();
    }
}

#[test]
fn axis_lock_pad_drag_swaps_with_a_sibling_across_the_axis() {
    for (mon, ids, along, across) in [
        ("MON0", [101, 102], (6., 0.), (0., 8.)),
        ("MON2", [301, 302], (0., 6.), (8., 0.)),
    ] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        make_group(&mut h, mon, &ids);
        let key = (1, mon.to_string());
        let before = h.engine.strips[&key]
            .columns
            .iter()
            .find(|c| c.members.contains(&ids[0]))
            .unwrap()
            .members
            .clone();
        focus(&mut h, ids[0]);
        let (mut x, mut y) = (500., 500.);
        h.engine.event(Event::Contacts(four(x, y)));
        for _ in 0..3 {
            x += along.0;
            y += along.1;
            h.engine.event(Event::Contacts(four(x, y)));
        }
        let start = cursor_of(&h);
        // Across the strip, over the group column: the carry follows (a sibling's strip peeks that way).
        // (Push until the drag targets the sibling: swap mode.)
        // The sibling peeks on one side of the column: head for it.
        let sib = h.engine.strips[&key]
            .rects(96)
            .into_iter()
            .find(|r| r.0 == ids[1])
            .unwrap()
            .1;
        let sib_c = if mon == "MON0" {
            sib.y + sib.h / 2
        } else {
            sib.x + sib.w / 2
        } as f64;
        let sign = if sib_c > if mon == "MON0" { start.1 } else { start.0 } {
            1.
        } else {
            -1.
        };
        for _ in 0..60 {
            if h.engine
                .drag
                .as_ref()
                .is_some_and(|d| d.mode == planor::Drag::Swap)
            {
                break;
            }
            x += across.0 * sign;
            y += across.1 * sign;
            h.engine.event(Event::Contacts(four(x, y)));
            thread::sleep(Duration::from_millis(10));
            h.engine.tick().unwrap();
        }
        assert_eq!(
            h.engine.drag.as_ref().map(|d| d.mode),
            Some(planor::Drag::Swap),
            "{mon}: over the sibling the drag swaps"
        );
        let end = cursor_of(&h);
        assert!(
            h.engine.pad.as_ref().unwrap().lock.is_some(),
            "{mon}: lock not released"
        );
        let moved = if mon == "MON0" {
            end.1 - start.1
        } else {
            end.0 - start.0
        };
        assert!(
            moved.abs() > 1.,
            "{mon}: carry follows across the axis over its own group ({moved})"
        );
        h.engine.event(Event::Contacts(vec![]));
        h.settle();
        let after = h
            .engine
            .strips
            .iter()
            .find_map(|(_, s)| s.columns.iter().find(|c| c.members.contains(&ids[0])))
            .unwrap()
            .members
            .clone();
        let mut swapped = before.clone();
        swapped.reverse();
        assert_eq!(
            after, swapped,
            "{mon}: the pad drag swapped the window with its sibling ({before:?} -> {after:?})"
        );
        assert!(
            h.engine.pad.is_none() && h.engine.drag.is_none(),
            "{mon}: dropped cleanly"
        );
    }
}

#[test]
fn axis_lock_keeps_the_lock_on_a_side_with_no_monitor_and_ignores_a_flick_there() {
    let cfg = note_cfg();
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
    focus(&mut h, 102);
    let (mut x, mut y) = (500., 500.);
    h.engine.event(Event::Contacts(four(x, y)));
    for _ in 0..3 {
        x += 6.;
        h.engine.event(Event::Contacts(four(x, y)));
    }
    let start = cursor_of(&h);
    // MON0 has monitors left (MON2) and right (MON1) only: 150 units straight down must not release the lock.
    for _ in 0..30 {
        y += 5.;
        h.engine.event(Event::Contacts(four(x, y)));
    }
    let p = h.engine.pad.as_ref().expect("carrying");
    assert!(p.lock.is_some(), "no monitor below MON0: the lock stays");
    assert_eq!(p.cursor.1, start.1, "no vertical travel");
    // A fast flick down (no monitor that way) is a harmless no-op: the window is still on MON0 afterwards.
    for _ in 0..4 {
        y += 40.;
        h.engine.event(Event::Contacts(four(x, y)));
    }
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert!(h.engine.pad.is_none() && h.engine.drag.is_none());
    let on = h
        .engine
        .strips
        .iter()
        .find(|(_, s)| s.locate(102).is_some())
        .map(|(k, _)| k.1.clone());
    assert_eq!(on.as_deref(), Some("MON0"), "window stays on MON0");
}

fn drag_group_beyond_border(
    rig: Vec<(String, Rect, Rect)>,
    ids: (isize, isize),
    mon: &str,
    horizontal: bool,
) -> Vec<Vec<isize>> {
    let (a, b) = ids;
    let r = if horizontal {
        Rect {
            x: 0,
            y: 0,
            w: 800,
            h: 1000,
        }
    } else {
        Rect {
            x: -1440,
            y: 0,
            w: 1440,
            h: 800,
        }
    };
    let mut h = SimHarness::with_setup(rig, test_cfg(), |sim| {
        sim.add_window(SimWindow::new(a, r));
        sim.add_window(SimWindow::new(b, r));
    });
    let key = (h.engine.desktop.unwrap(), mon.to_string());
    h.engine.strips.get_mut(&key).unwrap().group(a, b);
    h.engine.strips.get_mut(&key).unwrap().columns[0].length = 800;
    h.engine.dirty = true;
    h.settle();
    let area = h.engine.strips[&key].area;
    let col = crate::daemon::group_column_rect(&h.engine.strips[&key], a, 96);
    let (cx, cy) = (col.x + col.w / 2, col.y + col.h / 2);
    h.desktop.0.lock().unwrap().cursor = (cx, cy);
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, a));
    {
        let mut sim = h.desktop.0.lock().unwrap();
        if horizontal {
            sim.windows.get_mut(&a).unwrap().rect.x += 40
        } else {
            sim.windows.get_mut(&a).unwrap().rect.y += 40
        }
        sim.cursor = if horizontal {
            (area.x + area.w - 20, cy)
        } else {
            (cx, area.y + area.h - 20)
        };
    }
    h.engine.tick().unwrap();
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, a));
    h.settle();
    let out = h.engine.strips[&key]
        .columns
        .iter()
        .map(|c| c.members.clone())
        .collect();
    h.stop_clean();
    out
}

#[test]
fn sim_drag_grouped_window_beyond_group_border_leaves_group() {
    // #91 landscape: dropping past the group's shared border (empty space) ungroups onto that side.
    let cols = drag_group_beyond_border(primary_only_rig(), (101, 102), "MON0", true);
    assert_eq!(cols, vec![vec![102], vec![101]]);
    // portrait
    let cols = drag_group_beyond_border(three_monitor_rig(), (301, 302), "MON2", false);
    assert_eq!(cols, vec![vec![302], vec![301]]);
}

#[test]
fn sim_tray_hide_of_off_view_window_not_at_park_position_removes_ghost_tile() {
    // #71: an off-view window that hides itself (tray-hide) while NOT sitting at Planor's park position is not
    // "parked by Planor": it must leave the strip instead of staying behind as a ghost tile.
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 400,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            103,
            Rect {
                x: 800,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
    });
    focus(&mut h, 103);
    let key = (h.engine.desktop.unwrap(), "MON0".to_string());
    assert!(!h.engine.strips[&key].in_view(101));
    {
        let mut sim = h.desktop.0.lock().unwrap();
        let w = sim.windows.get_mut(&101).unwrap();
        w.rect = Rect {
            x: 100,
            y: 0,
            w: 800,
            h: 1000,
        };
        w.visible = false;
    }
    h.engine.event(Event::Window(EVENT_OBJECT_HIDE, 101));
    h.settle();
    assert!(
        h.engine.strips[&key].locate(101).is_none(),
        "tray-hidden window must not remain a ghost tile"
    );
    h.stop_clean();
}

#[test]
fn sim_real_click_on_another_window_inside_self_activated_ttl_is_not_swallowed() {
    // #71: inside the 250 ms self_activated TTL only the echo of Planor's own activation is ignored; a real
    // foreground change to a different window must still select it.
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 400,
                y: 0,
                w: 800,
                h: 1000,
            },
        ));
    });
    focus(&mut h, 101);
    let key = (h.engine.desktop.unwrap(), "MON0".to_string());
    h.engine.self_activated = Some((101, std::time::Instant::now()));
    h.desktop.0.lock().unwrap().foreground = 102;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 102));
    assert_eq!(
        h.engine.strips[&key].active(),
        Some(102),
        "real click within the TTL must select the clicked window"
    );
    h.stop_clean();
}

#[test]
fn fullscreen_tile_carries_backend_frame_into_scene_for_preserved_rect() {
    // #80: the renderer used to call win32::frame per pass; the scene now carries the backend frame.
    let mut h = SimHarness::with_setup(primary_only_rig(), test_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            1,
            Rect {
                x: 50,
                y: 80,
                w: 600,
                h: 400,
            },
        ));
        sim.add_window(SimWindow::new(
            2,
            Rect {
                x: 150,
                y: 80,
                w: 600,
                h: 400,
            },
        ));
        sim.foreground = 1;
    });
    let id = 1;
    let odd = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1200,
    };
    {
        let mut g = h.desktop.0.lock().unwrap();
        let w = g.windows.get_mut(&id).unwrap();
        w.rect = odd;
        w.caption = false;
    }
    h.engine
        .event(Event::Window(EVENT_OBJECT_LOCATIONCHANGE, id));
    h.settle();
    assert_eq!(
        h.engine.strips[&(1, "MON0".into())].fullscreen_id(),
        Some(id)
    );
    let scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
    let frame = scene
        .os_frames
        .iter()
        .find(|(i, _)| *i == id)
        .expect("fullscreen tile in scene")
        .1;
    assert!(frame.is_some(), "fullscreen tile carries the backend frame");
    let want = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1200,
    };
    assert_eq!(
        crate::render::fullscreen_preserved_rect(want, frame, true).h,
        frame.unwrap().h
    );
    // Non-fullscreen tiles never sample a frame.
    assert!(scene.os_frames.iter().all(|(i, _)| *i == id));
}

#[test]
fn scroll_hold_fg_survives_a_resumed_gesture_while_foreground_stays_parked() {
    // #80: after a 3-finger scroll the OS foreground stays on the now-scrolled-away window (101).
    // Resuming with another gesture must keep scrolling from the held view: no snap back to 101,
    // no foreground steal, and the hold only clears once the OS foreground genuinely moves.
    let mut cfg = test_cfg();
    cfg["touchpad"]["three_finger_enabled"] = toml::Value::Boolean(true);
    cfg["touchpad"]["swipe_natural_direction"] = toml::Value::Boolean(true);
    cfg["touchpad"]["swipe_scroll_gain"] = toml::Value::Float(4.0);
    cfg["touchpad"]["swipe_inertia"] = toml::Value::Boolean(false);
    let mut h = SimHarness::with_setup(primary_only_rig(), cfg, |sim| {
        for (i, x) in [(101, 0), (102, 400), (103, 800), (104, 1200)] {
            sim.add_window(SimWindow::new(
                i,
                Rect {
                    x,
                    y: 0,
                    w: 900,
                    h: 1000,
                },
            ));
        }
    });
    focus(&mut h, 101);
    let key = (h.engine.desktop.unwrap(), "MON0".to_string());
    let gesture = |h: &mut SimHarness, steps: usize, dx: f64| {
        let mut x = 500.0;
        h.engine.event(Event::Contacts(vec![
            (x, 100.0),
            (x + 20.0, 100.0),
            (x + 40.0, 100.0),
        ]));
        for _ in 0..steps {
            x += dx;
            h.engine.event(Event::Contacts(vec![
                (x, 100.0),
                (x + 20.0, 100.0),
                (x + 40.0, 100.0),
            ]));
            h.settle();
        }
        h.engine.event(Event::Contacts(vec![]));
        h.engine.inertia = None;
        if h.engine.settle.is_some() {
            h.engine.settle = Some(Instant::now() - Duration::from_millis(1));
        }
        for _ in 0..4 {
            h.engine.tick().unwrap();
        }
    };
    gesture(&mut h, 10, -25.0);
    let first = h.engine.strips[&key].active();
    assert_ne!(first, Some(101));
    assert_eq!(
        h.engine.scroll_hold_fg,
        Some(101),
        "hold remembers the parked foreground window"
    );

    // Resume with a second gesture in the same direction: view keeps going, foreground untouched.
    gesture(&mut h, 10, -25.0);
    assert_eq!(
        h.desktop.foreground(),
        101,
        "resumed gesture must not steal OS foreground"
    );
    assert_eq!(
        h.engine.scroll_hold_fg,
        Some(101),
        "hold persists across the resumed gesture"
    );
    let second = h.engine.strips[&key].active();
    assert_ne!(second, Some(101));
    for _ in 0..5 {
        h.engine.tick().unwrap();
    }
    assert_eq!(
        h.engine.strips[&key].active(),
        second,
        "ticks must not snap back to the held fg"
    );

    // A genuine foreground change releases the hold.
    h.desktop.0.lock().unwrap().foreground = 102;
    h.engine.event(Event::Window(EVENT_SYSTEM_FOREGROUND, 102));
    h.settle();
    assert_eq!(h.engine.strips[&key].active(), Some(102));
    assert_eq!(h.engine.scroll_hold_fg, None);
    h.stop_clean();
}

/// Carry window `id` (grouped with its neighbour) on `mon`: wobble across inside the column, then run far
/// along the strip. Returns the cross-axis cursor offset from the pick-up line after every report.
fn offsets_after_leaving_column(
    mon: &str,
    id: isize,
    along: (f64, f64),
    across: (f64, f64),
) -> (f64, Vec<f64>) {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    make_group(&mut h, mon, &[id, id + 1]);
    let horizontal = mon == "MON0";
    let (mut x, mut y) = (500., 500.);
    h.engine.event(Event::Contacts(four(x, y)));
    for _ in 0..3 {
        x += along.0;
        y += along.1;
        h.engine.event(Event::Contacts(four(x, y)));
    }
    let line = h.engine.pad.as_ref().expect("carrying").line;
    // Cross travel while still over the column pushes the cursor off the line.
    for _ in 0..3 {
        x += across.0;
        y += across.1;
        h.engine.event(Event::Contacts(four(x, y)));
    }
    let mut out = vec![];
    for _ in 0..40 {
        x += along.0;
        y += along.1;
        h.engine.event(Event::Contacts(four(x, y)));
        let c = h.engine.pad.as_ref().expect("carrying").cursor;
        out.push((if horizontal { c.1 } else { c.0 }) - line);
    }
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    (line, out)
}

#[test]
fn axis_lock_eases_back_onto_the_line_instead_of_snapping() {
    for (mon, id, along, across) in [
        ("MON0", 101, (6., 0.), (0., 10.)),
        ("MON2", 301, (0., 6.), (10., 0.)),
    ] {
        let (_, off) = offsets_after_leaving_column(mon, id, along, across);
        let start = off.iter().position(|o| o.abs() > 1e-6).unwrap_or(0);
        let peak = off
            .iter()
            .cloned()
            .fold(0., |a: f64, b| if b.abs() > a.abs() { b } else { a });
        assert!(
            peak.abs() > 2.,
            "{mon}: cross travel over the column must offset the cursor ({off:?})"
        );
        // Never jumps the whole way in one report, shrinks monotonically, and arrives exactly.
        let tail = &off[start..];
        for w in tail.windows(2) {
            assert!(
                w[1].abs() <= w[0].abs() + 1e-9,
                "{mon}: easing must not move away from the line: {tail:?}"
            );
        }
        let drops = tail
            .windows(2)
            .filter(|w| w[1].abs() < w[0].abs() - 1e-9)
            .count();
        assert!(
            drops >= 2,
            "{mon}: the return must take several frames, not one: {tail:?}"
        );
        assert_eq!(
            *off.last().unwrap(),
            0.,
            "{mon}: ends exactly on the pick-up line"
        );
    }
}

#[test]
fn axis_lock_cross_travel_over_the_group_column_does_not_count_towards_release() {
    // Cross travel (several times the 7x-deadzone release distance) while the cursor is over the window's
    // own group column must not release the lock towards a monitor that lies that way (#84).
    // MON2 (portrait) has MON0 to its right; with cross-monitor drags on, enough cross travel would release.
    let mut cfg = note_cfg();
    cfg["touchpad"]["swipe_fling_enabled"] = toml::Value::Boolean(false);
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
    make_group(&mut h, "MON2", &[301, 302]);
    let (mut x, mut y) = (500., 500.);
    h.engine.event(Event::Contacts(four(x, y)));
    for _ in 0..3 {
        y += 6.;
        h.engine.event(Event::Contacts(four(x, y)));
    }
    assert!(
        h.engine.pad.as_ref().is_some_and(|p| p.lock.is_some()),
        "locked after pick-up"
    );
    // Sweep rightwards (towards MON0) well past the 7x-deadzone release distance, still over the column.
    for _ in 0..14 {
        x += 8.;
        h.engine.event(Event::Contacts(four(x, y)));
        assert!(
            h.engine.pad.as_ref().is_some_and(|p| p.lock.is_some()),
            "lock released by travel over the column"
        );
    }
    let p = h.engine.pad.as_ref().expect("carrying");
    assert!(
        p.lock.is_some(),
        "lock must survive cross travel over the group column"
    );
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
}

#[test]
fn spawn_on_cursor_monitor_true_lands_on_cursor_monitor() {
    // (#97) When spawn_on_cursor_monitor = true, a window shown with a frame on MON0
    // lands on the monitor under the cursor (MON2).
    let mut cfg = test_cfg();
    cfg["layout"]["spawn_on_cursor_monitor"] = toml::Value::Boolean(true);
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, |sim| {
        sim.cursor = (-700, 500); // inside MON2 (-1440..0, 0..2560)
    });
    let id = h.desktop.0.lock().unwrap().add_window(SimWindow::new(
        501,
        Rect {
            x: 100,
            y: 100,
            w: 800,
            h: 600,
        },
    ));
    h.engine.event(Event::Window(EVENT_OBJECT_SHOW, id));
    h.settle();
    assert_eq!(
        h.engine
            .strips
            .iter()
            .find_map(|((_, m), s)| s.locate(id).map(|_| m.as_str())),
        Some("MON2"),
        "window shown on MON0 must land on cursor monitor MON2 when spawn_on_cursor_monitor = true"
    );
}

#[test]
fn spawn_on_cursor_monitor_false_lands_on_default_monitor() {
    // (#97) When spawn_on_cursor_monitor = false, a window shown with a frame on MON0
    // ignores the cursor on MON2 and lands on its overlapping monitor (MON0).
    let mut cfg = test_cfg();
    cfg["layout"]["spawn_on_cursor_monitor"] = toml::Value::Boolean(false);
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, |sim| {
        sim.cursor = (-700, 500); // inside MON2 (-1440..0, 0..2560)
    });
    let id = h.desktop.0.lock().unwrap().add_window(SimWindow::new(
        502,
        Rect {
            x: 100,
            y: 100,
            w: 800,
            h: 600,
        },
    ));
    h.engine.event(Event::Window(EVENT_OBJECT_SHOW, id));
    h.settle();
    assert_eq!(
        h.engine
            .strips
            .iter()
            .find_map(|((_, m), s)| s.locate(id).map(|_| m.as_str())),
        Some("MON0"),
        "window shown on MON0 must stay on MON0 when spawn_on_cursor_monitor = false"
    );
}

#[test]
fn drag_hover_never_draws_the_stack_border_before_group_mode() {
    // (#105, reworked in 4.8) While dragging over another window the target must not get the mauve stacked
    // border (nor any other drop colour) unless the drop would STACK: neither a swap-region nor a middle hover does.
    let mut h = SimHarness::with_setup(primary_only_rig(), note_cfg(), |sim| {
        sim.add_window(SimWindow::new(
            101,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.add_window(SimWindow::new(
            102,
            Rect {
                x: 960,
                y: 0,
                w: 900,
                h: 1000,
            },
        ));
        sim.foreground = 101;
    });
    let r1 = h.rect(101);
    let r2 = h.rect(102);
    // Normal focused border before drag is untouched.
    assert_eq!(tile_color(&h, 101), h.engine.hot.focused_color);

    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 101));
    // 1. Hover in Swap region (outer 25% of target 102)
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&101).unwrap().rect = Rect {
            x: r1.x + 40,
            y: r1.y + 20,
            ..r1
        };
        g.cursor = (r2.x + 20, r2.y + r2.h / 2);
    }
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert_eq!(
        h.engine.drag.as_ref().map(|d| (d.target, d.mode)),
        Some((Some(102), planor::Drag::None)),
        "outer edge hover is before 1/4: no tile mode yet"
    );
    assert_ne!(
        tile_color(&h, 102),
        h.engine.hot.stacked_color,
        "target window must not show the stack border during a before-quarter hover"
    );
    let scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
    assert!(scene.cue.is_none(), "no cue card exists any more");

    // 2. Hover in Stack (merge) region (centre of target 102)
    h.desktop.0.lock().unwrap().cursor = (r2.x + r2.w / 2, r2.y + r2.h / 2);
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert_eq!(
        h.engine.drag.as_ref().map(|d| (d.target, d.mode)),
        Some((Some(102), planor::Drag::Before)),
        "centre hover waits for the dwell and previews the midpoint side"
    );
    assert_ne!(
        tile_color(&h, 102),
        h.engine.hot.stacked_color,
        "target window must not show the stack border during a plain middle hover (no modifier)"
    );
    let scene = h.desktop.0.lock().unwrap().scenes.last().unwrap().clone();
    assert!(scene.cue.is_none(), "no cue card exists any more");
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 101));
    h.settle();
}

// ---- Milestone 4.8 (#111-#116) ----

/// Start a mouse drag of `id`, then put the cursor at `to` (no release). Returns after one tick.
fn mouse_hold(h: &mut SimHarness, id: isize, to: (i32, i32)) {
    let r = h.rect(id);
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.keys_down.insert(0x01);
        g.cursor = (r.x + r.w / 2, r.y + 10);
    }
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, id));
    h.settle_quiet();
    let mut g = h.desktop.0.lock().unwrap();
    let w = g.windows.get_mut(&id).unwrap();
    w.rect.x = to.0 - r.w / 2;
    w.rect.y = to.1 - 10;
    g.cursor = to;
    drop(g);
    h.engine.tick().unwrap();
}

fn phase(h: &SimHarness) -> Option<crate::daemon::DragPhase> {
    h.engine.drag.as_ref().map(|d| d.phase)
}

/// Middle of a neighbour (1/4..3/4 from the entry edge) that is not yet in dwell: waiting, no cue.
/// After the dwell: preview with the cue; with the modifier: group mode and the committed cue (#113).
#[test]
fn dwell_in_the_middle_range_previews_then_modifier_groups_both_orientations() {
    for (mon, ids) in [("MON0", [101, 102, 103]), ("MON2", [301, 302, 303])] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        let key = (1, mon.to_string());
        focus(&mut h, ids[0]);
        let s = &h.engine.strips[&key];
        let horizontal = s.area.horizontal();
        let (a, b) = (ids[0], ids[1]);
        let rb = crate::daemon::clip(
            s.rects(96).into_iter().find(|x| x.0 == b).unwrap().1,
            s.area,
        )
        .unwrap();
        // Enter b from its leading edge, then settle at 3/8 (middle range, left of midpoint).
        let at = |f: f64| {
            if horizontal {
                (rb.x + (rb.w as f64 * f) as i32, rb.y + rb.h / 2)
            } else {
                (rb.x + rb.w / 2, rb.y + (rb.h as f64 * f) as i32)
            }
        };
        mouse_hold(&mut h, a, at(0.05));
        assert!(
            matches!(
                phase(&h),
                Some(crate::daemon::DragPhase::BeforeQuarter { .. })
            ),
            "{mon}: before 1/4: {:?}",
            phase(&h)
        );
        assert!(zones(&h).is_empty(), "{mon}: no cue before 1/4");
        h.desktop.0.lock().unwrap().cursor = at(0.375);
        h.engine.tick().unwrap();
        assert!(
            matches!(
                phase(&h),
                Some(crate::daemon::DragPhase::MiddleWaiting { .. })
            ),
            "{mon}: middle range waits for the dwell: {:?}",
            phase(&h)
        );
        assert!(
            h.engine.drag.as_ref().unwrap().vacancy != crate::daemon::VacancyState::Bypassed,
            "{mon}: vacancy timer untouched until a preview triggers"
        );
        h.engine
            .advance_drag_timers_for_test(Duration::from_millis(300));
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert!(
            matches!(
                phase(&h),
                Some(crate::daemon::DragPhase::MiddlePreview { after: false, .. })
            ),
            "{mon}: dwell elapsed: preview on the pointer's side of the midpoint: {:?}",
            phase(&h)
        );
        assert_ne!(
            tile_color(&h, b),
            h.engine.hot.stacked_color,
            "{mon}: a tile-beside preview never shows the stack border"
        );
        assert!(zones(&h).is_empty(), "{mon}: no cue card exists any more");
        assert_eq!(
            h.engine.drag.as_ref().unwrap().vacancy,
            crate::daemon::VacancyState::Bypassed,
            "{mon}: preview bypasses the vacancy delay"
        );
        // Press the setting-selected modifier (Shift by default): group mode.
        h.desktop.0.lock().unwrap().keys_down.insert(0x10);
        h.engine.tick().unwrap();
        h.settle_quiet();
        assert!(
            matches!(
                phase(&h),
                Some(crate::daemon::DragPhase::MiddleGroup { .. })
            ),
            "{mon}: modifier -> group mode: {:?}",
            phase(&h)
        );
        assert_eq!(
            tile_color(&h, b),
            h.engine.hot.stacked_color,
            "{mon}: group mode lights the target in the mauve stacked border"
        );
        // Group mode latches once activated: releasing Shift with the button still groups on the drop.
        h.desktop.0.lock().unwrap().keys_down.remove(&0x10);
        h.engine.tick().unwrap();
        assert!(matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::MiddleGroup { .. })
        ));
        h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
        drop_now(&mut h, a);
        let s = &h.engine.strips[&key];
        assert!(
            s.columns[s.locate(a).unwrap().0].members.contains(&b),
            "{mon}: group mode latched -> grouped on the drop"
        );
        h.stop_clean();
    }
}

/// Moving again before the dwell elapses restarts it; reversing out of the neighbour cancels (#113).
#[test]
fn dwell_restarts_on_movement_and_reversal_out_of_the_neighbour_cancels() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let key = (1, "MON0".to_string());
    focus(&mut h, 101);
    let s = &h.engine.strips[&key];
    let rb = crate::daemon::clip(
        s.rects(96).into_iter().find(|x| x.0 == 102).unwrap().1,
        s.area,
    )
    .unwrap();
    mouse_hold(&mut h, 101, (rb.x + rb.w * 3 / 8, rb.y + rb.h / 2));
    h.engine
        .advance_drag_timers_for_test(Duration::from_millis(200));
    // Move 40 px: the dwell restarts, so another 200 ms is still waiting.
    h.desktop.0.lock().unwrap().cursor = (rb.x + rb.w * 3 / 8 + 40, rb.y + rb.h / 2);
    h.engine.tick().unwrap();
    h.engine
        .advance_drag_timers_for_test(Duration::from_millis(200));
    h.engine.tick().unwrap();
    assert!(
        matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::MiddleWaiting { .. })
        ),
        "movement restarted the dwell: {:?}",
        phase(&h)
    );
    h.engine
        .advance_drag_timers_for_test(Duration::from_millis(100));
    h.engine.tick().unwrap();
    assert!(matches!(
        phase(&h),
        Some(crate::daemon::DragPhase::MiddlePreview { .. })
    ));
    // Reverse back out into the gap/own tile: preview ends, no cue.
    h.desktop.0.lock().unwrap().cursor = (rb.x - 30, rb.y + rb.h / 2);
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert!(
        !matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::MiddlePreview { .. })
        ),
        "reversal cancels the preview: {:?}",
        phase(&h)
    );
    assert!(zones(&h).is_empty(), "no cue after cancelling");
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.stop_clean();
}

/// A zero dwell previews immediately; a changed setting is honoured (#113 configurable dwell).
#[test]
fn zero_dwell_previews_on_the_first_middle_range_sample() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    h.engine.hot.drag_dwell_ms = 0;
    let key = (1, "MON0".to_string());
    focus(&mut h, 101);
    let s = &h.engine.strips[&key];
    let rb = crate::daemon::clip(
        s.rects(96).into_iter().find(|x| x.0 == 102).unwrap().1,
        s.area,
    )
    .unwrap();
    mouse_hold(&mut h, 101, (rb.x + rb.w * 5 / 8, rb.y + rb.h / 2));
    assert!(matches!(
        phase(&h),
        Some(crate::daemon::DragPhase::MiddlePreview { after: true, .. })
    ));
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.stop_clean();
}

/// The vacated slot stays reserved for `drag_vacancy_delay_ms`, then closes (#114).
#[test]
fn vacancy_delay_reserves_the_slot_then_closes_and_a_new_drag_starts_fresh() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    assert_eq!(
        h.engine.hot.drag_vacancy_delay_ms, 2000,
        "default is two seconds"
    );
    focus(&mut h, 101);
    let r1 = h.rect(101);
    let free = (r1.x + 20, 1190); // below every tile: no target
    mouse_hold(&mut h, 101, free);
    assert!(
        matches!(
            h.engine.drag.as_ref().unwrap().vacancy,
            crate::daemon::VacancyState::Reserved { .. }
        ),
        "slot reserved while the move is in progress"
    );
    h.engine
        .advance_drag_timers_for_test(Duration::from_millis(1900));
    h.engine.tick().unwrap();
    assert!(matches!(
        h.engine.drag.as_ref().unwrap().vacancy,
        crate::daemon::VacancyState::Reserved { .. }
    ));
    h.engine
        .advance_drag_timers_for_test(Duration::from_millis(200));
    h.engine.tick().unwrap();
    assert_eq!(
        h.engine.drag.as_ref().unwrap().vacancy,
        crate::daemon::VacancyState::Closed,
        "closed after the configured delay"
    );
    // Cancelled gesture: restoring the frame and dropping leaves the layout; the next drag is fresh.
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&101)
        .unwrap()
        .rect = r1;
    drop_now(&mut h, 101);
    assert!(h.engine.drag.is_none());
    focus(&mut h, 101);
    mouse_hold(&mut h, 101, free);
    assert!(
        matches!(
            h.engine.drag.as_ref().unwrap().vacancy,
            crate::daemon::VacancyState::Reserved { .. }
        ),
        "a stale Closed state never leaks into the next gesture"
    );
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.stop_clean();
}

/// A zero delay closes the vacancy at once (#114 bounds/semantics).
#[test]
fn zero_vacancy_delay_closes_immediately() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    h.engine.hot.drag_vacancy_delay_ms = 0;
    focus(&mut h, 101);
    let r1 = h.rect(101);
    mouse_hold(&mut h, 101, (r1.x + 20, 1190));
    assert_eq!(
        h.engine.drag.as_ref().unwrap().vacancy,
        crate::daemon::VacancyState::Closed
    );
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.stop_clean();
}

/// Mouse edge-scroll uses the monitor under the cursor and its own configured percentage (#116):
/// the zone is 10% of that monitor's span, the old monitor stops scrolling after crossing, and 0 disables.
#[test]
fn mouse_edge_scroll_uses_the_cursor_monitor_percent_and_zero_disables() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let key = (1, "MON0".to_string());
    focus(&mut h, 103);
    let off0 = h.engine.strips[&key].offset;
    assert!(off0 > 0, "103 is scrolled into view");
    // 10% of 1920 = 192 px: x=150 is inside the zone, x=250 is not.
    mouse_hold(&mut h, 103, (250, 600));
    thread::sleep(Duration::from_millis(20));
    h.engine.fast_tick();
    assert_eq!(
        h.engine.strips[&key].offset, off0,
        "outside the 10% zone: no scroll"
    );
    h.desktop.0.lock().unwrap().cursor = (150, 600);
    h.engine.tick().unwrap();
    thread::sleep(Duration::from_millis(20));
    h.engine.fast_tick();
    assert!(
        h.engine.strips[&key].offset < off0,
        "inside the 10% zone: leading edge scrolls the strip back"
    );
    // Cross onto MON1 (x >= 1920): MON0 stops scrolling.
    let now = h.engine.strips[&key].offset;
    h.desktop.0.lock().unwrap().cursor = (2500, 600);
    h.engine.tick().unwrap();
    thread::sleep(Duration::from_millis(20));
    h.engine.fast_tick();
    assert_eq!(
        h.engine.strips[&key].offset, now,
        "old monitor no longer scrolls"
    );
    // Disabled: 0 means no edge scrolling anywhere.
    h.engine.hot.drag_edge_scroll_percent = 0;
    h.desktop.0.lock().unwrap().cursor = (5, 600);
    h.engine.tick().unwrap();
    thread::sleep(Duration::from_millis(20));
    h.engine.fast_tick();
    assert_eq!(h.engine.strips[&key].offset, now, "0 disables edge scroll");
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.stop_clean();
}

/// Portrait monitors edge-scroll vertically with their own percentage of height (#116).
#[test]
fn mouse_edge_scroll_on_a_portrait_monitor_follows_the_vertical_axis() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let key = (1, "MON2".to_string());
    focus(&mut h, 303);
    let off0 = h.engine.strips[&key].offset;
    assert!(off0 > 0);
    // MON2 is x -1440..0, y 0..2512: top 10% = 251 px.
    mouse_hold(&mut h, 303, (-700, 100));
    thread::sleep(Duration::from_millis(20));
    h.engine.fast_tick();
    assert!(
        h.engine.strips[&key].offset < off0,
        "top zone scrolls the portrait strip"
    );
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.stop_clean();
}

/// A four-finger drag never leaves its starting monitor unless it flings (#111).
#[test]
fn non_fling_four_finger_drag_stays_on_its_monitor_even_pushed_across_the_boundary() {
    let mut cfg = note_cfg();
    cfg["touchpad"]["swipe_fling_enabled"] = toml::Value::Boolean(false);
    for (focus_id, home, step) in [
        (102, "MON0", (10., 0.)),
        (302, "MON2", (0., 10.)),
        (302, "MON2", (10., 0.)),
    ] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), cfg.clone(), three_per_monitor);
        focus(&mut h, focus_id);
        h.engine.event(Event::Contacts(four(500., 500.)));
        for i in 1..=60 {
            h.engine.event(Event::Contacts(four(
                500. + step.0 * i as f64,
                500. + step.1 * i as f64,
            )));
        }
        h.engine.event(Event::Contacts(vec![]));
        h.settle();
        let k = (1, home.to_string());
        assert!(
            h.engine.strips[&k].locate(focus_id).is_some(),
            "{home}: window {focus_id} must stay on its starting monitor"
        );
        h.stop_clean();
    }
}

/// A mouse drag released while still before 1/4 of a neighbour on its own monitor returns to its slot (#113),
/// and crossing to another monitor and releasing still transfers (mouse drags keep monitor hopping).
#[test]
fn release_before_a_quarter_returns_home_and_mouse_still_crosses_monitors() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let key = (1, "MON0".to_string());
    focus(&mut h, 101);
    let layout = |h: &SimHarness| {
        h.engine.strips[&key]
            .columns
            .iter()
            .map(|c| c.members.clone())
            .collect::<Vec<_>>()
    };
    let before = layout(&h);
    let s = &h.engine.strips[&key];
    let rb = crate::daemon::clip(
        s.rects(96).into_iter().find(|x| x.0 == 102).unwrap().1,
        s.area,
    )
    .unwrap();
    mouse_hold(&mut h, 101, (rb.x + rb.w / 10, rb.y + rb.h / 2));
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    drop_now(&mut h, 101);
    assert_eq!(before, layout(&h), "release before 1/4: layout unchanged");
    // Mouse drag onto MON1 and release: transferred.
    focus(&mut h, 101);
    to_start(&mut h, "MON1");
    focus(&mut h, 101);
    let r201 = h.rect(201);
    mouse_hold(&mut h, 101, (r201.x + r201.w / 2, r201.y + r201.h / 2));
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    drop_now(&mut h, 101);
    assert!(
        h.engine.strips[&(1, "MON1".to_string())]
            .locate(101)
            .is_some(),
        "mouse drag keeps free monitor hopping"
    );
    h.stop_clean();
}

/// Stack exit: a quarter-span pull along the stack axis ungroups the member; a shorter pull keeps the
/// group (#115). Landscape pulls sideways, portrait pulls vertically.
#[test]
fn stack_exit_needs_a_quarter_pull_both_orientations() {
    for (mon, ids) in [("MON0", [101, 102, 103]), ("MON2", [301, 302, 303])] {
        for (frac, exits) in [(0.05_f64, false), (0.20, false), (0.30, true), (0.60, true)] {
            let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
            let key = (1, mon.to_string());
            focus(&mut h, ids[1]);
            h.cmd(Command::Key {
                name: "stack_toggle".into(),
            });
            h.settle();
            let s = &h.engine.strips[&key];
            let active = s.active().unwrap();
            assert_eq!(s.columns[s.locate(active).unwrap().0].members.len(), 2);
            let horizontal = s.area.horizontal();
            let span = s.columns[s.locate(active).unwrap().0].length;
            let pull = (span as f64 * frac) as i32;
            let r = h.rect(active);
            let start = (r.x + r.w / 2, r.y + 10);
            let to = if horizontal {
                (start.0 + pull, start.1)
            } else {
                (start.0, start.1 + pull)
            };
            mouse_hold(&mut h, active, start);
            {
                let mut g = h.desktop.0.lock().unwrap();
                let w = g.windows.get_mut(&active).unwrap();
                w.rect.x += if horizontal { pull } else { 0 };
                w.rect.y += if horizontal { 0 } else { pull };
                g.cursor = to;
            }
            h.engine.tick().unwrap();

            h.settle_quiet();
            h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
            drop_now(&mut h, active);
            let s = &h.engine.strips[&key];
            let grouped = s.columns[s.locate(active).unwrap().0].members.len() == 2;
            assert_eq!(
                !grouped,
                exits,
                "{mon}: pull {frac} of span {span} = {pull}px, phase {:?} cols {:?}",
                h.engine.drag.as_ref().map(|d| d.phase),
                s.columns
                    .iter()
                    .map(|c| (c.length, c.members.len()))
                    .collect::<Vec<_>>()
            );
            h.stop_clean();
        }
    }
}

/// Review #1: a topology change mid four-finger drag cancels both the session and the pad carry.
#[test]
fn topology_change_during_a_pad_drag_clears_session_and_pad() {
    let mut cfg = note_cfg();
    cfg["touchpad"]["swipe_fling_enabled"] = toml::Value::Boolean(false);
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
    focus(&mut h, 101);
    h.engine.event(Event::Contacts(four(500., 500.)));
    for i in 1..=12 {
        h.engine
            .event(Event::Contacts(four(500. + 10. * i as f64, 500.)));
    }
    assert!(h.engine.pad.is_some(), "carrying");
    h.desktop.0.lock().unwrap().monitors = primary_only_rig();
    h.engine.event(Event::Topology);
    h.engine.expire_timers_for_test();
    h.engine.tick().unwrap();
    assert!(h.engine.drag.is_none(), "session cancelled");
    assert!(h.engine.pad.is_none(), "pad carry cancelled too");
}

/// #113: after a pad preview, a perpendicular swipe commits group mode, a reverse swipe reverts it,
/// and the swipe direction picks the stack position.
#[test]
fn pad_perpendicular_swipe_commits_group_mode_and_reverse_reverts() {
    let mut cfg = note_cfg();
    cfg["touchpad"]["swipe_fling_enabled"] = toml::Value::Boolean(false);
    cfg["touchpad"]["drag_axis_lock"] = toml::Value::Boolean(false);
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
    h.engine.hot.drag_dwell_ms = 0;
    focus(&mut h, 101);
    h.engine.event(Event::Contacts(four(500., 500.)));
    let mut x = 500.;
    let mut seen_preview = false;
    for _ in 0..80 {
        x += 8.;
        h.engine.event(Event::Contacts(four(x, 500.)));
        h.engine.tick().unwrap();
        if matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::MiddlePreview { .. })
        ) {
            seen_preview = true;
            break;
        }
    }
    assert!(seen_preview, "pad drag reached a preview: {:?}", phase(&h));
    let mut y = 500.;
    for _ in 0..30 {
        y -= 8.;
        h.engine.event(Event::Contacts(four(x, y)));
        if matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::MiddleGroup { .. })
        ) {
            break;
        }
    }
    assert!(
        matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::MiddleGroup { direction: -1, .. })
        ),
        "perpendicular swipe commits: {:?}",
        phase(&h)
    );
    for _ in 0..40 {
        y += 8.;
        h.engine.event(Event::Contacts(four(x, y)));
        if matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::MiddlePreview { .. })
        ) {
            break;
        }
    }
    assert!(
        matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::MiddlePreview { .. })
        ),
        "reverse swipe reverts: {:?}",
        phase(&h)
    );
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
}

/// The group key is the one chosen in settings: Ctrl and Alt work through the daemon, Shift does not
/// activate it then (#113, #112 modifier cue).
#[test]
fn configured_group_modifier_ctrl_and_alt_activate_group_mode_and_shift_does_not() {
    for (name, vk, other) in [("Ctrl", 0x11, 0x10), ("Alt", 0x12, 0x10)] {
        let mut cfg = note_cfg();
        cfg["layout"]["drag_stack_modifier"] = toml::Value::String(name.into());
        let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
        h.engine.hot.drag_dwell_ms = 0;
        focus(&mut h, 101);
        let s = &h.engine.strips[&(1, "MON0".to_string())];
        let rb = crate::daemon::clip(
            s.rects(96).into_iter().find(|x| x.0 == 102).unwrap().1,
            s.area,
        )
        .unwrap();
        mouse_hold(&mut h, 101, (rb.x + rb.w / 2, rb.y + rb.h / 2));
        h.engine.tick().unwrap();
        assert!(matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::MiddlePreview { .. })
        ));
        h.settle_quiet();
        assert_ne!(
            tile_color(&h, 102),
            h.engine.hot.stacked_color,
            "{name}: a plain preview never shows the stack border"
        );
        h.desktop.0.lock().unwrap().keys_down.insert(other);
        h.engine.tick().unwrap();
        assert!(
            matches!(
                phase(&h),
                Some(crate::daemon::DragPhase::MiddlePreview { .. })
            ),
            "{name}: the other modifier does nothing"
        );
        h.desktop.0.lock().unwrap().keys_down.insert(vk);
        h.engine.tick().unwrap();
        assert!(
            matches!(
                phase(&h),
                Some(crate::daemon::DragPhase::MiddleGroup { .. })
            ),
            "{name}: configured key activates group mode"
        );
        h.desktop.0.lock().unwrap().keys_down.clear();
        h.stop_clean();
    }
}

/// A modifier already held when the dwell completes still shows the preview first (review #21): group
/// A modifier already held when the dwell completes shows the preview and does NOT group (review #13): group
/// mode needs a fresh press after the preview started.
fn modifier_held_before_the_dwell_still_previews_first() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    focus(&mut h, 101);
    let s = &h.engine.strips[&(1, "MON0".to_string())];
    let rb = crate::daemon::clip(
        s.rects(96).into_iter().find(|x| x.0 == 102).unwrap().1,
        s.area,
    )
    .unwrap();
    h.desktop.0.lock().unwrap().keys_down.insert(0x10);
    mouse_hold(&mut h, 101, (rb.x + rb.w / 2, rb.y + rb.h / 2));
    h.engine
        .advance_drag_timers_for_test(Duration::from_millis(300));
    h.engine.tick().unwrap();
    assert!(
        matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::MiddlePreview { .. })
        ),
        "{:?}",
        phase(&h)
    );
    // Edge semantics (review #13): a key held since before the preview never groups on its own ...
    h.engine.tick().unwrap();
    assert!(matches!(
        phase(&h),
        Some(crate::daemon::DragPhase::MiddlePreview { .. })
    ));
    // ... it must be released and pressed again.
    h.desktop.0.lock().unwrap().keys_down.remove(&0x10);
    h.engine.tick().unwrap();
    h.desktop.0.lock().unwrap().keys_down.insert(0x10);
    h.engine.tick().unwrap();
    assert!(matches!(
        phase(&h),
        Some(crate::daemon::DragPhase::MiddleGroup { .. })
    ));
    h.desktop.0.lock().unwrap().keys_down.clear();
    h.stop_clean();
}

/// Pulling a stack member toward the leading side exits to the leading side (#115, both directions).
#[test]
fn stack_exit_toward_the_leading_side_places_the_member_before_the_stack() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let key = (1, "MON0".to_string());
    focus(&mut h, 102);
    h.cmd(Command::Key {
        name: "stack_toggle".into(),
    });
    h.settle();
    let s = &h.engine.strips[&key];
    let active = s.active().unwrap();
    let col = s.locate(active).unwrap().0;
    assert_eq!(s.columns[col].members.len(), 2);
    let r = h.rect(active);
    // Grab at the member's centre so a leftward pull stays on screen; pull 40% of the window width.
    let start = (r.x + r.w / 2, r.y + 10);
    let pull = -(r.w * 4 / 10);
    mouse_hold(&mut h, active, start);
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&active).unwrap().rect.x += pull;
        g.cursor = (start.0 + pull, start.1);
    }
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert!(
        matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::StackExit { trailing: false })
        ),
        "pull left is a leading exit: {:?}",
        phase(&h)
    );
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    drop_now(&mut h, active);
    let s = &h.engine.strips[&key];
    let (i, _) = s.locate(active).expect("member still managed");
    assert_eq!(s.columns[i].members.len(), 1, "left the stack");
    let rest = s.columns.iter().position(|c| c.members.len() > 1);
    if let Some(o) = rest {
        assert!(
            i < o,
            "pulled left: the member sits before the remaining stack"
        );
    }
    h.stop_clean();
}

/// Settings: the three new keys are listed, validated against their bounds and clamped when stepped.
#[test]
fn new_drag_settings_are_validated_with_documented_bounds() {
    let int = toml::Value::Integer;
    for (k, ok, bad) in [
        ("drag_dwell_ms", 5000, 5001),
        ("drag_vacancy_delay_ms", 10000, 10001),
        ("drag_edge_scroll_percent", 40, 41),
    ] {
        assert!(crate::valid_value(k, &int(1), &int(0)), "{k}: 0 is valid");
        assert!(crate::valid_value(k, &int(1), &int(ok)), "{k}: max valid");
        assert!(!crate::valid_value(k, &int(1), &int(bad)), "{k}: over max");
        assert!(!crate::valid_value(k, &int(1), &int(-1)), "{k}: negative");
    }
    let m = crate::settings_model::Model::from_text(std::path::Path::new("p.toml"), crate::EXAMPLE)
        .unwrap();
    for k in [
        "drag_dwell_ms",
        "drag_vacancy_delay_ms",
        "drag_edge_scroll_percent",
    ] {
        assert!(
            m.entries.iter().any(|e| e.key == k),
            "{k} appears in the settings window"
        );
    }
    // Removed keys never warn and never become entries.
    assert!(crate::REMOVED_KEYS.contains(&"stack_zone_percent"));
    assert!(!m.entries.iter().any(|e| e.key == "stack_zone_percent"));
}

/// A pad drag cancelled mid-preview restores everything at once: no session, no pad carry, no cue (#113/#114).
#[test]
fn pad_drag_cancel_during_preview_clears_session_cue_and_layout() {
    let mut cfg = note_cfg();
    cfg["touchpad"]["swipe_fling_enabled"] = toml::Value::Boolean(false);
    cfg["touchpad"]["drag_axis_lock"] = toml::Value::Boolean(false);
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
    h.engine.hot.drag_dwell_ms = 0;
    let key = (1, "MON0".to_string());
    focus(&mut h, 101);
    let before: Vec<_> = h.engine.strips[&key]
        .columns
        .iter()
        .map(|c| c.members.clone())
        .collect();
    h.engine.event(Event::Contacts(four(500., 500.)));
    let mut x = 500.;
    for _ in 0..80 {
        x += 8.;
        h.engine.event(Event::Contacts(four(x, 500.)));
        h.engine.tick().unwrap();
        if matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::MiddlePreview { .. })
        ) {
            break;
        }
    }
    assert!(matches!(
        phase(&h),
        Some(crate::daemon::DragPhase::MiddlePreview { .. })
    ));
    // A fifth finger / lift mid-gesture is the recogniser's cancel.
    h.engine.event(Event::Contacts(five(x, 500.)));
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert!(h.engine.drag.is_none() && h.engine.pad.is_none());
    assert!(zones(&h).is_empty(), "no cue left behind");
    let after: Vec<_> = h.engine.strips[&key]
        .columns
        .iter()
        .map(|c| c.members.clone())
        .collect();
    assert_eq!(before, after, "layout untouched by a cancelled gesture");
    h.stop_clean();
}

fn five(x: f64, y: f64) -> Vec<(f64, f64)> {
    (0..5).map(|i| (x + 20. * i as f64, y)).collect()
}

/// A mouse drag whose window vanishes mid-gesture ends cleanly: no session, no cue, no stale vacancy (#113).
#[test]
fn closing_the_dragged_window_mid_preview_clears_the_gesture() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    h.engine.hot.drag_dwell_ms = 0;
    focus(&mut h, 101);
    let s = &h.engine.strips[&(1, "MON0".to_string())];
    let rb = crate::daemon::clip(
        s.rects(96).into_iter().find(|x| x.0 == 102).unwrap().1,
        s.area,
    )
    .unwrap();
    mouse_hold(&mut h, 101, (rb.x + rb.w / 2, rb.y + rb.h / 2));
    h.desktop.0.lock().unwrap().windows.remove(&101);
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert!(h.engine.drag.is_none(), "session ended with its window");
    assert!(zones(&h).is_empty(), "no cue for a window that is gone");
    h.desktop.0.lock().unwrap().keys_down.clear();
    h.stop_clean();
}

/// Mouse group entry onto a neighbour of another monitor: the modifier groups there and the window
/// transfers with it; without the modifier it tiles beside the target (#113 + monitor hopping).
#[test]
fn mouse_group_across_monitors_transfers_and_stacks_only_with_the_modifier() {
    for with_modifier in [false, true] {
        let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
        h.engine.hot.drag_dwell_ms = 0;
        to_start(&mut h, "MON1");
        focus(&mut h, 101);
        let s = &h.engine.strips[&(1, "MON1".to_string())];
        let rb = crate::daemon::clip(
            s.rects(96).into_iter().find(|x| x.0 == 201).unwrap().1,
            s.area,
        )
        .unwrap();
        mouse_hold(&mut h, 101, (rb.x + rb.w / 2, rb.y + rb.h / 2));
        h.engine.tick().unwrap();
        if with_modifier {
            h.desktop.0.lock().unwrap().keys_down.insert(0x10);
            h.engine.tick().unwrap();
        }
        h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
        drop_now(&mut h, 101);
        h.desktop.0.lock().unwrap().keys_down.clear();
        let s1 = &h.engine.strips[&(1, "MON1".to_string())];
        let (i, _) = s1.locate(101).expect("transferred to MON1");
        assert_eq!(
            s1.columns[i].members.contains(&201),
            with_modifier,
            "modifier={with_modifier}"
        );
        assert!(
            h.engine.strips[&(1, "MON0".to_string())]
                .locate(101)
                .is_none()
        );
        h.stop_clean();
    }
}

/// Edge scroll must not be starved or doubled by a changed setting during a live drag (#116 live reload).
#[test]
fn edge_scroll_percent_changes_apply_to_the_running_drag() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let key = (1, "MON0".to_string());
    focus(&mut h, 103);
    let off0 = h.engine.strips[&key].offset;
    mouse_hold(&mut h, 103, (300, 600)); // 15.6%: outside the default 10%
    thread::sleep(Duration::from_millis(20));
    h.engine.fast_tick();
    assert_eq!(h.engine.strips[&key].offset, off0);
    h.engine.hot.drag_edge_scroll_percent = 20; // now inside
    h.engine.tick().unwrap();
    thread::sleep(Duration::from_millis(20));
    h.engine.fast_tick();
    assert!(
        h.engine.strips[&key].offset < off0,
        "the larger zone applies at once"
    );
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.stop_clean();
}

/// A mouse pull ACROSS the strip (down in a landscape strip, sideways in a portrait one) of a quarter of
/// the column depth also leaves a vertical group; a short pull keeps it.
#[test]
fn mouse_pull_along_the_stack_axis_exits_a_vertical_group() {
    for (mon, id) in [("MON0", 102), ("MON2", 302)] {
        for (frac, exits) in [(0.10_f64, false), (0.40, true)] {
            let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
            let key = (1, mon.to_string());
            focus(&mut h, id);
            h.cmd(Command::Key {
                name: "stack_toggle".into(),
            });
            h.settle();
            let s = &h.engine.strips[&key];
            let active = s.active().unwrap();
            assert_eq!(s.columns[s.locate(active).unwrap().0].members.len(), 2);
            let horizontal = s.area.horizontal();
            let depth = if horizontal { s.area.h } else { s.area.w };
            let pull = (depth as f64 * frac) as i32;
            let r = h.rect(active);
            let start = (r.x + r.w / 2, r.y + 10);
            let to = if horizontal {
                (start.0, start.1 + pull)
            } else {
                (start.0 + pull, start.1)
            };
            mouse_hold(&mut h, active, to);
            h.engine.tick().unwrap();
            h.settle_quiet();
            h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
            drop_now(&mut h, active);
            let s = &h.engine.strips[&key];
            let grouped = s.columns[s.locate(active).unwrap().0].members.len() == 2;
            assert_eq!(
                !grouped,
                exits,
                "{mon}: pull {frac} of depth {depth} = {pull}px, phase {:?}",
                h.engine.drag.as_ref().map(|d| d.phase)
            );
            h.stop_clean();
        }
    }
}

/// Edge scroll accelerates smoothly: slow at first, faster with time held and with depth into the zone,
/// never above the cap (owner item: fluid like the trackpad).
#[test]
fn edge_scroll_speed_eases_in_with_hold_time_and_depth() {
    use crate::daemon::edge_scroll_speed;
    let start = edge_scroll_speed(1.0, 0.0);
    let mid = edge_scroll_speed(1.0, 0.2);
    let full = edge_scroll_speed(1.0, 0.4);
    assert!(
        start > 0.0 && start < mid && mid < full,
        "{start} {mid} {full}"
    );
    assert!(edge_scroll_speed(1.0, 5.0) <= 1400.0 + 1e-9, "capped");
    assert!(
        edge_scroll_speed(0.1, 0.4) < edge_scroll_speed(1.0, 0.4),
        "deeper is faster"
    );
    assert!(
        edge_scroll_speed(0.0, 0.4) > 0.0,
        "the zone's inner edge still moves"
    );
    assert_eq!(
        edge_scroll_speed(-3.0, 9.0),
        edge_scroll_speed(0.0, 9.0),
        "depth clamps"
    );
}

/// While a window is dragged or edge-resized it is a real window above the overlay's stand-ins: its live
/// frame is cut out of the overlay (a hole) so it is never painted over by neighbours (owner item 3).
#[test]
fn the_dragged_or_resized_window_gets_an_overlay_hole() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    focus(&mut h, 101);
    let r = h.rect(102);
    h.desktop.0.lock().unwrap().cursor = (r.x + r.w / 2, r.y + 10);
    h.desktop.0.lock().unwrap().keys_down.insert(0x01);
    h.engine
        .event(Event::Window(EVENT_SYSTEM_MOVESIZESTART, 102));
    h.settle_quiet();
    // Resize by the right edge: the frame grows, so the session latches `resizing`.
    {
        let mut g = h.desktop.0.lock().unwrap();
        g.windows.get_mut(&102).unwrap().rect.w += 120;
    }
    h.engine.tick().unwrap();
    h.settle_quiet();
    assert!(h.engine.drag.as_ref().is_some_and(|d| d.resizing));
    h.engine.holes_stale = true;
    h.engine.tick().unwrap();
    h.settle_quiet();
    let frame = h.desktop.0.lock().unwrap().windows[&102].rect;
    assert!(
        h.engine.last_holes.iter().any(|hole| *hole == frame),
        "dragged window frame {frame:?} is a hole in {:?}",
        h.engine.last_holes
    );
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.engine.event(Event::Window(EVENT_SYSTEM_MOVESIZEEND, 102));
    h.settle_quiet();
    assert!(h.engine.drag.is_none());
    assert!(
        !h.engine.last_holes.iter().any(|hole| *hole == frame),
        "the hole goes away with the drag"
    );
    h.stop_clean();
}

/// #116: moving back toward the centre while still inside the edge zone stops the scroll promptly.
#[test]
fn edge_scroll_stops_when_the_pointer_reverses_inside_the_zone() {
    let mut h = SimHarness::with_setup(three_monitor_rig(), note_cfg(), three_per_monitor);
    let key = (1, "MON0".to_string());
    focus(&mut h, 103);
    mouse_hold(&mut h, 103, (60, 600)); // inside the leading 10 % zone
    thread::sleep(Duration::from_millis(30));
    h.engine.fast_tick();
    let moving = h.engine.strips[&key].offset;
    h.desktop.0.lock().unwrap().cursor = (140, 600); // back toward the centre, still inside the zone
    h.engine.tick().unwrap();
    thread::sleep(Duration::from_millis(30));
    h.engine.fast_tick();
    thread::sleep(Duration::from_millis(30));
    h.engine.fast_tick();
    assert_eq!(
        h.engine.strips[&key].offset, moving,
        "reversal stops the scroll even inside the zone"
    );
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.stop_clean();
}

/// Review #2: a brisk lift after a committed perpendicular group swipe stacks the window; it is not flung.
#[test]
fn fling_does_not_hijack_a_committed_group_entry() {
    let mut cfg = note_cfg();
    cfg["touchpad"]["swipe_fling_enabled"] = toml::Value::Boolean(true);
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
    h.engine.hot.drag_dwell_ms = 0;
    focus(&mut h, 101);
    let before = h.engine.drag.is_none();
    assert!(before);
    h.engine.hot.swipe_fling_speed = 1.0; // any lift qualifies as a fling
    h.engine.event(Event::Contacts(four(500., 500.)));
    for i in 1..=40 {
        h.engine
            .event(Event::Contacts(four(500. + 10. * i as f64, 500.)));
        h.engine.tick().unwrap();
        if matches!(
            phase(&h),
            Some(crate::daemon::DragPhase::MiddlePreview { .. })
        ) {
            break;
        }
    }
    if let Some(d) = h.engine.drag.as_mut() {
        d.phase = crate::daemon::DragPhase::MiddleGroup {
            target: 102,
            direction: 1,
        };
        d.target = Some(102);
        d.mode = planor::Drag::Stack;
    }
    h.engine.event(Event::Contacts(vec![]));
    h.settle();
    assert!(
        h.engine.strips[&(1, "MON0".to_string())]
            .locate(101)
            .is_some(),
        "the window stayed on its monitor (not flung away)"
    );
    h.stop_clean();
}

/// Review #1: after the vacancy delay the neighbours are drawn in the closed slot; a neighbour must be
/// targetable where it is DRAWN, not where it used to be.
#[test]
fn after_the_vacancy_delay_neighbours_are_hit_where_they_are_drawn() {
    let mut cfg = note_cfg();
    cfg["layout"]["drag_vacancy_delay_ms"] = toml::Value::Integer(0);
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
    h.engine.hot.drag_dwell_ms = 0;
    let key = (1, "MON0".to_string());
    focus(&mut h, 101);
    // Window 101 is first. With it removed, 102 slides left into x == area.x.
    let mut closed = h.engine.strips[&key].clone();
    closed.remove(101);
    closed.fill();
    let r102 = closed.rects(96).into_iter().find(|x| x.0 == 102).unwrap().1;
    let at = (r102.x + r102.w / 2, r102.y + r102.h / 2);
    let old = h.engine.strips[&key]
        .rects(96)
        .into_iter()
        .find(|x| x.0 == 102)
        .unwrap()
        .1;
    assert_ne!((r102.x, r102.w), (old.x, old.w), "the slot really closes");
    mouse_hold(&mut h, 101, at);
    h.engine.tick().unwrap();
    h.settle_quiet();
    let d = h.engine.drag.as_ref().expect("drag in flight");
    assert_eq!(d.vacancy, crate::daemon::VacancyState::Closed);
    assert_eq!(
        d.target,
        Some(102),
        "the drawn neighbour is targeted, phase {:?}",
        d.phase
    );
    h.desktop.0.lock().unwrap().keys_down.remove(&0x01);
    h.stop_clean();
}

/// Port of the #44 regression to the 4.8 model (review #7): a qualifying four-finger fling released onto
/// another monitor ends the pad and drag sessions at once, engages the placeholder cover, and transfers the
/// window even though the asynchronous OS move has not landed yet.
#[test]
fn fling_release_onto_another_monitor_commits_immediately_even_before_async_move_lands() {
    let mut cfg = note_cfg();
    cfg["touchpad"]["swipe_fling_enabled"] = toml::Value::Boolean(true);
    cfg["touchpad"]["swipe_fling_speed"] = toml::Value::Integer(100);
    cfg["animation"]["placeholder"] = toml::Value::Boolean(true);
    let mut h = SimHarness::with_setup(three_monitor_rig(), cfg, three_per_monitor);
    focus(&mut h, 301);
    let orig = h.rect(301);
    let mut steps = vec![(0., 5.); 4];
    steps.extend(vec![(10., 0.); 6]);
    feed_steps(&mut h, &mut steps);
    assert!(h.engine.pad.is_some(), "carrying");
    // A slow cross-DPI app: its OS frame has not left the original rect yet.
    h.desktop
        .0
        .lock()
        .unwrap()
        .windows
        .get_mut(&301)
        .unwrap()
        .rect = orig;
    h.engine.event(Event::Contacts(vec![]));
    assert!(h.engine.pad.is_none(), "tip-up releases the pad at once");
    assert!(h.engine.drag.is_none(), "the drag session ends at once");
    assert!(
        h.engine.covering.contains_key(&301),
        "the transfer engages the placeholder cover"
    );
    h.settle();
    let on = h
        .engine
        .strips
        .iter()
        .find(|(_, s)| s.locate(301).is_some())
        .map(|(k, _)| k.1.clone());
    assert_eq!(
        on,
        Some("MON0".to_string()),
        "transferred before the OS move landed"
    );
    h.stop_clean();
}

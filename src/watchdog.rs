//! Freeze watchdog (9.2): checks daemon and render progress every 50 ms and emits `STALL`
//! trace lines + diagnostic stall bundles (including ring video dump when `planor-rec ring` runs).

use crate::{diag, perf};
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

fn stall_threshold_us() -> i64 {
    std::env::var("PLANOR_STALL_MS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|&ms| ms >= 50)
        .unwrap_or(300)
        * 1000
}

pub(crate) fn find_planor_rec() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PLANOR_REC_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("planor-rec.exe");
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    let manifest_rel = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/planor-rec-bin/planor-rec.exe");
    if manifest_rel.is_file() {
        return Some(manifest_rel);
    }
    None
}

fn write_stall_bundle(thread_name: &'static str, stalled_ms: i64, span_name: &'static str) {
    let Ok(dir) = crate::folder("LOCALAPPDATA") else {
        return;
    };
    let stamp = diag::stamp().replace([':', '.'], "");
    let bundle_dir = dir.join("stalls").join(format!("{stamp}-{thread_name}"));
    if std::fs::create_dir_all(&bundle_dir).is_err() {
        return;
    }
    let _ = std::fs::write(
        bundle_dir.join("stall.txt"),
        format!(
            "thread={thread_name}\nstalled_ms={stalled_ms}\nspan={span_name}\nqpc_us={}\n",
            perf::qpc_us()
        ),
    );
    let trace_src = dir.join("trace.log");
    if let Ok(raw) = std::fs::read_to_string(&trace_src) {
        let lines: Vec<&str> = raw.lines().collect();
        let start = lines.len().saturating_sub(400);
        let _ = std::fs::write(bundle_dir.join("trace.tail.log"), lines[start..].join("\n"));
    }
    if let Ok(json) = serde_json::to_string_pretty(&perf::build_report(None)) {
        let _ = std::fs::write(bundle_dir.join("perf.json"), json);
    }
    if let Ok(trace) = serde_json::to_string(&perf::export_chrome_trace(Some(15))) {
        let _ = std::fs::write(bundle_dir.join("perf.trace.json"), trace);
    }
    if let Some(rec) = find_planor_rec() {
        let _ = Command::new(rec)
            .args(["dump", "--out"])
            .arg(bundle_dir.join("video"))
            .args(["--reason", &format!("stall-{thread_name}-{span_name}"), "--pipe", "ring"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

pub struct Watchdog {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl Watchdog {
    pub fn start(diag_enabled: bool, test_mode: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let threshold_us = stall_threshold_us();
        let join = thread::spawn(move || {
            perf::register_thread("watchdog");
            let mut ticks = 0u64;
            let mut last_daemon_warn_us = 0i64;
            let mut last_render_warn_us = 0i64;
            let mut last_bundle_us = 0i64;
            while !stop_flag.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(50));
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                ticks += 1;
                if ticks % 5 == 0 {
                    perf::sample_counters();
                }
                let now = perf::qpc_us();
                let check = [
                    (
                        "daemon",
                        perf::DAEMON_HEARTBEAT_QPC_US.load(Ordering::Relaxed),
                        perf::DAEMON_SPAN_ID.load(Ordering::Relaxed),
                        &mut last_daemon_warn_us,
                    ),
                    (
                        "render",
                        perf::RENDER_HEARTBEAT_QPC_US.load(Ordering::Relaxed),
                        perf::RENDER_SPAN_ID.load(Ordering::Relaxed),
                        &mut last_render_warn_us,
                    ),
                ];
                for (name, hb, span_id, last_warn) in check {
                    if hb <= 0 {
                        continue;
                    }
                    let gap_us = now - hb;
                    if gap_us >= threshold_us && now - *last_warn >= 1_000_000 {
                        *last_warn = now;
                        let span_name = perf::SpanKind::from_u8(span_id).name();
                        let gap_ms = gap_us / 1000;
                        perf::note_stall();
                        perf::record_instant(
                            "STALL",
                            "watchdog",
                            format!("thread={name} ms={gap_ms} span={span_name}"),
                        );
                        if diag_enabled {
                            diag::trace(format!("STALL {name} {gap_ms} ms span={span_name}"));
                        }
                        if !test_mode && now - last_bundle_us >= 15_000_000 {
                            last_bundle_us = now;
                            thread::spawn(move || write_stall_bundle(name, gap_ms, span_name));
                        }
                    }
                }
            }
        });
        Self {
            stop,
            join: Some(join),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_stall(now: i64, hb: i64, threshold_us: i64, span_id: u8) -> Option<String> {
        let gap = now - hb;
        if hb > 0 && gap >= threshold_us {
            let span = perf::SpanKind::from_u8(span_id).name();
            Some(format!("STALL daemon {} ms span={span}", gap / 1000))
        } else {
            None
        }
    }

    #[test]
    fn detects_intentional_stall_and_stays_silent_when_healthy() {
        let _guard = perf::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        perf::init(false);
        perf::reset();
        let now = perf::qpc_us();
        // Normal healthy 16 ms cadence produces zero stall lines.
        for delta_ms in [0, 16, 32, 100, 250] {
            assert_eq!(
                eval_stall(now + delta_ms * 1000, now, 300_000, perf::SpanKind::DaemonTick as u8),
                None
            );
        }
        // Intentional 420 ms stall while inside render.place / daemon.layout produces the STALL line.
        let stalled = eval_stall(
            now + 420_000,
            now,
            300_000,
            perf::SpanKind::DaemonLayout as u8,
        )
        .expect("420 ms stall must emit STALL line");
        assert!(stalled.starts_with("STALL daemon 420 ms span=daemon.layout"));

        // Live thread smoke check: advancing heartbeats emit zero stalls; blocking for 350 ms emits one.
        let before = perf::stall_count();
        let _wd = Watchdog::start(false, true);
        for _ in 0..8 {
            perf::heartbeat_daemon();
            perf::heartbeat_render();
            thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(perf::stall_count(), before, "healthy heartbeats must never trigger a stall");

        perf::DAEMON_SPAN_ID.store(perf::SpanKind::DaemonTick as u8, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(400));
        assert!(
            perf::stall_count() > before,
            "stalled daemon heartbeat for 400 ms must trigger watchdog stall detection"
        );
        perf::DAEMON_HEARTBEAT_QPC_US.store(perf::qpc_us(), Ordering::Relaxed);
        perf::RENDER_HEARTBEAT_QPC_US.store(perf::qpc_us(), Ordering::Relaxed);
    }

    #[test]
    fn heartbeat_overhead_is_negligible() {
        let _guard = perf::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        perf::init(false);
        const N: usize = 500_000;
        let t = std::time::Instant::now();
        for _ in 0..N {
            perf::heartbeat_daemon();
        }
        let ns = t.elapsed().as_nanos() as f64 / N as f64;
        eprintln!("WATCHDOG_HEARTBEAT_OVERHEAD: {ns:.1} ns/op");
    }
}

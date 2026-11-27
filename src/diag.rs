use super::*;
use crossbeam_queue::ArrayQueue;
use std::path::Path;
#[derive(Default, Clone, Copy, Serialize)]
pub struct Frame {
    pub input_to_present_us: Option<u64>,
    pub render_us: u64,
    pub interval_us: u64,
    pub placement_us: u64,
    pub thumbnail_us: u64,
    pub retries: u32,
}
pub struct Telemetry {
    pub enabled: bool,
    ring: ArrayQueue<Frame>,
    recent: std::collections::VecDeque<Frame>,
}
impl Telemetry {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ring: ArrayQueue::new(4096),
            recent: Default::default(),
        }
    }
    pub fn record(&self, f: Frame) {
        if self.enabled {
            self.ring.force_push(f);
        }
    }
    pub fn command(&mut self, action: Diagnostic) -> Result<String> {
        match action {
            Diagnostic::On => self.enabled = true,
            Diagnostic::Off => self.enabled = false,
            _ => {
                while let Some(f) = self.ring.pop() {
                    if self.recent.len() == 4096 {
                        self.recent.pop_front();
                    }
                    self.recent.push_back(f)
                }
                let mut times: Vec<_> = self
                    .recent
                    .iter()
                    .map(|f| f.interval_us)
                    .filter(|&v| v > 0)
                    .collect();
                times.sort_unstable();
                let fps = |fraction: usize| {
                    let n = times.len().div_ceil(fraction);
                    if n == 0 {
                        None
                    } else {
                        Some(
                            1_000_000. * n as f64
                                / times[times.len() - n..].iter().sum::<u64>() as f64,
                        )
                    }
                };
                let nominal = times.get(times.len() / 2).copied().unwrap_or(1).max(1);
                let drops: u64 = times
                    .iter()
                    .map(|v| ((v + nominal / 2) / nominal).saturating_sub(1))
                    .sum();
                let mut report = serde_json::json!({"frames":times.len(),"one_percent_low_fps":fps(100),"point_one_percent_low_fps":fps(1000),"estimated_dropped_frames":drops,"last":self.recent.back()});
                if matches!(action, Diagnostic::Export { .. }) {
                    report["samples"] = serde_json::to_value(&self.recent)?
                }
                return Ok(serde_json::to_string(&report)?);
            }
        }
        Ok("ok".into())
    }
}
pub fn append(dir: &Path, msg: &[u8], cap: u64, count: usize) -> Result<()> {
    if count == 0 || msg.len() as u64 + 1 > cap {
        return Ok(());
    }
    let path = |i| dir.join(format!("frames.{i}.log"));
    if fs::metadata(path(0)).map(|m| m.len()).unwrap_or(0) + msg.len() as u64 + 1 > cap {
        if path(count - 1).exists() {
            fs::remove_file(path(count - 1))?
        }
        for i in (1..count).rev() {
            if path(i - 1).exists() {
                fs::rename(path(i - 1), path(i))?
            }
        }
    }
    let mut f = OpenOptions::new().append(true).create(true).open(path(0))?;
    f.write_all(msg)?;
    f.write_all(b"\n")?;
    Ok(())
}
/// Diagnostics trace: one text file, `%LOCALAPPDATA%\planor\trace.log`, every line stamped with UTC
/// wall-clock time (HH:MM:SS.mmm). A file (not stderr) so it works however the daemon was started.
static TRACE: std::sync::OnceLock<std::sync::Mutex<Option<fs::File>>> = std::sync::OnceLock::new();
pub fn stamp() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let s = d.as_secs() % 86400;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        s / 3600,
        s / 60 % 60,
        s % 60,
        d.subsec_millis()
    )
}
pub fn trace(line: impl AsRef<str>) {
    // Unit and simulated tests run the engine in-process: they must never touch the owner's trace (the first
    // line used to rename the running daemon's trace.log away and interleave test lines into recordings).
    if cfg!(test) {
        let _ = line;
        return;
    }
    use std::io::Write;
    let slot = TRACE.get_or_init(|| {
        let file = std::env::var("LOCALAPPDATA").ok().and_then(|dir| {
            let dir = std::path::PathBuf::from(dir).join("planor");
            let _ = fs::create_dir_all(&dir);
            let path = dir.join("trace.log");
            // Keep the previous run, start a fresh file each daemon start.
            let _ = fs::rename(&path, dir.join("trace.prev.log"));
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
        std::sync::Mutex::new(file)
    });
    if let Ok(mut guard) = slot.lock()
        && let Some(f) = guard.as_mut()
    {
        let _ = writeln!(f, "{} {}", stamp(), line.as_ref());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn caps() {
        let d = std::env::temp_dir().join(format!("planor-log-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        for _ in 0..10 {
            append(&d, b"1234", 8, 2).unwrap()
        }
        assert_eq!(fs::read_dir(&d).unwrap().count(), 2);
        fs::remove_dir_all(d).unwrap()
    }
}

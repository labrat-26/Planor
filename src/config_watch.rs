//! Hot reload of hand edits: watches the folder holding `planor.toml` and, once the file has been quiet for
//! 300 ms and really changed, tells the daemon to reload. Runs on its own thread and only ever talks to the
//! daemon through its pipe, so it can never stall tiling.
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};
use windows_sys::Win32::{
    Foundation::WAIT_OBJECT_0,
    Storage::FileSystem::{
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
        FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification,
    },
    System::Threading::{INFINITE, WaitForSingleObject},
};

/// Fires once, `quiet` after the last of a burst of events (an editor's save is several writes and renames).
pub struct Debounce {
    quiet: Duration,
    last: Option<Instant>,
}
impl Debounce {
    pub fn new(quiet: Duration) -> Self {
        Self { quiet, last: None }
    }
    pub fn event(&mut self, now: Instant) {
        self.last = Some(now);
    }
    /// True exactly once per burst, when it has been quiet long enough.
    pub fn due(&mut self, now: Instant) -> bool {
        match self.last {
            Some(t) if now.duration_since(t) >= self.quiet => {
                self.last = None;
                true
            }
            _ => false,
        }
    }
    pub fn pending(&self) -> bool {
        self.last.is_some()
    }
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// Watch `path`'s folder on a background thread; `on_change` runs after each settled change of the file.
pub fn spawn(path: PathBuf, on_change: impl Fn() + Send + 'static) {
    let Some(dir) = path.parent().map(Path::to_path_buf) else {
        return;
    };
    std::thread::spawn(move || {
        crate::perf::register_thread("config-watch");
        let wide_dir = crate::wide(&dir.to_string_lossy());
        let handle = unsafe {
            FindFirstChangeNotificationW(
                wide_dir.as_ptr(),
                0,
                FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_SIZE,
            )
        };
        if handle.is_null() || handle as isize == -1 {
            crate::diag::trace("config watcher: cannot watch the config folder");
            return;
        }
        let mut seen = stamp(&path);
        let mut debounce = Debounce::new(Duration::from_millis(300));
        loop {
            let wait = if debounce.pending() { 50 } else { INFINITE };
            if unsafe { WaitForSingleObject(handle, wait) } == WAIT_OBJECT_0 {
                debounce.event(Instant::now());
                if unsafe { FindNextChangeNotification(handle) } == 0 {
                    break;
                }
            }
            if debounce.due(Instant::now()) {
                let now = stamp(&path);
                if now != seen {
                    seen = now;
                    crate::diag::trace("config watcher: planor.toml changed, reloading");
                    on_change();
                }
            }
        }
        unsafe { FindCloseChangeNotification(handle) };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_fires_once_after_quiet_period() {
        let t0 = Instant::now();
        let mut d = Debounce::new(Duration::from_millis(300));
        assert!(!d.due(t0), "no event, nothing due");
        for ms in [0, 20, 50] {
            d.event(t0 + Duration::from_millis(ms));
        }
        assert!(d.pending());
        assert!(
            !d.due(t0 + Duration::from_millis(349)),
            "300 ms after the LAST event, not the first"
        );
        assert!(d.due(t0 + Duration::from_millis(350)));
        assert!(
            !d.due(t0 + Duration::from_millis(900)),
            "fires exactly once"
        );
        assert!(!d.pending());
    }

    #[test]
    fn debounce_rearms_after_firing() {
        let t0 = Instant::now();
        let mut d = Debounce::new(Duration::from_millis(100));
        d.event(t0);
        assert!(d.due(t0 + Duration::from_millis(100)));
        d.event(t0 + Duration::from_secs(5));
        assert!(!d.due(t0 + Duration::from_millis(5050)));
        assert!(d.due(t0 + Duration::from_millis(5100)));
    }

    #[test]
    fn watcher_reports_a_real_edit_once() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let dir = std::env::temp_dir().join(format!("planor-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("planor.toml");
        std::fs::write(&file, "[layout]\ngap_px = 6\n").unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        spawn(file.clone(), move || {
            h.fetch_add(1, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(dir.join("other.txt"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "another file in the folder is not a config change"
        );
        std::fs::write(&file, "[layout]\ngap_px = 9\n").unwrap();
        std::fs::write(&file, "[layout]\ngap_px = 10\n").unwrap();
        std::thread::sleep(Duration::from_millis(900));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a burst of writes reloads once"
        );
    }
}

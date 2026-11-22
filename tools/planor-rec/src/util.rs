//! Shared types, the QueryPerformanceCounter clock and a few Windows helpers.

use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// A rectangle in physical pixels, top-left origin (Windows desktop coordinates).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(self) -> i32 {
        self.y + self.h
    }
    pub fn is_empty(self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }
    pub fn area(self) -> i64 {
        self.w.max(0) as i64 * self.h.max(0) as i64
    }
    pub fn intersection(self, other: Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        Rect {
            x,
            y,
            w: self.right().min(other.right()) - x,
            h: self.bottom().min(other.bottom()) - y,
        }
    }
    pub fn overlap_area(self, other: Rect) -> i64 {
        self.intersection(other).area()
    }
    pub fn expand(self, px: i32) -> Rect {
        Rect::new(self.x - px, self.y - px, self.w + 2 * px, self.h + 2 * px)
    }
    pub fn inset(self, px: i32) -> Rect {
        self.expand(-px)
    }
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect {
            x,
            y,
            w: self.right().max(other.right()) - x,
            h: self.bottom().max(other.bottom()) - y,
        }
    }
    pub fn clamp_to(self, bounds: Rect) -> Rect {
        let r = self.intersection(bounds);
        Rect::new(r.x.max(bounds.x), r.y.max(bounds.y), r.w.max(0), r.h.max(0))
    }
    pub fn max_side_diff(self, other: Rect) -> i32 {
        [
            (self.x - other.x).abs(),
            (self.y - other.y).abs(),
            (self.right() - other.right()).abs(),
            (self.bottom() - other.bottom()).abs(),
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
    }
    pub fn to_array(self) -> [i32; 4] {
        [self.x, self.y, self.w, self.h]
    }
    pub fn from_array(v: [i32; 4]) -> Self {
        Rect::new(v[0], v[1], v[2], v[3])
    }
}

// ---- QueryPerformanceCounter -------------------------------------------------------------------

#[cfg(windows)]
pub fn qpc_frequency() -> i64 {
    use windows_sys::Win32::System::Performance::QueryPerformanceFrequency;
    let mut f: i64 = 0;
    unsafe { QueryPerformanceFrequency(&mut f) };
    if f <= 0 { 10_000_000 } else { f }
}

#[cfg(not(windows))]
pub fn qpc_frequency() -> i64 {
    10_000_000
}

#[cfg(windows)]
pub fn qpc_ticks() -> i64 {
    use windows_sys::Win32::System::Performance::QueryPerformanceCounter;
    let mut c: i64 = 0;
    unsafe { QueryPerformanceCounter(&mut c) };
    c
}

#[cfg(not(windows))]
pub fn qpc_ticks() -> i64 {
    unix_ms() * 10_000
}

pub fn ticks_to_us(ticks: i64, frequency: i64) -> i64 {
    if frequency <= 0 {
        return ticks;
    }
    ((ticks as i128 * 1_000_000i128) / frequency as i128) as i64
}

pub fn us_to_ticks(us: i64, frequency: i64) -> i64 {
    if frequency <= 0 {
        return us;
    }
    ((us as i128 * frequency as i128) / 1_000_000i128) as i64
}

pub fn qpc_us() -> i64 {
    ticks_to_us(qpc_ticks(), qpc_frequency())
}

/// Run the calling thread above normal threads (the capture tick loop and its rotation helpers). Best effort.
pub fn raise_current_thread_priority() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

/// Keep Windows from running the recorder slowly. A process started from a background context (a test run
/// from a terminal, a minimised IDE) can be put into "efficiency mode" (EcoQoS: low clocks, efficiency cores),
/// which turned a 3 ms frame rotation into 40 to 100 ms and stalled the capture loop. Opt out of execution
/// speed throttling and run slightly above normal priority. Best effort; failures are ignored.
pub fn run_at_full_speed() {
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
    };
    unsafe {
        let process = GetCurrentProcess();
        let mut state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        SetProcessInformation(
            process,
            ProcessPowerThrottling,
            (&mut state as *mut PROCESS_POWER_THROTTLING_STATE).cast(),
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
        SetPriorityClass(process, ABOVE_NORMAL_PRIORITY_CLASS);
    }
}

// ---- Wall clock (UTC) --------------------------------------------------------------------------

pub fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Days since 1970-01-01 to a proleptic Gregorian civil date (Howard Hinnant's algorithm).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Civil date to days since 1970-01-01.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn iso_utc(unix_ms: i64) -> String {
    let secs = unix_ms.div_euclid(1000);
    let millis = unix_ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        sod / 3600,
        sod / 60 % 60,
        sod % 60
    )
}

/// Parse the ISO-8601 UTC timestamps this tool writes (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
pub fn parse_iso_utc(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: u32 = d.next()?.parse().ok()?;
    let da: u32 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let sec: f64 = t.next()?.parse().ok()?;
    let secs = sec.floor() as i64;
    let millis = ((sec - sec.floor()) * 1000.0).round() as i64;
    Some(
        days_from_civil(y, mo, da) * 86_400_000
            + (h * 3600 + mi * 60 + secs) * 1000
            + millis,
    )
}

/// The UTC day (milliseconds) that contains `unix_ms`.
pub fn utc_day_start_ms(unix_ms: i64) -> i64 {
    unix_ms.div_euclid(86_400_000) * 86_400_000
}

// ---- FFmpeg ------------------------------------------------------------------------------------

/// Find `ffmpeg.exe`: `PLANOR_FFMPEG`, then PATH, then the Winget Gyan full build, then essentials.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PLANOR_FFMPEG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("ffmpeg.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let candidates = [
        local.join(
            "Microsoft/WinGet/Packages/Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe/ffmpeg-9.0.2-full_build/bin/ffmpeg.exe",
        ),
        local.join(
            "Microsoft/WinGet/Packages/Gyan.FFmpeg.Essentials_Microsoft.Winget.Source_8wekyb3d8bbwe/ffmpeg-9.0.1-essentials_build/bin/ffmpeg.exe",
        ),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

pub fn write_json(path: &Path, value: &impl Serialize) -> anyhow_lite::Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    std::fs::write(path, text + "\n")?;
    Ok(())
}

/// Tiny stand-in for `anyhow` so the crate needs no extra dependency.
pub mod anyhow_lite {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

    #[derive(Debug)]
    pub struct Error(pub String);
    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for Error {}

    pub fn err<T>(msg: impl Into<String>) -> Result<T> {
        Err(Box::new(Error(msg.into())))
    }
}

pub fn fmt_err(msg: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(anyhow_lite::Error(msg.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_round_trip() {
        for ms in [
            0,
            1_700_000_000_000,
            parse_iso_utc("2026-11-22T05:30:43.996Z").unwrap(),
        ] {
            let s = iso_utc(ms);
            assert_eq!(parse_iso_utc(&s).unwrap(), ms, "{s}");
        }
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn ticks_convert() {
        assert_eq!(ticks_to_us(10_000_000, 10_000_000), 1_000_000);
        assert_eq!(us_to_ticks(1_000_000, 10_000_000), 10_000_000);
    }

    #[test]
    fn rect_math() {
        let a = Rect::new(10, 10, 100, 50);
        let b = Rect::new(50, 30, 100, 50);
        assert_eq!(a.intersection(b), Rect::new(50, 30, 60, 30));
        assert_eq!(a.union(b), Rect::new(10, 10, 140, 70));
        assert_eq!(a.max_side_diff(Rect::new(13, 10, 100, 50)), 3);
    }
}

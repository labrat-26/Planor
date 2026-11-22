//! Continuous low-cost 10-second rolling video & event ring-buffer (`planor-rec ring`).
//!
//! Keeps only the last N seconds on a wrapped 1-second MPEG-TS segment wheel per monitor plus
//! bounded in-memory frame-timestamp and event rings, so freezing or user-triggered dumps (`dump`)
//! write a complete, zero-reencode MP4 + `frames-N.csv` + `events.jsonl` + `meta.json` aligned to
//! the exact QPC microsecond clock used by `planor`.

use crate::{
    capture::{self, DxgiOpener, Duplicator, MonitorInfo},
    events::{self, EventWriter},
    pipe::{self, CommandHandler},
    record::{default_trace_path, select_monitors, CaptureStats, Meta, ENCODER_QUEUE_FRAMES},
    timeline::{self, FrameSink, Push, RealClock, Row, TimelineCfg, CSV_HEADER},
    util::{self, qpc_frequency, qpc_us, unix_ms, Rect},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    ffi::c_void,
    fs::{self, File},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::{
        Console::SetConsoleCtrlHandler,
        Threading::{GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

const SEGMENT_BYTES_PACKET: usize = 188;
const DEFAULT_TRACE_TAIL_LINES: usize = 4000;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FileTime {
    low: u32,
    high: u32,
}

impl FileTime {
    fn as_us(&self) -> u64 {
        let ticks = ((self.high as u64) << 32) | (self.low as u64);
        ticks / 10
    }
}

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCountersEx {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetProcessTimes(
        hProcess: *mut c_void,
        lpCreationTime: *mut FileTime,
        lpExitTime: *mut FileTime,
        lpKernelTime: *mut FileTime,
        lpUserTime: *mut FileTime,
    ) -> i32;
    fn K32GetProcessMemoryInfo(
        Process: *mut c_void,
        ppsmemCounters: *mut ProcessMemoryCountersEx,
        cb: u32,
    ) -> i32;
}

pub struct RingArgs {
    pub seconds: u32,
    pub fps: u32,
    pub monitors: String,
    pub preset: String,
    pub encoder_threads: u32,
    pub pipe: Option<String>,
    pub trace: Option<PathBuf>,
    pub dir: Option<PathBuf>,
}

static RING_CTRL_C: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn ring_ctrl_handler(_ctrl_type: u32) -> i32 {
    RING_CTRL_C.store(true, Ordering::SeqCst);
    1
}

fn default_ring_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("planor")
        .join("ring")
}

// ---- Rolling frame timestamp ring --------------------------------------------------------------

#[derive(Debug)]
pub struct FrameRing {
    rows: VecDeque<(u64, Row)>,
    max_rows: usize,
}

impl FrameRing {
    pub fn new(max_rows: usize) -> Self {
        Self {
            rows: VecDeque::new(),
            max_rows: max_rows.max(16),
        }
    }

    pub fn push(&mut self, written_idx: u64, row: Row) {
        self.rows.push_back((written_idx, row));
        while self.rows.len() > self.max_rows {
            self.rows.pop_front();
        }
    }

    /// Return rows whose original written index lies in `[start_idx, end_idx]` (inclusive),
    /// re-indexed from 0 for the dumped clip.
    pub fn slice_range(&self, start_idx: u64, end_idx: u64) -> Vec<Row> {
        let mut out = Vec::new();
        for &(idx, mut row) in &self.rows {
            if idx >= start_idx && idx <= end_idx {
                row.tick = out.len() as u64;
                out.push(row);
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

struct RingItem {
    buf: Vec<u8>,
    row: Row,
}

struct RingFrameSink {
    tx: Option<SyncSender<RingItem>>,
    pool: Receiver<Vec<u8>>,
    closed: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    handle: Option<JoinHandle<RingWriterStats>>,
}

struct RingWriterStats {
    written: u64,
    error: Option<String>,
}

impl RingFrameSink {
    fn new(
        mut video: Box<dyn Write + Send>,
        ring: Arc<Mutex<FrameRing>>,
        written_counter: Arc<AtomicU64>,
        dropped_counter: Arc<AtomicU64>,
        capacity: usize,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<RingItem>(capacity.max(1));
        let (pool_tx, pool) = mpsc::channel::<Vec<u8>>();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_w = closed.clone();
        let handle = thread::spawn(move || {
            let mut stats = RingWriterStats {
                written: 0,
                error: None,
            };
            while let Ok(item) = rx.recv() {
                if let Err(e) = video.write_all(&item.buf) {
                    stats.error = Some(format!("FFmpeg pipe closed: {e}"));
                    closed_w.store(true, Ordering::SeqCst);
                    break;
                }
                let idx = stats.written;
                if let Ok(mut guard) = ring.lock() {
                    guard.push(idx, item.row);
                }
                stats.written += 1;
                written_counter.store(stats.written, Ordering::Relaxed);
                let _ = pool_tx.send(item.buf);
            }
            drop(video);
            stats
        });
        Self {
            tx: Some(tx),
            pool,
            closed,
            dropped: dropped_counter,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> RingWriterStats {
        self.tx.take();
        match self.handle.take().map(|h| h.join()) {
            Some(Ok(stats)) => stats,
            _ => RingWriterStats {
                written: 0,
                error: Some("ring writer thread panicked".into()),
            },
        }
    }
}

impl FrameSink for RingFrameSink {
    fn push(&mut self, bytes: &[u8], row: Row) -> Push {
        if self.closed.load(Ordering::SeqCst) {
            return Push::Closed;
        }
        let Some(tx) = &self.tx else {
            return Push::Closed;
        };
        let mut buf = self.pool.try_recv().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(bytes);
        match tx.try_send(RingItem { buf, row }) {
            Ok(()) => Push::Written,
            Err(TrySendError::Full(item)) => {
                let _ = item.buf;
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Push::Dropped
            }
            Err(TrySendError::Disconnected(_)) => {
                self.closed.store(true, Ordering::SeqCst);
                Push::Closed
            }
        }
    }
}

// ---- Rolling trace tail ------------------------------------------------------------------------

struct TraceTail {
    source: Option<PathBuf>,
    lines: VecDeque<String>,
    last_len: u64,
}

impl TraceTail {
    fn new(source: Option<PathBuf>) -> Self {
        Self {
            source,
            lines: VecDeque::new(),
            last_len: 0,
        }
    }

    fn refresh(&mut self) {
        let Some(src) = self.source.clone().or_else(default_trace_path) else {
            return;
        };
        let Ok(meta) = fs::metadata(&src) else {
            return;
        };
        if meta.len() == self.last_len && !self.lines.is_empty() {
            return;
        }
        if let Ok(raw) = fs::read_to_string(&src) {
            self.last_len = meta.len();
            self.lines.clear();
            for line in raw.lines() {
                if self.lines.len() >= DEFAULT_TRACE_TAIL_LINES {
                    self.lines.pop_front();
                }
                self.lines.push_back(line.to_string());
            }
        }
    }

    fn snapshot(&mut self) -> Vec<String> {
        self.refresh();
        self.lines.iter().cloned().collect()
    }
}

// ---- Segment parsing & selection ---------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct SegmentEntry {
    pub filename: String,
    pub start_sec: f64,
    pub end_sec: f64,
}

pub fn parse_segment_csv(raw: &str) -> Vec<SegmentEntry> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split(',');
        let Some(name) = parts.next() else { continue };
        let Some(start) = parts.next().and_then(|v| v.trim().parse::<f64>().ok()) else {
            continue;
        };
        let Some(end) = parts.next().and_then(|v| v.trim().parse::<f64>().ok()) else {
            continue;
        };
        if !name.is_empty() && end >= start {
            out.push(SegmentEntry {
                filename: name.trim().to_string(),
                start_sec: start,
                end_sec: end,
            });
        }
    }
    out
}

/// Select the trailing segments whose span covers up to `seconds`, skipping the oldest entry if the
/// wrap buffer is full so FFmpeg cannot overwrite a file while it is being copied.
pub fn select_segments(entries: &[SegmentEntry], seconds: u32, wrap: u32) -> Vec<SegmentEntry> {
    if entries.is_empty() {
        return Vec::new();
    }
    let start_guard = if entries.len() >= wrap as usize && entries.len() > 2 {
        1
    } else {
        0
    };
    let safe = &entries[start_guard..];
    let target_end = safe.last().map(|s| s.end_sec).unwrap_or(0.0);
    let cutoff = (target_end - seconds as f64).max(0.0);
    let mut chosen: Vec<SegmentEntry> = safe
        .iter()
        .filter(|s| s.end_sec > cutoff || (target_end - cutoff) < 0.25)
        .cloned()
        .collect();
    if chosen.is_empty() {
        if let Some(last) = safe.last() {
            chosen.push(last.clone());
        }
    }
    chosen
}

/// Copy a possibly in-progress MPEG-TS file cleanly, truncating to whole 188-byte TS packets.
pub fn copy_ts_aligned(src: &Path, dst: &Path) -> std::io::Result<u64> {
    let mut buf = Vec::new();
    File::open(src)?.read_to_end(&mut buf)?;
    let usable = (buf.len() / SEGMENT_BYTES_PACKET) * SEGMENT_BYTES_PACKET;
    if usable < SEGMENT_BYTES_PACKET {
        return Ok(0);
    }
    fs::write(dst, &buf[..usable])?;
    Ok(usable as u64)
}

pub fn write_concat_file(paths: &[PathBuf], dst: &Path) -> std::io::Result<()> {
    let mut body = String::new();
    for p in paths {
        let s = p.to_string_lossy().replace('\'', "'\\''");
        body.push_str(&format!("file '{s}'\n"));
    }
    fs::write(dst, body)
}

pub fn write_frames_csv(dst: &Path, rows: &[Row]) -> std::io::Result<()> {
    let file = File::create(dst)?;
    let mut w = BufWriter::new(file);
    w.write_all(CSV_HEADER.as_bytes())?;
    for row in rows {
        w.write_all(row.csv_line().as_bytes())?;
    }
    w.flush()?;
    Ok(())
}

// ---- Ring state & resource monitoring ----------------------------------------------------------

struct MonitorRingHandle {
    info: MonitorInfo,
    frame_ring: Arc<Mutex<FrameRing>>,
    written: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    ticks: AtomicU64,
    lost_ticks: AtomicU64,
    lost_periods: AtomicU64,
    reopens: AtomicU64,
    max_lost_ms: AtomicI64,
    child_pid: Arc<AtomicU32>,
    seg_csv: PathBuf,
    scratch: PathBuf,
}

#[derive(Default)]
struct CpuSampleState {
    last_wall_us: i64,
    last_cpu_us: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RingStatus {
    pub ok: bool,
    pub uptime_s: f64,
    pub monitors: usize,
    pub seconds: u32,
    pub fps: u32,
    pub wrap_segments: u32,
    pub frames_written: u64,
    pub frames_dropped: u64,
    pub events_in_ring: usize,
    pub cpu_pct: f64,
    pub working_set_bytes: u64,
    pub working_set_mb: f64,
    pub disk_bytes: u64,
    pub disk_mb: f64,
    pub dumps: u64,
    pub ring_dir: String,
}

struct RingCoordinator {
    seconds: u32,
    fps: u32,
    wrap: u32,
    started_at: Instant,
    qpc_frequency: i64,
    scratch_dir: PathBuf,
    all_monitors: Vec<MonitorInfo>,
    taskbars: Vec<Rect>,
    monitors: Vec<Arc<MonitorRingHandle>>,
    writer: Arc<EventWriter>,
    trace_tail: Arc<Mutex<TraceTail>>,
    topology_log: Arc<Mutex<Vec<Value>>>,
    dumps_count: AtomicU64,
    dump_lock: Mutex<()>,
    cpu_sample: Mutex<CpuSampleState>,
}

fn process_cpu_and_ram(pid: Option<u32>) -> (u64, u64) {
    unsafe {
        let handle = match pid {
            Some(0) => return (0, 0),
            Some(p) => OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, p),
            None => GetCurrentProcess(),
        };
        if handle.is_null() {
            return (0, 0);
        }
        let mut c = FileTime::default();
        let mut e = FileTime::default();
        let mut k = FileTime::default();
        let mut u = FileTime::default();
        let cpu_us = if GetProcessTimes(handle, &mut c, &mut e, &mut k, &mut u) != 0 {
            k.as_us().saturating_add(u.as_us())
        } else {
            0
        };
        let mut mem = ProcessMemoryCountersEx {
            cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
            ..Default::default()
        };
        let ram = if K32GetProcessMemoryInfo(
            handle,
            &mut mem,
            std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
        ) != 0
        {
            mem.working_set_size as u64
        } else {
            0
        };
        if pid.is_some() {
            CloseHandle(handle);
        }
        (cpu_us, ram)
    }
}

fn dir_bytes(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| fs::metadata(e.path()).ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

impl RingCoordinator {
    fn status(&self) -> RingStatus {
        let now_us = qpc_us();
        let (mut total_cpu_us, mut total_ram) = process_cpu_and_ram(None);
        let mut frames_written = 0u64;
        let mut frames_dropped = 0u64;
        for m in &self.monitors {
            frames_written += m.written.load(Ordering::Relaxed);
            frames_dropped += m.dropped.load(Ordering::Relaxed);
            let pid = m.child_pid.load(Ordering::Relaxed);
            if pid != 0 {
                let (c, r) = process_cpu_and_ram(Some(pid));
                total_cpu_us += c;
                total_ram += r;
            }
        }
        let cpu_pct = if let Ok(mut sample) = self.cpu_sample.lock() {
            let pct = if sample.last_wall_us > 0 && now_us > sample.last_wall_us {
                let wall = (now_us - sample.last_wall_us) as f64;
                let cpu = total_cpu_us.saturating_sub(sample.last_cpu_us) as f64;
                let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
                ((cpu / wall) * 100.0 / cpus).clamp(0.0, 100.0)
            } else {
                0.0
            };
            sample.last_wall_us = now_us;
            sample.last_cpu_us = total_cpu_us;
            pct
        } else {
            0.0
        };
        let disk = dir_bytes(&self.scratch_dir);
        RingStatus {
            ok: true,
            uptime_s: self.started_at.elapsed().as_secs_f64(),
            monitors: self.monitors.len(),
            seconds: self.seconds,
            fps: self.fps,
            wrap_segments: self.wrap,
            frames_written,
            frames_dropped,
            events_in_ring: self.writer.ring_len(),
            cpu_pct: (cpu_pct * 100.0).round() / 100.0,
            working_set_bytes: total_ram,
            working_set_mb: ((total_ram as f64 / 1_048_576.0) * 100.0).round() / 100.0,
            disk_bytes: disk,
            disk_mb: ((disk as f64 / 1_048_576.0) * 100.0).round() / 100.0,
            dumps: self.dumps_count.load(Ordering::Relaxed),
            ring_dir: self.scratch_dir.display().to_string(),
        }
    }

    fn dump(&self, out: &Path, reason: &str) -> Result<String, String> {
        let _guard = self.dump_lock.lock().map_err(|_| "dump lock poisoned".to_string())?;
        util::ensure_dir(out).map_err(|e| e.to_string())?;
        let out = std::path::absolute(out).unwrap_or_else(|_| out.to_path_buf());
        let tmp = out.join(".ring-tmp");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).map_err(|e| format!("creating tmp: {e}"))?;

        let ffmpeg_path = util::find_ffmpeg().ok_or_else(|| "FFmpeg not found".to_string())?;
        let dump_qpc = qpc_us();
        let mut frame_counts = Vec::with_capacity(self.monitors.len());
        let mut capture_stats = Vec::with_capacity(self.monitors.len());
        let mut min_qpc: Option<i64> = None;
        let mut max_qpc: Option<i64> = None;

        for m in &self.monitors {
            let idx = m.info.index;
            let raw_csv = fs::read_to_string(&m.seg_csv).unwrap_or_default();
            let entries = parse_segment_csv(&raw_csv);
            let selected = select_segments(&entries, self.seconds, self.wrap);

            let mut seg_paths = Vec::new();
            for entry in &selected {
                let src = m.scratch.join(&entry.filename);
                let dst = tmp.join(&entry.filename);
                if let Ok(bytes) = copy_ts_aligned(&src, &dst) {
                    if bytes >= SEGMENT_BYTES_PACKET as u64 {
                        seg_paths.push(dst);
                    }
                }
            }

            let mut dumped_rows = Vec::new();
            if !seg_paths.is_empty() {
                let first_sec = selected.first().map(|s| s.start_sec).unwrap_or(0.0);
                let last_sec = selected.last().map(|s| s.end_sec).unwrap_or(first_sec);
                let start_idx = (first_sec * self.fps as f64).round().max(0.0) as u64;
                let end_idx = (last_sec * self.fps as f64).ceil().max(start_idx as f64) as u64;
                if let Ok(ring) = m.frame_ring.lock() {
                    dumped_rows = ring.slice_range(start_idx, end_idx);
                }

                let concat_path = tmp.join(format!("concat-{idx}.txt"));
                write_concat_file(&seg_paths, &concat_path).map_err(|e| e.to_string())?;
                let mp4_path = out.join(format!("video-{idx}.mp4"));
                let status = Command::new(&ffmpeg_path)
                    .args([
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-y",
                        "-fflags",
                        "+genpts",
                        "-avoid_negative_ts",
                        "make_zero",
                        "-f",
                        "concat",
                        "-safe",
                        "0",
                    ])
                    .arg("-i")
                    .arg(&concat_path)
                    .args(["-c", "copy", "-movflags", "+faststart"])
                    .arg(&mp4_path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map_err(|e| format!("running ffmpeg concat: {e}"))?;
                if !status.success() {
                    return Err(format!("ffmpeg concat monitor {idx} exited with {status}"));
                }
            }

            if let Some(first) = dumped_rows.first() {
                min_qpc = Some(min_qpc.map_or(first.present_us, |v| v.min(first.present_us)));
            }
            if let Some(last) = dumped_rows.last() {
                max_qpc = Some(max_qpc.map_or(last.present_us, |v| v.max(last.present_us)));
            }
            write_frames_csv(&out.join(format!("frames-{idx}.csv")), &dumped_rows)
                .map_err(|e| format!("frames-{idx}.csv: {e}"))?;
            let count = dumped_rows.len() as u64;
            frame_counts.push(count);
            capture_stats.push(CaptureStats {
                monitor: idx,
                ticks: m.ticks.load(Ordering::Relaxed),
                written: count,
                dropped: m.dropped.load(Ordering::Relaxed),
                lost_ticks: m.lost_ticks.load(Ordering::Relaxed),
                lost_periods: m.lost_periods.load(Ordering::Relaxed),
                reopens: m.reopens.load(Ordering::Relaxed),
                max_lost_ms: m.max_lost_ms.load(Ordering::Relaxed),
            });
        }

        let clip_start_us = min_qpc.unwrap_or_else(|| dump_qpc.saturating_sub(self.seconds as i64 * 1_000_000));
        let clip_end_us = max_qpc.unwrap_or(dump_qpc);

        let mut events = self.writer.snapshot_since(clip_start_us);
        events.push(json!({
            "kind": "mark",
            "text": format!("dump: {reason}"),
            "source": "ring-dump",
            "t": dump_qpc,
        }));
        {
            let ef = File::create(out.join("events.jsonl")).map_err(|e| e.to_string())?;
            let mut bw = BufWriter::new(ef);
            for ev in &events {
                let _ = serde_json::to_writer(&mut bw, ev);
                let _ = bw.write_all(b"\n");
            }
            let _ = bw.flush();
        }

        if let Ok(mut tail) = self.trace_tail.lock() {
            let lines = tail.snapshot();
            if !lines.is_empty() {
                let _ = fs::write(out.join("trace.log"), lines.join("\n"));
            }
        }

        let topo = self.topology_log.lock().map(|l| l.clone()).unwrap_or_default();
        let freq = self.qpc_frequency;
        let qpc_start_ticks = ((clip_start_us as i128 * freq as i128) / 1_000_000) as i64;
        let meta = Meta {
            tool: "planor-rec".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            qpc_frequency: freq,
            qpc_start: qpc_start_ticks,
            qpc_start_us: clip_start_us,
            utc_start: util::iso_utc(unix_ms()),
            utc_start_unix_ms: unix_ms(),
            fps: self.fps,
            monitors: self.all_monitors.clone(),
            taskbars: self.taskbars.clone(),
            mic: false,
            mic_device: None,
            qpc_end_us: Some(clip_end_us),
            utc_end: Some(util::iso_utc(unix_ms())),
            frames: frame_counts.clone(),
            hotkeys: Vec::new(),
            audio: None,
            capture: capture_stats,
            topology_changes: topo,
            stop_reason: Some(reason.to_string()),
        };
        util::write_json(&out.join("meta.json"), &meta).map_err(|e| e.to_string())?;
        let _ = fs::remove_dir_all(&tmp);
        self.dumps_count.fetch_add(1, Ordering::Relaxed);
        let total_frames: u64 = frame_counts.iter().sum();
        Ok(format!(
            "ok: dumped {} ({} monitor(s), {} frame(s))",
            out.display(),
            self.monitors.len(),
            total_frames
        ))
    }
}

#[derive(Deserialize)]
struct DumpPayload {
    out: PathBuf,
    #[serde(default)]
    reason: Option<String>,
}

fn parse_dump_request(rest: &str) -> Result<(PathBuf, String), String> {
    let trimmed = rest.trim();
    if trimmed.starts_with('{') {
        let p: DumpPayload =
            serde_json::from_str(trimmed).map_err(|e| format!("invalid dump payload: {e}"))?;
        return Ok((p.out, p.reason.unwrap_or_else(|| "manual".to_string())));
    }
    if trimmed.is_empty() {
        return Err("missing dump --out path".to_string());
    }
    Ok((PathBuf::from(trimmed), "manual".to_string()))
}

fn run_ring_monitor(
    handle: Arc<MonitorRingHandle>,
    fps: u32,
    wrap: u32,
    preset: String,
    encoder_threads: u32,
    stop: Arc<AtomicBool>,
    writer: Arc<EventWriter>,
    epoch: Arc<AtomicU64>,
) -> util::anyhow_lite::Result<()> {
    let idx = handle.info.index;
    let size = (handle.info.rect.w as u32, handle.info.rect.h as u32);
    let ffmpeg_path = util::find_ffmpeg().ok_or_else(|| util::fmt_err("FFmpeg not found"))?;
    let seg_pattern = handle.scratch.join(format!("seg-{idx}-%03d.ts"));
    let gop = fps.max(1).to_string();
    let mut child: Child = Command::new(ffmpeg_path)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pixel_format",
            "bgra",
            "-video_size",
            &format!("{}x{}", size.0, size.1),
            "-framerate",
            &fps.to_string(),
            "-i",
            "pipe:0",
            "-an",
            "-c:v",
            "libx264",
            "-preset",
            &preset,
            "-threads",
            &encoder_threads.to_string(),
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
            "-g",
            &gop,
            "-keyint_min",
            &gop,
            "-sc_threshold",
            "0",
            "-flags",
            "+cgop",
            "-f",
            "segment",
            "-segment_format",
            "mpegts",
            "-segment_time",
            "1",
            "-segment_wrap",
            &wrap.to_string(),
            "-segment_list",
        ])
        .arg(&handle.seg_csv)
        .args([
            "-segment_list_type",
            "csv",
            "-segment_list_flags",
            "+live",
            "-segment_list_size",
            &wrap.to_string(),
        ])
        .arg(&seg_pattern)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(handle.scratch.join(format!("ffmpeg-{idx}.log")))
                .map_err(|e| util::fmt_err(format!("ffmpeg ring log: {e}")))?,
        ))
        .spawn()
        .map_err(|e| util::fmt_err(format!("starting FFmpeg ring: {e}")))?;
    handle.child_pid.store(child.id(), Ordering::Relaxed);
    let stdin = child.stdin.take().ok_or_else(|| util::fmt_err("ffmpeg stdin"))?;

    let capture = match Duplicator::open_by_device(Some(&handle.info.device), idx) {
        Ok(c) => c,
        Err(e) => {
            drop(stdin);
            let _ = child.wait();
            handle.child_pid.store(0, Ordering::Relaxed);
            return util::anyhow_lite::err(format!("opening monitor {idx} for ring capture: {e}"));
        }
    };
    if (capture.width, capture.height) != size {
        drop(stdin);
        let _ = child.wait();
        handle.child_pid.store(0, Ordering::Relaxed);
        return util::anyhow_lite::err(format!(
            "monitor {idx}: capture is {}x{} but meta says {}x{}",
            capture.width, capture.height, size.0, size.1
        ));
    }

    let mut sink = RingFrameSink::new(
        Box::new(stdin),
        handle.frame_ring.clone(),
        handle.written.clone(),
        handle.dropped.clone(),
        ENCODER_QUEUE_FRAMES,
    );
    let mut cfg = TimelineCfg::new(idx, fps, size);
    cfg.topology_epoch = Some(epoch);
    let mut opener = DxgiOpener {
        device: handle.info.device.clone(),
        index: idx,
    };
    let result = timeline::run_timeline(
        &cfg,
        Box::new(capture),
        &mut opener,
        &mut sink as &mut dyn FrameSink,
        &*writer,
        &RealClock,
        &stop,
    );
    let _writer_stats = sink.finish();
    let _ = child.wait();
    handle.child_pid.store(0, Ordering::Relaxed);
    let stats = result.map_err(util::fmt_err)?;
    handle.ticks.store(stats.ticks, Ordering::Relaxed);
    handle.dropped.store(stats.dropped, Ordering::Relaxed);
    handle.lost_ticks.store(stats.lost_ticks, Ordering::Relaxed);
    handle.lost_periods.store(stats.lost_periods, Ordering::Relaxed);
    handle.reopens.store(stats.reopens, Ordering::Relaxed);
    handle.max_lost_ms.store(stats.max_lost_ms, Ordering::Relaxed);
    Ok(())
}

fn spawn_topology_watcher(
    initial: Vec<MonitorInfo>,
    stop: Arc<AtomicBool>,
    writer: Arc<EventWriter>,
    epoch: Arc<AtomicU64>,
    log: Arc<Mutex<Vec<Value>>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        capture::dpi_aware();
        let mut last = crate::record::topology_key(&initial);
        let mut next = Instant::now() + Duration::from_secs(2);
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
            if Instant::now() < next {
                continue;
            }
            next = Instant::now() + Duration::from_secs(2);
            let Ok(now_monitors) = capture::monitors() else {
                continue;
            };
            let key = crate::record::topology_key(&now_monitors);
            if key != last {
                last = key;
                let event = json!({
                    "kind": "capture",
                    "event": "topology",
                    "t": qpc_us(),
                    "monitors": now_monitors.iter().map(|m| json!({
                        "index": m.index, "device": m.device,
                        "rect": m.rect.to_array(), "work": m.work.to_array(),
                    })).collect::<Vec<_>>(),
                });
                if let Ok(mut l) = log.lock() {
                    l.push(event.clone());
                }
                writer.write(event);
                epoch.fetch_add(1, Ordering::SeqCst);
            }
        }
    })
}

pub fn run(args: RingArgs) -> util::anyhow_lite::Result<()> {
    let seconds = args.seconds.clamp(2, 120);
    let fps = args.fps.clamp(5, 120);
    let wrap = seconds + 5;
    let scratch_dir = args.dir.unwrap_or_else(default_ring_dir);
    let _ = fs::remove_dir_all(&scratch_dir);
    util::ensure_dir(&scratch_dir)?;

    capture::dpi_aware();
    let qpc_freq = qpc_frequency();
    let monitors =
        capture::monitors().map_err(|e| util::fmt_err(format!("enumerating monitors: {e}")))?;
    let taskbars = capture::taskbar_rects();
    let selected = select_monitors(&monitors, &args.monitors)?;

    let window_us = (seconds as i64 + 4) * 1_000_000;
    let writer = EventWriter::open_ring(window_us, 8192);
    writer.write(json!({
        "kind": "session",
        "event": "ring-start",
        "version": env!("CARGO_PKG_VERSION"),
        "monitors": selected,
        "seconds": seconds,
        "fps": fps,
        "qpc_frequency": qpc_freq,
    }));

    let trace_tail = Arc::new(Mutex::new(TraceTail::new(args.trace.clone())));
    let max_rows = ((seconds + 6) * fps) as usize;
    let mut monitor_handles = Vec::new();
    for &idx in &selected {
        let info = monitors.iter().find(|m| m.index == idx).unwrap().clone();
        monitor_handles.push(Arc::new(MonitorRingHandle {
            info,
            frame_ring: Arc::new(Mutex::new(FrameRing::new(max_rows))),
            written: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            ticks: AtomicU64::new(0),
            lost_ticks: AtomicU64::new(0),
            lost_periods: AtomicU64::new(0),
            reopens: AtomicU64::new(0),
            max_lost_ms: AtomicI64::new(0),
            child_pid: Arc::new(AtomicU32::new(0)),
            seg_csv: scratch_dir.join(format!("seg-{idx}.csv")),
            scratch: scratch_dir.clone(),
        }));
    }

    let topology_log = Arc::new(Mutex::new(Vec::<Value>::new()));
    let coord = Arc::new(RingCoordinator {
        seconds,
        fps,
        wrap,
        started_at: Instant::now(),
        qpc_frequency: qpc_freq,
        scratch_dir: scratch_dir.clone(),
        all_monitors: monitors.clone(),
        taskbars,
        monitors: monitor_handles.clone(),
        writer: writer.clone(),
        trace_tail: trace_tail.clone(),
        topology_log: topology_log.clone(),
        dumps_count: AtomicU64::new(0),
        dump_lock: Mutex::new(()),
        cpu_sample: Mutex::new(CpuSampleState::default()),
    });

    RING_CTRL_C.store(false, Ordering::SeqCst);
    let stop = Arc::new(AtomicBool::new(false));
    unsafe { SetConsoleCtrlHandler(Some(ring_ctrl_handler), 1) };

    let pipe_name = args.pipe.or_else(|| Some("ring".into()));
    let coord_handler = coord.clone();
    let handler: CommandHandler = Arc::new(move |verb, rest| match verb {
        "status" => serde_json::to_string(&coord_handler.status()).ok(),
        "dump" => match parse_dump_request(rest) {
            Ok((out, reason)) => match coord_handler.dump(&out, &reason) {
                Ok(msg) => Some(msg),
                Err(e) => Some(format!("error: {e}")),
            },
            Err(e) => Some(format!("error: {e}")),
        },
        _ => None,
    });
    let pipe_thread = pipe::spawn_server_with(writer.clone(), stop.clone(), pipe_name.clone(), Some(handler));
    let input = events::spawn_input(writer.clone(), stop.clone());
    let os = events::spawn_os(writer.clone(), stop.clone());
    let epoch = Arc::new(AtomicU64::new(0));
    let topology = spawn_topology_watcher(
        monitors,
        stop.clone(),
        writer.clone(),
        epoch.clone(),
        topology_log,
    );

    let trace_stop = stop.clone();
    let trace_tail_bg = trace_tail.clone();
    let trace_thread = thread::spawn(move || {
        while !trace_stop.load(Ordering::Relaxed) {
            if let Ok(mut t) = trace_tail_bg.lock() {
                t.refresh();
            }
            for _ in 0..6 {
                if trace_stop.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    });

    let mut mon_threads = Vec::new();
    for mh in monitor_handles {
        let stop_m = stop.clone();
        let writer_m = writer.clone();
        let epoch_m = epoch.clone();
        let preset = args.preset.clone();
        let enc_threads = args.encoder_threads;
        let idx = mh.info.index;
        let jh = thread::spawn(move || {
            run_ring_monitor(mh, fps, wrap, preset, enc_threads, stop_m, writer_m, epoch_m)
        });
        mon_threads.push((idx, jh));
    }

    println!(
        "planor-rec: ring capturing {} monitor(s) ({seconds}s window @ {fps} fps, pipe {})",
        selected.len(),
        pipe::pipe_path(pipe_name.as_deref())
    );

    while !stop.load(Ordering::Relaxed) && !RING_CTRL_C.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(50));
    }
    stop.store(true, Ordering::SeqCst);

    for (idx, jh) in mon_threads {
        if let Ok(Err(e)) = jh.join() {
            eprintln!("planor-rec: ring monitor {idx}: {e}");
        }
    }
    let _ = input.join();
    let _ = os.join();
    let _ = topology.join();
    let _ = trace_thread.join();
    thread::sleep(Duration::from_millis(60));
    drop(pipe_thread);
    let _ = fs::remove_dir_all(&scratch_dir);
    println!("planor-rec: ring stopped cleanly");
    Ok(())
}

pub fn send_dump(out: &Path, reason: Option<&str>, pipe_name: Option<&str>) -> util::anyhow_lite::Result<()> {
    let abs = std::path::absolute(out).unwrap_or_else(|_| out.to_path_buf());
    let payload = json!({
        "out": abs,
        "reason": reason.unwrap_or("manual"),
    });
    let reply = pipe::send(&format!("dump {payload}"), pipe_name.or(Some("ring")))
        .map_err(|e| util::fmt_err(format!("ring pipe unavailable: {e}")))?;
    println!("{reply}");
    if reply.starts_with("error:") {
        return util::anyhow_lite::err(reply);
    }
    Ok(())
}

pub fn send_status(pipe_name: Option<&str>) -> util::anyhow_lite::Result<()> {
    let reply = pipe::send("status", pipe_name.or(Some("ring")))
        .map_err(|e| util::fmt_err(format!("ring pipe unavailable: {e}")))?;
    println!("{reply}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::FrameState;

    fn row_at(present_us: i64) -> Row {
        Row {
            tick: 0,
            present_us,
            changed: true,
            state: FrameState::Ok,
            rotate_stall_us: 0,
        }
    }

    #[test]
    fn event_ring_evicts_oldest_and_keeps_baseline_os() {
        let w = EventWriter::open_ring(1_000_000, 40);
        // Push an initial OS snapshot at t = 100_000, then push 50 input events past capacity.
        w.write(json!({"kind": "os", "t": 100_000, "windows": [{"hwnd": 1}]}));
        for i in 1..=50i64 {
            w.write(json!({"kind": "input", "t": 200_000 + i * 50_000, "seq": i}));
        }
        assert!(w.ring_len() <= 40, "ring length exceeded max_events");
        let snap = w.snapshot_since(2_000_000);
        assert!(
            snap.iter().any(|v| v.get("kind").and_then(|k| k.as_str()) == Some("os")),
            "expected preserved baseline OS snapshot even after oldest events evicted"
        );
        let input_seqs: Vec<i64> = snap
            .iter()
            .filter_map(|v| v.get("seq").and_then(|s| s.as_i64()))
            .collect();
        assert!(!input_seqs.is_empty());
        assert!(input_seqs.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(*input_seqs.last().unwrap(), 50);
    }

    #[test]
    fn frame_ring_bounds_capacity_and_reindexes_slice() {
        let mut fr = FrameRing::new(20);
        for i in 0..35u64 {
            fr.push(i, row_at(1_000_000 + (i as i64) * 33_333));
        }
        assert_eq!(fr.len(), 20);
        let sliced = fr.slice_range(20, 29);
        assert_eq!(sliced.len(), 10);
        assert_eq!(sliced[0].tick, 0);
        assert_eq!(sliced[0].present_us, 1_000_000 + 20 * 33_333);
        assert_eq!(sliced[9].tick, 9);
        assert_eq!(sliced[9].present_us, 1_000_000 + 29 * 33_333);
    }

    #[test]
    fn segment_selection_skips_wrap_slot_and_writes_concat_and_csv() {
        let raw = "\
seg-0-000.ts,0.000000,1.000000
seg-0-001.ts,1.000000,2.000000
seg-0-002.ts,2.000000,3.000000
seg-0-003.ts,3.000000,4.000000
seg-0-004.ts,4.000000,5.000000
seg-0-005.ts,5.000000,5.400000
";
        let entries = parse_segment_csv(raw);
        assert_eq!(entries.len(), 6);
        let chosen = select_segments(&entries, 3, 6);
        // Should skip seg-0-000.ts when buffer is full (len == wrap), covering > 2.4s..5.4s -> seg 002..005.
        assert_eq!(chosen[0].filename, "seg-0-002.ts");
        assert_eq!(chosen.last().unwrap().filename, "seg-0-005.ts");

        let dir = std::env::temp_dir().join(format!("planor-ring-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("partial.ts");
        let dst = dir.join("aligned.ts");
        // Write 188*2 + 45 trailing partial bytes: copy_ts_aligned must truncate cleanly to 376 bytes.
        fs::write(&src, vec![0x47u8; 188 * 2 + 45]).unwrap();
        let copied = copy_ts_aligned(&src, &dst).unwrap();
        assert_eq!(copied, 376);
        assert_eq!(fs::metadata(&dst).unwrap().len(), 376);

        let concat = dir.join("concat.txt");
        write_concat_file(std::slice::from_ref(&dst), &concat).unwrap();
        let txt = fs::read_to_string(&concat).unwrap();
        assert!(txt.contains("file '"));

        let csv_path = dir.join("frames-0.csv");
        write_frames_csv(&csv_path, &[row_at(100), Row { tick: 1, ..row_at(200) }]).unwrap();
        let csv_raw = fs::read_to_string(&csv_path).unwrap();
        assert!(csv_raw.starts_with("index,present_qpc_us,changed,state,rotate_stall_us\n"));
        assert!(csv_raw.contains("\n0,100,1,ok,0\n1,200,1,ok,0\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ffmpeg_segment_wrap_and_concat_remux_produces_mp4() {
        let Some(ffmpeg) = util::find_ffmpeg() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("planor-ring-ffmpeg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let seg_csv = dir.join("seg-0.csv");
        let seg_pat = dir.join("seg-0-%03d.ts");

        let status = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x180:rate=30:duration=2.5",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "30",
                "-keyint_min",
                "30",
                "-sc_threshold",
                "0",
                "-flags",
                "+cgop",
                "-f",
                "segment",
                "-segment_format",
                "mpegts",
                "-segment_time",
                "1",
                "-segment_wrap",
                "4",
                "-segment_list",
            ])
            .arg(&seg_csv)
            .args([
                "-segment_list_type",
                "csv",
                "-segment_list_flags",
                "+live",
                "-segment_list_size",
                "4",
            ])
            .arg(&seg_pat)
            .status()
            .expect("spawn ffmpeg testsrc");
        assert!(status.success(), "ffmpeg segment generation failed: {status}");

        let raw_csv = fs::read_to_string(&seg_csv).expect("seg-0.csv readable");
        let entries = parse_segment_csv(&raw_csv);
        assert!(!entries.is_empty(), "expected segment entries in seg-0.csv");
        let chosen = select_segments(&entries, 2, 4);
        assert!(!chosen.is_empty(), "expected selected segments");

        let tmp = dir.join("tmp");
        fs::create_dir_all(&tmp).unwrap();
        let mut aligned_paths = Vec::new();
        for e in &chosen {
            let dst = tmp.join(&e.filename);
            let bytes = copy_ts_aligned(&dir.join(&e.filename), &dst).unwrap();
            assert!(bytes >= SEGMENT_BYTES_PACKET as u64);
            aligned_paths.push(dst);
        }
        let concat_txt = tmp.join("concat-0.txt");
        write_concat_file(&aligned_paths, &concat_txt).unwrap();

        let mp4 = dir.join("video-0.mp4");
        let remux = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-fflags",
                "+genpts",
                "-avoid_negative_ts",
                "make_zero",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
            ])
            .arg(&concat_txt)
            .args(["-c", "copy", "-movflags", "+faststart"])
            .arg(&mp4)
            .status()
            .expect("spawn ffmpeg concat remux");
        assert!(remux.success(), "ffmpeg concat remux failed: {remux}");

        let bytes = fs::read(&mp4).expect("video-0.mp4 readable");
        assert!(bytes.len() > 2000, "unexpectedly small mp4 ({} bytes)", bytes.len());
        assert!(
            bytes.windows(4).any(|w| w == b"ftyp"),
            "missing MP4 ftyp header box"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

//! The recorder: DXGI frames -> FFmpeg H.264, plus the shared event stream, pipe control, global
//! hotkeys and the microphone.

use crate::{
    capture::{self, DxgiOpener, Duplicator, MonitorInfo},
    events::{self, EventWriter},
    pipe,
    timeline::{self, FrameSink, QueueSink, RealClock, TimelineCfg},
    util::{self, qpc_frequency, qpc_us, unix_ms, Rect},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::File,
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::GetLastError,
    System::{Console::SetConsoleCtrlHandler, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, VK_M,
        },
        WindowsAndMessaging::{GetMessageW, PostThreadMessageW, MSG, WM_HOTKEY, WM_QUIT},
    },
};

pub const MIC_DEVICE: &str = "Microphone Array (AMD Audio Device)";

/// Frames the encoder queue of one monitor may hold before frames are dropped on purpose.
pub const ENCODER_QUEUE_FRAMES: usize = 4;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HotkeyStatus {
    pub name: String,
    pub keys: String,
    pub ok: bool,
    #[serde(default)]
    pub error_code: Option<u32>,
    #[serde(default)]
    pub detail: String,
}

/// Where the microphone recording sits on the recorder's clock, so a dictated note can be lined up
/// with video frames: video frame `n` of a monitor is at `frames-N.csv` row `n`, audio sample `s` is at
/// `audio.start_qpc_us + s / sample_rate`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AudioMeta {
    pub file: String,
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// QPC µs when FFmpeg was started.
    pub spawn_qpc_us: i64,
    /// QPC µs when the stop request was sent.
    pub end_qpc_us: i64,
    /// Length of the WAV data in ms.
    pub duration_ms: i64,
    /// Estimated QPC µs of the first sample: stop time minus the length actually recorded.
    pub start_qpc_us: i64,
    /// `start_qpc_us` minus the recording's start, in ms (positive: the audio starts after the video).
    pub offset_to_video_start_ms: i64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CaptureStats {
    pub monitor: u32,
    pub ticks: u64,
    pub written: u64,
    pub dropped: u64,
    pub lost_ticks: u64,
    pub lost_periods: u64,
    pub reopens: u64,
    pub max_lost_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Meta {
    pub tool: String,
    pub version: String,
    pub qpc_frequency: i64,
    pub qpc_start: i64,
    pub qpc_start_us: i64,
    pub utc_start: String,
    pub utc_start_unix_ms: i64,
    pub fps: u32,
    pub monitors: Vec<MonitorInfo>,
    pub taskbars: Vec<Rect>,
    pub mic: bool,
    #[serde(default)]
    pub mic_device: Option<String>,
    #[serde(default)]
    pub qpc_end_us: Option<i64>,
    #[serde(default)]
    pub utc_end: Option<String>,
    #[serde(default)]
    pub frames: Vec<u64>,
    #[serde(default)]
    pub hotkeys: Vec<HotkeyStatus>,
    #[serde(default)]
    pub audio: Option<AudioMeta>,
    #[serde(default)]
    pub capture: Vec<CaptureStats>,
    /// Monitor lists seen after a topology change (`t` on the QPC µs clock).
    #[serde(default)]
    pub topology_changes: Vec<Value>,
    #[serde(default)]
    pub stop_reason: Option<String>,
}

pub struct RecordArgs {
    pub out: PathBuf,
    pub mic: bool,
    pub mic_device: Option<String>,
    pub monitors: String,
    pub fps: u32,
    /// Planor trace to copy instead of `%LOCALAPPDATA%\planor\trace.log`.
    pub trace: Option<PathBuf>,
    /// A marker written at the start (a test name).
    pub mark: Option<String>,
    /// Pipe name suffix so several recorders can coexist.
    pub pipe: Option<String>,
    /// Do not register the global hotkeys (a test recorder next to a manual one).
    pub no_hotkeys: bool,
    pub max_size_mb: Option<u64>,
    pub max_minutes: Option<f64>,
    /// x264 preset (`ultrafast` by default; `veryfast` makes smaller files but drops frames under motion).
    pub preset: String,
    /// Threads per FFmpeg encoder; 0 lets x264 pick (about one per core, and a lot of memory).
    pub encoder_threads: u32,
}

static CONTROL_C: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> i32 {
    CONTROL_C.store(true, Ordering::SeqCst);
    1
}

pub fn default_trace_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|p| p.join("planor/trace.log"))
}

fn copy_trace(dir: &Path, name: &str, source: &Option<PathBuf>) {
    if let Some(source) = source.clone().or_else(default_trace_path) {
        if source.is_file() {
            let target = dir.join(name);
            match std::fs::copy(&source, &target) {
                Ok(bytes) => println!("planor-rec: copied Planor trace ({bytes} bytes) to {}", target.display()),
                Err(e) => eprintln!("planor-rec: could not copy {}: {e}", source.display()),
            }
        } else if !dir.join(name).is_file() {
            println!(
                "planor-rec: no Planor trace at {} (diagnostics may be off)",
                source.display()
            );
        }
    }
}

/// Keep `dst` a copy of `src` while recording. An isolated test daemon deletes its folder (and its trace)
/// when it exits, which can be before the recording is stopped, so the recorder keeps its own copy as the
/// trace grows and never removes it. The copy is only refreshed when the size or modified time changed.
pub fn spawn_trace_follower(src: PathBuf, dst: PathBuf, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut last: Option<(u64, Option<std::time::SystemTime>)> = None;
        while !stop.load(Ordering::Relaxed) {
            follow_trace_once(&src, &dst, &mut last);
            for _ in 0..6 {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
        follow_trace_once(&src, &dst, &mut last);
    })
}

/// One refresh step of `spawn_trace_follower`.
pub fn follow_trace_once(
    src: &Path,
    dst: &Path,
    last: &mut Option<(u64, Option<std::time::SystemTime>)>,
) {
    if let Ok(m) = std::fs::metadata(src) {
        let key = (m.len(), m.modified().ok());
        if *last != Some(key) && std::fs::copy(src, dst).is_ok() {
            *last = Some(key);
        }
    }
}
pub fn select_monitors(monitors: &[MonitorInfo], spec: &str) -> util::anyhow_lite::Result<Vec<u32>> {
    if monitors.is_empty() {
        return util::anyhow_lite::err("no monitors found");
    }
    if spec.eq_ignore_ascii_case("all") || spec.is_empty() {
        return Ok(monitors.iter().map(|m| m.index).collect());
    }
    let index: u32 = spec
        .parse()
        .map_err(|_| util::fmt_err(format!("--monitors must be `all` or a monitor index, got `{spec}`")))?;
    if monitors.iter().any(|m| m.index == index) {
        Ok(vec![index])
    } else {
        util::anyhow_lite::err(format!(
            "monitor {index} not found (available: {:?})",
            monitors.iter().map(|m| m.index).collect::<Vec<_>>()
        ))
    }
}

// ---- Hotkeys -----------------------------------------------------------------------------------

const ERROR_HOTKEY_ALREADY_REGISTERED: u32 = 1409;

/// The text shown (and stored in `meta.json`) when a hotkey could not be registered.
pub fn describe_hotkey_failure(keys: &str, code: u32) -> String {
    if code == ERROR_HOTKEY_ALREADY_REGISTERED {
        format!(
            "{keys} is already registered by another program (Windows error {code}); \
             it will NOT work in this recording. Use `planor-rec stop` / `planor-rec mark <text>` instead, \
             or close the program that owns it"
        )
    } else {
        format!("{keys} could not be registered (Windows error {code}); it will NOT work in this recording")
    }
}

/// Register Ctrl+Alt+Shift+R (stop) and Ctrl+Alt+Shift+M (mark) on a thread with a message loop.
/// The outcome of each registration is reported back, never swallowed.
fn spawn_hotkeys(
    stop: Arc<AtomicBool>,
    writer: Arc<EventWriter>,
) -> (Vec<HotkeyStatus>, Option<(thread::JoinHandle<()>, u32)>) {
    let (tx, rx) = mpsc::channel::<(Vec<HotkeyStatus>, u32)>();
    let handle = thread::spawn(move || {
        let mods = MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT;
        let mut statuses = Vec::new();
        for (id, name, keys, vk) in [
            (1, "stop", "Ctrl+Alt+Shift+R", 'R' as u32),
            (2, "mark", "Ctrl+Alt+Shift+M", VK_M as u32),
        ] {
            let ok = unsafe { RegisterHotKey(std::ptr::null_mut(), id, mods, vk) } != 0;
            let code = if ok { 0 } else { unsafe { GetLastError() } };
            statuses.push(HotkeyStatus {
                name: name.into(),
                keys: keys.into(),
                ok,
                error_code: if ok { None } else { Some(code) },
                detail: if ok {
                    "registered".into()
                } else {
                    describe_hotkey_failure(keys, code)
                },
            });
        }
        let thread_id = unsafe { GetCurrentThreadId() };
        let _ = tx.send((statuses, thread_id));
        let mut msg = MSG::default();
        while !stop.load(Ordering::Relaxed) {
            let ret = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
            if ret <= 0 {
                break;
            }
            if msg.message == WM_HOTKEY && msg.wParam == 1 {
                println!("planor-rec: Ctrl+Alt+Shift+R pressed; stopping");
                writer.write(json!({"kind": "session", "event": "hotkey", "name": "stop"}));
                stop.store(true, Ordering::SeqCst);
            } else if msg.message == WM_HOTKEY && msg.wParam == 2 {
                writer.write(json!({"kind": "mark", "text": "hotkey Ctrl+Alt+Shift+M", "source": "hotkey"}));
                println!("planor-rec: mark added by hotkey");
            }
        }
        unsafe {
            UnregisterHotKey(std::ptr::null_mut(), 1);
            UnregisterHotKey(std::ptr::null_mut(), 2);
        }
    });
    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok((statuses, thread_id)) => (statuses, Some((handle, thread_id))),
        Err(_) => (
            vec![HotkeyStatus {
                name: "all".into(),
                keys: "Ctrl+Alt+Shift+R/M".into(),
                ok: false,
                error_code: None,
                detail: "the hotkey thread did not report within 2 s".into(),
            }],
            None,
        ),
    }
}

// ---- Microphone --------------------------------------------------------------------------------

fn list_audio_devices(ffmpeg: &Path) -> Vec<String> {
    let output = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-list_devices",
            "true",
            "-f",
            "dshow",
            "-i",
            "dummy",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stderr);
    let mut devices = Vec::new();
    for line in text.lines() {
        if !line.contains("(audio)") {
            continue;
        }
        if let Some(start) = line.find('"') {
            if let Some(end) = line[start + 1..].find('"') {
                let name = &line[start + 1..start + 1 + end];
                if !name.is_empty() && !devices.iter().any(|d| d == name) {
                    devices.push(name.to_string());
                }
            }
        }
    }
    devices
}

fn choose_mic_device(ffmpeg: &Path, requested: &Option<String>) -> Option<String> {
    if let Some(name) = requested {
        return Some(name.clone());
    }
    let devices = list_audio_devices(ffmpeg);
    if devices.is_empty() {
        return None;
    }
    if devices.iter().any(|d| d == MIC_DEVICE) {
        return Some(MIC_DEVICE.to_string());
    }
    eprintln!(
        "planor-rec: microphone \"{MIC_DEVICE}\" not found; using the first dshow audio device: \"{}\"",
        devices[0]
    );
    Some(devices[0].clone())
}

struct AudioProc {
    child: Child,
    device: String,
    spawn_qpc_us: i64,
}

fn spawn_audio(out: &Path, requested_device: &Option<String>) -> Option<AudioProc> {
    let ffmpeg = match util::find_ffmpeg() {
        Some(p) => p,
        None => {
            eprintln!("planor-rec: microphone requested but FFmpeg was not found; recording video only");
            return None;
        }
    };
    let Some(device) = choose_mic_device(&ffmpeg, requested_device) else {
        eprintln!(
            "planor-rec: microphone requested but no dshow audio device was found; recording video only"
        );
        return None;
    };
    let path = out.join("audio.wav");
    let log = File::create(out.join("ffmpeg-audio.log")).ok();
    let mut command = Command::new(ffmpeg);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "dshow",
        "-i",
        &format!("audio={device}"),
        "-ac",
        "1",
        "-ar",
        "16000",
        "-c:a",
        "pcm_s16le",
        // No LIST/INFO chunk: the data chunk starts at byte 44.
        "-fflags",
        "+bitexact",
    ]);
    // stdin is piped so the recorder can send `q` and let FFmpeg finish the WAV header itself.
    command.arg(&path).stdin(Stdio::piped()).stdout(Stdio::null());
    if let Some(log) = log {
        command.stderr(Stdio::from(log));
    } else {
        command.stderr(Stdio::null());
    }
    let spawn_qpc_us = qpc_us();
    match command.spawn() {
        Ok(child) => {
            println!(
                "microphone recording: \"{device}\" -> {} (16 kHz mono, local only)",
                path.display()
            );
            Some(AudioProc {
                child,
                device,
                spawn_qpc_us,
            })
        }
        Err(e) => {
            eprintln!("planor-rec: microphone failed to start: {e}; recording video only");
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavInfo {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits: u16,
    pub data_offset: u64,
    /// Bytes of audio data actually present in the file.
    pub data_bytes: u64,
}

impl WavInfo {
    pub fn duration_ms(&self) -> i64 {
        let bytes_per_second =
            self.sample_rate as u64 * self.channels as u64 * (self.bits as u64 / 8).max(1);
        if bytes_per_second == 0 {
            0
        } else {
            (self.data_bytes * 1000 / bytes_per_second) as i64
        }
    }
}

/// Parse a RIFF/WAVE file by walking its chunks (FFmpeg may or may not write a LIST chunk before
/// `data`). The data size is taken from the file length when the header still holds a streaming
/// placeholder.
pub fn wav_info(path: &Path) -> Option<WavInfo> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut head = [0u8; 12];
    file.read_exact(&mut head).ok()?;
    if &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12u64;
    let (mut channels, mut rate, mut bits) = (0u16, 0u32, 0u16);
    loop {
        file.seek(SeekFrom::Start(pos)).ok()?;
        let mut chunk = [0u8; 8];
        file.read_exact(&mut chunk).ok()?;
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        match &chunk[0..4] {
            b"fmt " => {
                let mut fmt = [0u8; 16];
                file.read_exact(&mut fmt).ok()?;
                channels = u16::from_le_bytes([fmt[2], fmt[3]]);
                rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                bits = u16::from_le_bytes([fmt[14], fmt[15]]);
            }
            b"data" => {
                let offset = pos + 8;
                let available = len.saturating_sub(offset);
                let data_bytes = if size == 0 || size == 0xFFFF_FFFF || size > available {
                    available
                } else {
                    size
                };
                return Some(WavInfo {
                    channels,
                    sample_rate: rate,
                    bits,
                    data_offset: offset,
                    data_bytes,
                });
            }
            _ => {}
        }
        pos += 8 + size + (size & 1);
        if pos >= len {
            return None;
        }
    }
}

/// Patch the RIFF and data lengths that FFmpeg leaves at their streaming placeholders when it is
/// killed. The data chunk is located by walking the chunks, not assumed at byte 44.
pub fn finalize_wav(path: &Path) {
    let Some(info) = wav_info(path) else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new().read(true).write(true).open(path) else {
        return;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return;
    };
    let data = info.data_bytes.min(u32::MAX as u64) as u32;
    let riff = (len.saturating_sub(8)).min(u32::MAX as u64) as u32;
    let _ = file.seek(SeekFrom::Start(4));
    let _ = file.write_all(&riff.to_le_bytes());
    let _ = file.seek(SeekFrom::Start(info.data_offset - 4));
    let _ = file.write_all(&data.to_le_bytes());
}

fn stop_audio(mut proc: AudioProc, out: &Path, qpc_start_us: i64) -> Option<AudioMeta> {
    let end_qpc_us = qpc_us();
    if let Some(mut stdin) = proc.child.stdin.take() {
        let _ = stdin.write_all(b"q");
        let _ = stdin.flush();
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match proc.child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = proc.child.kill();
                let _ = proc.child.wait();
                break;
            }
        }
    }
    let path = out.join("audio.wav");
    finalize_wav(&path);
    let info = wav_info(&path)?;
    let duration_ms = info.duration_ms();
    let start_qpc_us = end_qpc_us - duration_ms * 1000;
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!(
        "planor-rec: microphone stopped ({}): {} ms, {} Hz, {} ch; audio starts {:+} ms from the video start",
        path.display(),
        duration_ms,
        info.sample_rate,
        info.channels,
        (start_qpc_us - qpc_start_us) / 1000
    );
    Some(AudioMeta {
        file: "audio.wav".into(),
        device: proc.device,
        sample_rate: info.sample_rate,
        channels: info.channels,
        spawn_qpc_us: proc.spawn_qpc_us,
        end_qpc_us,
        duration_ms,
        start_qpc_us,
        offset_to_video_start_ms: (start_qpc_us - qpc_start_us) / 1000,
        bytes,
    })
}

// ---- One monitor -------------------------------------------------------------------------------

fn record_monitor(
    monitor: MonitorInfo,
    out: PathBuf,
    fps: u32,
    preset: String,
    encoder_threads: u32,
    stop: Arc<AtomicBool>,
    writer: Arc<EventWriter>,
    epoch: Arc<AtomicU64>,
) -> util::anyhow_lite::Result<CaptureStats> {
    let index = monitor.index;
    let size = (monitor.rect.w as u32, monitor.rect.h as u32);
    let ffmpeg_path = util::find_ffmpeg().ok_or_else(|| util::fmt_err("FFmpeg not found"))?;
    let mut child = Command::new(ffmpeg_path)
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
            "16",
            "-pix_fmt",
            "yuv420p",
            "-r",
            &fps.to_string(),
            "-movflags",
            "+faststart",
        ])
        .arg(out.join(format!("video-{index}.mp4")))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(out.join(format!("ffmpeg-{index}.log")))
                .map_err(|e| util::fmt_err(format!("ffmpeg log: {e}")))?,
        ))
        .spawn()
        .map_err(|e| util::fmt_err(format!("starting FFmpeg: {e}")))?;
    let stdin = child.stdin.take().ok_or_else(|| util::fmt_err("ffmpeg stdin"))?;

    let capture = match Duplicator::open_by_device(Some(&monitor.device), index) {
        Ok(c) => c,
        Err(e) => {
            drop(stdin);
            let _ = child.wait();
            return util::anyhow_lite::err(format!("opening monitor {index} for capture: {e}"));
        }
    };
    if (capture.width, capture.height) != size {
        drop(stdin);
        let _ = child.wait();
        return util::anyhow_lite::err(format!(
            "monitor {index}: capture is {}x{} but meta says {}x{}; refusing to write a mismatched video",
            capture.width, capture.height, size.0, size.1
        ));
    }
    let csv_file = File::create(out.join(format!("frames-{index}.csv")))
        .map_err(|e| util::fmt_err(format!("frames csv: {e}")))?;
    let mut sink = QueueSink::new(
        Box::new(stdin),
        Box::new(BufWriter::new(csv_file)),
        ENCODER_QUEUE_FRAMES,
    );
    let mut cfg = TimelineCfg::new(index, fps, size);
    cfg.topology_epoch = Some(epoch);
    let mut opener = DxgiOpener {
        device: monitor.device.clone(),
        index,
    };
    // The tick loop must not be starved by whatever else is busy (Planor building its overlay, the encoders):
    // a 100 ms stall is a gap in the recorded timeline.
    util::raise_current_thread_priority();
    let result = timeline::run_timeline(
        &cfg,
        Box::new(capture),
        &mut opener,
        &mut sink as &mut dyn FrameSink,
        &*writer,
        &RealClock,
        &stop,
    );
    let writer_stats = sink.finish();
    let status = child.wait();
    if let Some(e) = &writer_stats.error {
        eprintln!("planor-rec: monitor {index}: {e}");
        stop.store(true, Ordering::SeqCst);
    }
    if writer_stats.error.is_none() {
        if let Ok(status) = status {
            if !status.success() {
                eprintln!("planor-rec: monitor {index}: FFmpeg exited with {status}");
            }
        }
    }
    let stats = result.map_err(util::fmt_err)?;
    Ok(CaptureStats {
        monitor: index,
        ticks: stats.ticks,
        written: writer_stats.written,
        dropped: stats.dropped,
        lost_ticks: stats.lost_ticks,
        lost_periods: stats.lost_periods,
        reopens: stats.reopens,
        max_lost_ms: stats.max_lost_ms,
    })
}

// ---- Monitor topology --------------------------------------------------------------------------

/// What identifies the monitor layout: device names and rectangles.
pub fn topology_key(monitors: &[MonitorInfo]) -> Vec<(String, [i32; 4])> {
    let mut key: Vec<(String, [i32; 4])> = monitors
        .iter()
        .map(|m| (m.device.clone(), m.rect.to_array()))
        .collect();
    key.sort();
    key
}

fn spawn_topology_watcher(
    initial: Vec<MonitorInfo>,
    stop: Arc<AtomicBool>,
    writer: Arc<EventWriter>,
    epoch: Arc<AtomicU64>,
    log: Arc<Mutex<Vec<Value>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        capture::dpi_aware();
        let mut last = topology_key(&initial);
        let mut next = Instant::now() + Duration::from_secs(2);
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
            if Instant::now() < next {
                continue;
            }
            next = Instant::now() + Duration::from_secs(2);
            // A failure here usually means the monitor list is changing right now; try again next time.
            let Ok(now_monitors) = capture::monitors() else {
                continue;
            };
            let key = topology_key(&now_monitors);
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
                println!("planor-rec: monitor layout changed; reopening captures");
                if let Ok(mut l) = log.lock() {
                    l.push(event.clone());
                }
                writer.write(event);
                epoch.fetch_add(1, Ordering::SeqCst);
            }
        }
    })
}

// ---- Limits ------------------------------------------------------------------------------------

/// Bytes of the recording files that grow with time (videos and audio).
///
/// The size comes from `std::fs::metadata(path)` (a handle query), not from `DirEntry::metadata`: NTFS
/// directory entries keep a stale size (often 0) for files another process still has open, which is
/// exactly the state of a video FFmpeg is writing. Using the stale value made `--max-size-mb` blind.
pub fn recording_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            (name.starts_with("video-") && name.ends_with(".mp4")) || name == "audio.wav"
        })
        .filter_map(|e| std::fs::metadata(e.path()).ok())
        .map(|m| m.len())
        .sum()
}

/// Why a recording should stop on its own, if it should.
pub fn limit_reached(
    elapsed: Duration,
    bytes: u64,
    max_size_mb: Option<u64>,
    max_minutes: Option<f64>,
) -> Option<String> {
    if let Some(minutes) = max_minutes {
        if elapsed.as_secs_f64() >= minutes * 60.0 {
            return Some(format!("--max-minutes {minutes} reached"));
        }
    }
    if let Some(mb) = max_size_mb {
        if bytes >= mb.saturating_mul(1024 * 1024) {
            return Some(format!(
                "--max-size-mb {mb} reached ({:.1} MB written)",
                bytes as f64 / 1_048_576.0
            ));
        }
    }
    None
}

// ---- Run ---------------------------------------------------------------------------------------

#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(uPeriod: u32) -> u32;
    fn timeEndPeriod(uPeriod: u32) -> u32;
}

struct HighResTimer(bool);

impl HighResTimer {
    fn acquire(enabled: bool) -> Self {
        Self(enabled && unsafe { timeBeginPeriod(1) == 0 })
    }
}

impl Drop for HighResTimer {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

pub fn run(args: RecordArgs) -> util::anyhow_lite::Result<()> {
    let out = args.out;
    util::ensure_dir(&out)?;
    let out = std::path::absolute(&out).unwrap_or(out);
    let fps = args.fps.clamp(1, 240);
    let _hi_res = HighResTimer::acquire(fps > 60);
    util::run_at_full_speed();
    capture::dpi_aware();
    let qpc_freq = qpc_frequency();
    let qpc_start = util::qpc_ticks();
    let qpc_start_us = util::ticks_to_us(qpc_start, qpc_freq);
    let utc_start_unix_ms = unix_ms();
    let monitors = capture::monitors().map_err(|e| util::fmt_err(format!("enumerating monitors: {e}")))?;
    let taskbars = capture::taskbar_rects();
    let selected = select_monitors(&monitors, &args.monitors)?;

    let meta = Meta {
        tool: "planor-rec".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        qpc_frequency: qpc_freq,
        qpc_start,
        qpc_start_us,
        utc_start: util::iso_utc(utc_start_unix_ms),
        utc_start_unix_ms,
        fps,
        monitors: monitors.clone(),
        taskbars: taskbars.clone(),
        mic: args.mic,
        ..Default::default()
    };
    util::write_json(&out.join("meta.json"), &meta)?;

    let writer = EventWriter::open(&out)?;
    writer.write(json!({
        "kind": "session",
        "event": "start",
        "version": env!("CARGO_PKG_VERSION"),
        "monitors": selected,
        "fps": fps,
        "mic": args.mic,
        "qpc_frequency": qpc_freq,
        "qpc_start": qpc_start,
        "max_size_mb": args.max_size_mb,
        "max_minutes": args.max_minutes,
    }));
    if let Some(text) = &args.mark {
        writer.write(json!({"kind": "mark", "text": text, "source": "cli"}));
    }

    CONTROL_C.store(false, Ordering::SeqCst);
    let stop = Arc::new(AtomicBool::new(false));
    unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), 1) };
    let (hotkeys, hotkey_thread) = if args.no_hotkeys {
        (
            vec![HotkeyStatus {
                name: "all".into(),
                keys: "Ctrl+Alt+Shift+R/M".into(),
                ok: false,
                error_code: None,
                detail: "not registered (--no-hotkeys)".into(),
            }],
            None,
        )
    } else {
        spawn_hotkeys(stop.clone(), writer.clone())
    };
    for h in &hotkeys {
        if h.ok {
            println!("planor-rec: hotkey {} ({}) registered", h.keys, h.name);
        } else {
            eprintln!("planor-rec: WARNING: hotkey {}", h.detail);
        }
    }
    writer.write(json!({"kind": "session", "event": "hotkeys", "hotkeys": hotkeys}));
    let pipe_thread = pipe::spawn_server(writer.clone(), stop.clone(), args.pipe.clone());
    let input = events::spawn_input(writer.clone(), stop.clone());
    let os = events::spawn_os(writer.clone(), stop.clone());
    let epoch = Arc::new(AtomicU64::new(0));
    let topology_log = Arc::new(Mutex::new(Vec::<Value>::new()));
    let topology = spawn_topology_watcher(
        monitors.clone(),
        stop.clone(),
        writer.clone(),
        epoch.clone(),
        topology_log.clone(),
    );

    copy_trace(&out, "trace-start.log", &args.trace);
    // An explicit --trace is an isolated test daemon's trace: follow it, it may vanish before we stop.
    let trace_follower = args
        .trace
        .clone()
        .map(|src| spawn_trace_follower(src, out.join("trace.log"), stop.clone()));

    let mut audio_proc = if args.mic { spawn_audio(&out, &args.mic_device) } else { None };

    let mut threads = Vec::new();
    for &index in &selected {
        let monitor = monitors.iter().find(|m| m.index == index).unwrap().clone();
        println!(
            "planor-rec: recording monitor {index} ({}, {}x{} at {},{} work {}x{}) -> {}",
            monitor.device,
            monitor.rect.w,
            monitor.rect.h,
            monitor.rect.x,
            monitor.rect.y,
            monitor.work.w,
            monitor.work.h,
            out.join(format!("video-{index}.mp4")).display()
        );
        let stop = stop.clone();
        let out = out.clone();
        let writer = writer.clone();
        let epoch = epoch.clone();
        let preset = args.preset.clone();
        let encoder_threads = args.encoder_threads;
        let handle = thread::spawn(move || {
            record_monitor(monitor, out, fps, preset, encoder_threads, stop, writer, epoch)
        });
        threads.push((index, handle));
    }
    println!(
        "planor-rec: press Ctrl+C or Ctrl+Alt+Shift+R to stop, Ctrl+Alt+Shift+M or `planor-rec mark <text>` for a marker"
    );
    if let Some(mb) = args.max_size_mb {
        println!("planor-rec: will stop by itself at {mb} MB");
    }
    if let Some(min) = args.max_minutes {
        println!("planor-rec: will stop by itself after {min} minutes");
    }

    let started = Instant::now();
    let mut stop_reason: Option<String> = None;
    let mut next_limit_check = Instant::now();
    while !stop.load(Ordering::Relaxed) && !CONTROL_C.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(50));
        if Instant::now() >= next_limit_check {
            next_limit_check = Instant::now() + Duration::from_secs(1);
            if args.max_size_mb.is_some() || args.max_minutes.is_some() {
                let bytes = recording_bytes(&out);
                if let Some(reason) =
                    limit_reached(started.elapsed(), bytes, args.max_size_mb, args.max_minutes)
                {
                    println!("planor-rec: {reason}; stopping cleanly");
                    stop_reason = Some(reason);
                    break;
                }
            }
        }
    }
    stop.store(true, Ordering::SeqCst);
    if CONTROL_C.load(Ordering::SeqCst) {
        println!("planor-rec: Ctrl+C; finishing files");
        stop_reason.get_or_insert_with(|| "Ctrl+C".into());
    }
    stop_reason.get_or_insert_with(|| "stop requested (pipe or hotkey)".into());

    let mut frame_counts = Vec::new();
    let mut capture_stats = Vec::new();
    for (index, handle) in threads {
        match handle.join() {
            Ok(Ok(stats)) => {
                println!(
                    "planor-rec: monitor {index}: {} frames encoded ({} ticks, {} dropped, {} lost, {} reopen(s))",
                    stats.written, stats.ticks, stats.dropped, stats.lost_ticks, stats.reopens
                );
                frame_counts.push(stats.written);
                capture_stats.push(stats);
            }
            Ok(Err(e)) => {
                eprintln!("planor-rec: monitor {index}: {e}");
                frame_counts.push(0);
            }
            Err(_) => {
                eprintln!("planor-rec: monitor {index}: recorder thread panicked");
                frame_counts.push(0);
            }
        }
    }

    let mut audio_meta = None;
    if let Some(proc) = audio_proc.take() {
        audio_meta = stop_audio(proc, &out, qpc_start_us);
    }

    let _ = input.join();
    let _ = os.join();
    let _ = topology.join();
    if let Some(h) = trace_follower {
        let _ = h.join();
    }
    if let Some((handle, thread_id)) = hotkey_thread {
        // Wake the message loop so the hotkeys are released.
        unsafe { PostThreadMessageW(thread_id, WM_QUIT, 0, 0) };
        let _ = handle.join();
    }
    // Give the pipe server a moment to notice the stop flag and release its pipe instance.
    thread::sleep(Duration::from_millis(60));
    drop(pipe_thread);
    writer.write(json!({
        "kind": "session", "event": "stop", "frames": frame_counts,
        "reason": stop_reason,
    }));
    writer.close();
    copy_trace(&out, "trace.log", &args.trace);

    let mut done = meta;
    done.mic_device = audio_meta.as_ref().map(|a| a.device.clone());
    done.audio = audio_meta;
    done.qpc_end_us = Some(qpc_us());
    done.utc_end = Some(util::iso_utc(unix_ms()));
    done.frames = frame_counts;
    done.hotkeys = hotkeys;
    done.capture = capture_stats;
    done.topology_changes = topology_log.lock().map(|l| l.clone()).unwrap_or_default();
    done.stop_reason = stop_reason;
    util::write_json(&out.join("meta.json"), &done)?;
    println!(
        "planor-rec: recording complete at {} ({:.1} MB of video/audio)",
        out.display(),
        recording_bytes(&out) as f64 / 1_048_576.0
    );
    Ok(())
}

pub fn meta_from_dir(dir: &Path) -> util::anyhow_lite::Result<Meta> {
    let path = dir.join("meta.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| util::fmt_err(format!("reading {}: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| util::fmt_err(format!("parsing {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("planor-rec-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn wav_bytes(list_chunk: bool, samples: usize, placeholder: bool) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(if placeholder { 0xFFFF_FFFFu32 } else { 0 }).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&16_000u32.to_le_bytes());
        v.extend_from_slice(&32_000u32.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        if list_chunk {
            v.extend_from_slice(b"LIST");
            v.extend_from_slice(&26u32.to_le_bytes());
            v.extend_from_slice(b"INFOISFT\x0e\x00\x00\x00Lavf62.3.100\x00\x00");
        }
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(if placeholder { 0xFFFF_FFFFu32 } else { (samples * 2) as u32 }).to_le_bytes());
        v.extend(std::iter::repeat_n(0u8, samples * 2));
        v
    }

    #[test]
    fn wav_info_finds_the_data_chunk_with_and_without_a_list_chunk() {
        let dir = tmp("wav");
        for (list, placeholder) in [(false, false), (true, false), (true, true)] {
            let path = dir.join("a.wav");
            std::fs::write(&path, wav_bytes(list, 32_000, placeholder)).unwrap();
            let info = wav_info(&path).unwrap();
            assert_eq!(info.sample_rate, 16_000);
            assert_eq!(info.channels, 1);
            assert_eq!(info.bits, 16);
            assert_eq!(info.data_bytes, 64_000, "list={list} placeholder={placeholder}");
            assert_eq!(info.duration_ms(), 2_000);
            assert_eq!(info.data_offset, if list { 12 + 24 + 8 + 26 + 8 } else { 44 });
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finalize_patches_the_real_data_chunk_not_byte_40() {
        let dir = tmp("wavfix");
        let path = dir.join("a.wav");
        std::fs::write(&path, wav_bytes(true, 16_000, true)).unwrap();
        finalize_wav(&path);
        let bytes = std::fs::read(&path).unwrap();
        let info = wav_info(&path).unwrap();
        let size_at = (info.data_offset - 4) as usize;
        assert_eq!(
            u32::from_le_bytes(bytes[size_at..size_at + 4].try_into().unwrap()),
            32_000
        );
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
            bytes.len() - 8
        );
        // The LIST chunk is untouched.
        assert_eq!(&bytes[36..40], b"LIST");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hotkey_conflicts_are_described_clearly() {
        let text = describe_hotkey_failure("Ctrl+Alt+Shift+R", 1409);
        assert!(text.contains("already registered by another program"), "{text}");
        assert!(text.contains("1409"));
        assert!(text.contains("planor-rec stop"));
        let other = describe_hotkey_failure("Ctrl+Alt+Shift+M", 5);
        assert!(other.contains("could not be registered"));
    }

    #[test]
    fn limits_stop_on_time_or_size() {
        let mb = 1024 * 1024;
        assert!(limit_reached(Duration::from_secs(10), 5 * mb, Some(100), Some(1.0)).is_none());
        assert!(limit_reached(Duration::from_secs(61), 0, None, Some(1.0))
            .unwrap()
            .contains("--max-minutes"));
        assert!(limit_reached(Duration::from_secs(1), 100 * mb, Some(100), None)
            .unwrap()
            .contains("--max-size-mb"));
        assert!(limit_reached(Duration::from_secs(1000), u64::MAX, None, None).is_none());
    }

    #[test]
    fn recording_bytes_counts_videos_and_audio_only() {
        let dir = tmp("bytes");
        std::fs::write(dir.join("video-0.mp4"), vec![0u8; 1000]).unwrap();
        std::fs::write(dir.join("video-1.mp4"), vec![0u8; 500]).unwrap();
        std::fs::write(dir.join("audio.wav"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("events.jsonl"), vec![0u8; 9999]).unwrap();
        assert_eq!(recording_bytes(&dir), 1600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_bytes_sees_a_file_that_is_still_open_for_writing() {
        let dir = tmp("openbytes");
        let mut f = File::create(dir.join("video-0.mp4")).unwrap();
        f.write_all(&vec![0u8; 123_456]).unwrap();
        f.flush().unwrap();
        // The writer keeps the handle open, as FFmpeg does.
        assert_eq!(recording_bytes(&dir), 123_456);
        drop(f);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_trace_follower_keeps_a_copy_after_the_source_is_deleted() {
        let dir = tmp("follow");
        let src = dir.join("src.log");
        let dst = dir.join("copy.log");
        let mut last = None;
        // Nothing to copy yet.
        follow_trace_once(&src, &dst, &mut last);
        assert!(!dst.exists());
        std::fs::write(&src, "one\n").unwrap();
        follow_trace_once(&src, &dst, &mut last);
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "one\n");
        std::fs::write(&src, "one\ntwo\n").unwrap();
        follow_trace_once(&src, &dst, &mut last);
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "one\ntwo\n");
        // The daemon exits and deletes its folder: the copy stays.
        std::fs::remove_file(&src).unwrap();
        follow_trace_once(&src, &dst, &mut last);
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "one\ntwo\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn topology_key_ignores_order_and_catches_a_size_change() {
        let m = |i: u32, w: i32| MonitorInfo {
            index: i,
            device: format!("\\\\.\\DISPLAY{}", i + 1),
            rect: Rect::new(0, 0, w, 1080),
            work: Rect::new(0, 0, w, 1040),
            dpi: 96,
            primary: i == 0,
        };
        let a = vec![m(0, 1920), m(1, 2560)];
        let b = vec![m(1, 2560), m(0, 1920)];
        assert_eq!(topology_key(&a), topology_key(&b));
        assert_ne!(topology_key(&a), topology_key(&[m(0, 1920), m(1, 1920)]));
        assert_ne!(topology_key(&a), topology_key(&[m(0, 1920)]));
    }
}

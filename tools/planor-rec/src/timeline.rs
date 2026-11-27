//! The constant-rate capture timeline of one monitor, written against small traits so the dropout
//! handling (lost duplication, back-off reopen, topology change, encoder falling behind) is
//! unit-testable without a GPU, a monitor or FFmpeg.
//!
//! One tick is `1/fps` of wall clock. Every tick produces exactly one timeline row, whatever the
//! capture API does: a healthy tick (`ok`), a tick while the duplicator is lost or reopening
//! (`lost`, the previous picture is repeated) and the first tick with a fresh picture after a
//! reopen (`reopened`). A row is only missing when the encoder queue was full and the frame was
//! dropped on purpose; that is logged as an `encoder_behind` capture event.

use crate::util::Rect;
use serde_json::{Value, json};
use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::JoinHandle,
};

/// How a timeline row came about; written to the `state` column of `frames-N.csv`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameState {
    Ok,
    Lost,
    Reopened,
}

impl FrameState {
    pub fn as_str(self) -> &'static str {
        match self {
            FrameState::Ok => "ok",
            FrameState::Lost => "lost",
            FrameState::Reopened => "reopened",
        }
    }
    pub fn parse(s: &str) -> FrameState {
        match s.trim() {
            "lost" => FrameState::Lost,
            "reopened" => FrameState::Reopened,
            _ => FrameState::Ok,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub tick: u64,
    pub present_us: i64,
    pub changed: bool,
    pub state: FrameState,
    /// Recorder time spent on this frame after `AcquireNextFrame` returned and before `present_us` was taken
    /// (GPU copy, CPU copy and rotation of a rotated monitor). `present_us` is stamped after that work, so it
    /// is late by this much. Zero for an unchanged frame and for an unrotated monitor.
    pub rotate_stall_us: i64,
}

impl Row {
    pub fn csv_line(&self) -> String {
        format!(
            "{},{},{},{},{}\n",
            self.tick,
            self.present_us,
            if self.changed { 1 } else { 0 },
            self.state.as_str(),
            self.rotate_stall_us
        )
    }
}

pub const CSV_HEADER: &str = "index,present_qpc_us,changed,state,rotate_stall_us\n";

/// One open desktop duplication (or a fake in tests).
pub trait FrameSource {
    /// Width and height of the frames this source delivers.
    fn size(&self) -> (u32, u32);
    /// Desktop rectangle of the monitor at the time the source was opened.
    fn rect(&self) -> Rect;
    /// `Ok(true)`: `out` holds a new frame. `Ok(false)`: nothing changed within the timeout.
    /// `Err`: the duplication is lost (access lost, mode change, device removed, ...).
    fn acquire_into(&mut self, out: &mut Vec<u8>, timeout_ms: u32) -> Result<bool, String>;
    /// Microseconds the last `acquire_into` spent in `[AcquireNextFrame, GPU copy + Map, CPU copy/rotate]`
    /// (evidence for slow-call events); `None` for sources that do not measure it.
    fn phases_us(&self) -> Option<[i64; 4]> {
        None
    }
    /// Recorder time after the last `acquire_into` returned a frame, for a rotated monitor only (see `Row`);
    /// zero for sources that do not rotate.
    fn rotate_stall_us(&self) -> i64 {
        0
    }
}

/// Opens (or reopens) the duplication of one monitor.
pub trait Opener {
    fn open(&mut self) -> Result<Box<dyn FrameSource>, String>;
}

pub trait Clock {
    fn now_us(&self) -> i64;
    fn sleep_ms(&self, ms: u64);
}

pub struct RealClock;

impl Clock for RealClock {
    fn now_us(&self) -> i64 {
        crate::util::qpc_us()
    }
    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

pub trait EventSink {
    fn event(&self, value: Value);
}

impl EventSink for crate::events::EventWriter {
    fn event(&self, value: Value) {
        self.write(value);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Push {
    Written,
    /// The encoder queue was full: this tick was dropped (no video frame, no CSV row).
    Dropped,
    /// The encoder is gone; stop recording this monitor.
    Closed,
}

pub trait FrameSink {
    fn push(&mut self, bytes: &[u8], row: Row) -> Push;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimelineStats {
    pub ticks: u64,
    pub written: u64,
    pub dropped: u64,
    pub lost_ticks: u64,
    pub lost_periods: u64,
    pub reopens: u64,
    pub max_lost_ms: i64,
}

pub struct TimelineCfg {
    pub monitor: u32,
    pub fps: u32,
    /// The frame size the video was started with; a reopened source of another size cannot be encoded.
    pub size: (u32, u32),
    /// Shared counter bumped by the supervisor when the monitor list or a monitor's rectangle changes.
    pub topology_epoch: Option<Arc<AtomicU64>>,
    pub first_frame_timeout_us: i64,
}

impl TimelineCfg {
    pub fn new(monitor: u32, fps: u32, size: (u32, u32)) -> Self {
        Self {
            monitor,
            fps,
            size,
            topology_epoch: None,
            first_frame_timeout_us: 2_000_000,
        }
    }
}

/// A single capture call longer than this is logged as a `slow_acquire` event.
const SLOW_CALL_US: i64 = 40_000;
const BACKOFF_START_MS: i64 = 100;
const BACKOFF_MAX_MS: i64 = 2_000;
/// A reopened duplication that delivers nothing for this long counts as a failed reopen.
const REOPEN_FRAME_TIMEOUT_US: i64 = 3_000_000;

fn capture_event(monitor: u32, what: &str, extra: Value) -> Value {
    let mut v = json!({"kind": "capture", "monitor": monitor, "event": what});
    if let (Some(obj), Some(more)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in more {
            obj.insert(k.clone(), val.clone());
        }
    }
    v
}

/// Run the timeline until `stop`. `initial` is the already open first source.
pub fn run_timeline(
    cfg: &TimelineCfg,
    initial: Box<dyn FrameSource>,
    opener: &mut dyn Opener,
    sink: &mut dyn FrameSink,
    events: &dyn EventSink,
    clock: &dyn Clock,
    stop: &AtomicBool,
) -> Result<TimelineStats, String> {
    let frame_bytes = cfg.size.0 as usize * cfg.size.1 as usize * 4;
    let period_us = (1_000_000i64 / cfg.fps.max(1) as i64).max(1);
    let mut stats = TimelineStats::default();
    let mut previous = vec![0u8; frame_bytes];
    let mut source: Option<Box<dyn FrameSource>> = Some(initial);

    // Desktop duplication delivers an initial frame shortly after DuplicateOutput. Wait for it
    // before starting the constant timeline, otherwise a fast first tick would encode black.
    let until = clock.now_us() + cfg.first_frame_timeout_us;
    let mut have_frame = false;
    while !stop.load(Ordering::Relaxed) && !have_frame && clock.now_us() < until {
        match source.as_mut().unwrap().acquire_into(&mut previous, 100) {
            Ok(true) => have_frame = true,
            Ok(false) => {}
            Err(e) => {
                eprintln!("planor-rec: monitor {} capture error: {e}", cfg.monitor);
                clock.sleep_ms(20);
            }
        }
    }
    if !have_frame {
        return Err(format!(
            "monitor {} produced no frame within 2 s (desktop duplication failed)",
            cfg.monitor
        ));
    }

    let t0 = clock.now_us();
    let mut tick = 0u64;
    let mut epoch_seen = cfg
        .topology_epoch
        .as_ref()
        .map(|e| e.load(Ordering::Relaxed))
        .unwrap_or(0);

    // Loss bookkeeping.
    let mut have_picture = true; // `previous` holds a picture of the current source
    let mut lost_since: Option<i64> = None;
    let mut lost_reason = String::new();
    let mut attempts = 0u32;
    let mut backoff_ms = BACKOFF_START_MS;
    let mut next_open_us = 0i64;
    let mut opened_at_us = 0i64;
    let mut resized_reported: Option<(u32, u32)> = None;
    // Encoder-behind bookkeeping.
    let mut dropped_unreported = 0u64;
    let mut last_drop_report_us = t0;

    let lose = |reason: String,
                now: i64,
                lost_since: &mut Option<i64>,
                lost_reason: &mut String,
                attempts: &mut u32,
                backoff_ms: &mut i64,
                next_open_us: &mut i64,
                stats: &mut TimelineStats| {
        if lost_since.is_none() {
            *lost_since = Some(now);
            stats.lost_periods += 1;
            events.event(capture_event(
                cfg.monitor,
                "lost",
                json!({"t": now, "reason": reason}),
            ));
            eprintln!(
                "planor-rec: monitor {}: capture lost ({reason}); repeating the last frame and reopening",
                cfg.monitor
            );
        }
        *lost_reason = reason;
        *attempts = 0;
        *backoff_ms = BACKOFF_START_MS;
        *next_open_us = now + BACKOFF_START_MS * 1000;
    };

    while !stop.load(Ordering::Relaxed) {
        let deadline = t0 + tick as i64 * period_us;
        let mut changed = false;
        let mut stall_us = 0i64;
        let mut got_frame_after_open = false;
        loop {
            let now = clock.now_us();
            if now >= deadline {
                break;
            }
            // Topology change: drop the current source and reopen it right away.
            if let Some(epoch) = &cfg.topology_epoch {
                let e = epoch.load(Ordering::Relaxed);
                if e != epoch_seen {
                    epoch_seen = e;
                    if source.take().is_some() {
                        events.event(capture_event(
                            cfg.monitor,
                            "topology_reopen",
                            json!({"t": now, "reason": "monitor list or size changed"}),
                        ));
                    }
                    have_picture = false;
                    if lost_since.is_none() {
                        lost_since = Some(now);
                        stats.lost_periods += 1;
                        lost_reason = "topology change".into();
                    }
                    attempts = 0;
                    backoff_ms = BACKOFF_START_MS;
                    next_open_us = now;
                }
            }
            let remaining_ms = (((deadline - now) / 1000) as u64).clamp(1, 8);
            if let Some(src) = source.as_mut() {
                let call_start = now;
                let got = src.acquire_into(&mut previous, remaining_ms as u32);
                let call_us = clock.now_us() - call_start;
                if call_us >= SLOW_CALL_US {
                    // One capture call blocked the tick loop: the encoded timeline gets a gap here. Say where
                    // the time went so a pacing finding can be attributed (GPU wait vs CPU copy/rotate).
                    let ph = src.phases_us();
                    events.event(capture_event(
                        cfg.monitor,
                        "slow_acquire",
                        json!({
                            "t": call_start,
                            "ms": call_us / 1000,
                            "acquire_next_frame_ms": ph.map(|p| p[0] / 1000),
                            "gpu_copy_map_ms": ph.map(|p| p[1] / 1000),
                            "cpu_copy_rotate_ms": ph.map(|p| (p[2] + p[3]) / 1000),
                            "cpu_readback_copy_ms": ph.map(|p| p[2] / 1000),
                            "cpu_rotate_ms": ph.map(|p| p[3] / 1000),
                        }),
                    ));
                }
                match got {
                    Ok(true) => {
                        changed = true;
                        stall_us = src.rotate_stall_us();
                        if !have_picture {
                            have_picture = true;
                            got_frame_after_open = true;
                        }
                    }
                    Ok(false) => {
                        if !have_picture && now - opened_at_us > REOPEN_FRAME_TIMEOUT_US {
                            // Reopened but silent: treat as a failed reopen and back off.
                            source = None;
                            attempts += 1;
                            backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
                            next_open_us = now + backoff_ms * 1000;
                        }
                    }
                    Err(e) => {
                        source = None;
                        have_picture = false;
                        lose(
                            e,
                            now,
                            &mut lost_since,
                            &mut lost_reason,
                            &mut attempts,
                            &mut backoff_ms,
                            &mut next_open_us,
                            &mut stats,
                        );
                    }
                }
            } else if now >= next_open_us {
                attempts += 1;
                match opener.open() {
                    Ok(s) => {
                        if s.size() != cfg.size {
                            let new_size = s.size();
                            if resized_reported != Some(new_size) {
                                resized_reported = Some(new_size);
                                events.event(capture_event(
                                    cfg.monitor,
                                    "topology",
                                    json!({
                                        "t": now,
                                        "old_size": [cfg.size.0, cfg.size.1],
                                        "new_size": [new_size.0, new_size.1],
                                        "new_rect": s.rect().to_array(),
                                        "detail": "the video size is fixed; frames stay `lost` until the old size returns"
                                    }),
                                ));
                            }
                            backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
                            next_open_us = now + backoff_ms * 1000;
                        } else {
                            if resized_reported.is_some() {
                                resized_reported = None;
                            }
                            source = Some(s);
                            have_picture = false;
                            opened_at_us = now;
                        }
                    }
                    Err(e) => {
                        backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
                        next_open_us = now + backoff_ms * 1000;
                        if attempts == 1 || attempts % 5 == 0 {
                            events.event(capture_event(
                                cfg.monitor,
                                "reopen_failed",
                                json!({"t": now, "attempt": attempts, "reason": e, "next_try_ms": backoff_ms}),
                            ));
                        }
                    }
                }
            } else {
                let until_open_ms = ((next_open_us - now) / 1000).max(1) as u64;
                clock.sleep_ms(remaining_ms.min(until_open_ms).min(8));
            }
        }

        let state = if source.is_none() || !have_picture {
            FrameState::Lost
        } else if got_frame_after_open && lost_since.is_some() {
            FrameState::Reopened
        } else {
            FrameState::Ok
        };
        if state == FrameState::Reopened {
            let now = clock.now_us();
            let since = lost_since.take().unwrap_or(now);
            let lost_ms = (now - since) / 1000;
            stats.reopens += 1;
            stats.max_lost_ms = stats.max_lost_ms.max(lost_ms);
            let rect = source.as_ref().map(|s| s.rect()).unwrap_or_default();
            events.event(capture_event(
                cfg.monitor,
                "reopened",
                json!({
                    "t": now,
                    "lost_since": since,
                    "lost_ms": lost_ms,
                    "attempts": attempts,
                    "reason": lost_reason,
                    "rect": rect.to_array(),
                    "size": [cfg.size.0, cfg.size.1],
                }),
            ));
            eprintln!(
                "planor-rec: monitor {}: capture reopened after {lost_ms} ms",
                cfg.monitor
            );
            attempts = 0;
            backoff_ms = BACKOFF_START_MS;
        }
        stats.ticks += 1;
        if state == FrameState::Lost {
            stats.lost_ticks += 1;
        }
        let present_us = clock.now_us();
        let row = Row {
            tick,
            present_us,
            changed: changed && state != FrameState::Lost,
            state,
            rotate_stall_us: stall_us,
        };
        match sink.push(&previous, row) {
            Push::Written => {
                stats.written += 1;
                if dropped_unreported > 0 {
                    events.event(capture_event(
                        cfg.monitor,
                        "encoder_behind",
                        json!({"t": present_us, "dropped": dropped_unreported}),
                    ));
                    dropped_unreported = 0;
                    last_drop_report_us = present_us;
                }
            }
            Push::Dropped => {
                stats.dropped += 1;
                dropped_unreported += 1;
                if present_us - last_drop_report_us >= 1_000_000 {
                    events.event(capture_event(
                        cfg.monitor,
                        "encoder_behind",
                        json!({"t": present_us, "dropped": dropped_unreported}),
                    ));
                    dropped_unreported = 0;
                    last_drop_report_us = present_us;
                }
            }
            Push::Closed => {
                return Err(format!("monitor {}: the encoder pipe closed", cfg.monitor));
            }
        }
        tick += 1;
    }
    if dropped_unreported > 0 {
        events.event(capture_event(
            cfg.monitor,
            "encoder_behind",
            json!({"t": clock.now_us(), "dropped": dropped_unreported}),
        ));
    }
    if let Some(since) = lost_since {
        // Recording ended while the capture was still lost.
        let now = clock.now_us();
        stats.max_lost_ms = stats.max_lost_ms.max((now - since) / 1000);
        events.event(capture_event(
            cfg.monitor,
            "lost_at_stop",
            json!({"t": now, "lost_since": since, "lost_ms": (now - since) / 1000}),
        ));
    }
    Ok(stats)
}

// ---- Encoder queue ---------------------------------------------------------------------------

struct Item {
    buf: Vec<u8>,
    row: Row,
}

pub struct WriterStats {
    pub written: u64,
    pub error: Option<String>,
}

/// A bounded hand-off to the thread that feeds FFmpeg and writes the CSV. When the queue is full the
/// frame is dropped instead of blocking the capture loop; memory is bounded by `capacity + 2`
/// frames per monitor.
pub struct QueueSink {
    tx: Option<SyncSender<Item>>,
    pool: Receiver<Vec<u8>>,
    closed: Arc<AtomicBool>,
    handle: Option<JoinHandle<WriterStats>>,
}

impl QueueSink {
    pub fn new(
        mut video: Box<dyn Write + Send>,
        mut csv: Box<dyn Write + Send>,
        capacity: usize,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Item>(capacity.max(1));
        let (pool_tx, pool) = mpsc::channel::<Vec<u8>>();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_w = closed.clone();
        let handle = std::thread::spawn(move || {
            let mut stats = WriterStats {
                written: 0,
                error: None,
            };
            let _ = csv.write_all(CSV_HEADER.as_bytes());
            while let Ok(item) = rx.recv() {
                if let Err(e) = video.write_all(&item.buf) {
                    stats.error = Some(format!("FFmpeg pipe closed: {e}"));
                    closed_w.store(true, Ordering::SeqCst);
                    break;
                }
                let _ = csv.write_all(item.row.csv_line().as_bytes());
                stats.written += 1;
                if stats.written % 30 == 0 {
                    let _ = csv.flush();
                }
                let _ = pool_tx.send(item.buf);
            }
            let _ = csv.flush();
            drop(video);
            stats
        });
        Self {
            tx: Some(tx),
            pool,
            closed,
            handle: Some(handle),
        }
    }

    /// Close the queue, wait for everything queued to be written, and return the writer's stats.
    pub fn finish(mut self) -> WriterStats {
        self.tx.take();
        match self.handle.take().map(|h| h.join()) {
            Some(Ok(stats)) => stats,
            _ => WriterStats {
                written: 0,
                error: Some("encoder writer thread panicked".into()),
            },
        }
    }
}

impl FrameSink for QueueSink {
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
        match tx.try_send(Item { buf, row }) {
            Ok(()) => Push::Written,
            Err(TrySendError::Full(item)) => {
                // Keep the allocation; the next accepted frame reuses it.
                let _ = item.buf;
                Push::Dropped
            }
            Err(TrySendError::Disconnected(_)) => Push::Closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        rc::Rc,
    };

    struct FakeClock {
        now: Rc<Cell<i64>>,
    }
    impl Clock for FakeClock {
        fn now_us(&self) -> i64 {
            self.now.get()
        }
        fn sleep_ms(&self, ms: u64) {
            self.now.set(self.now.get() + ms as i64 * 1000);
        }
    }

    #[derive(Default)]
    struct Events(RefCell<Vec<Value>>);
    impl EventSink for Events {
        fn event(&self, value: Value) {
            self.0.borrow_mut().push(value);
        }
    }

    /// Delivers a new frame every 1 ms of fake time until it has delivered `frames`, then reports
    /// `fail` as a capture error.
    struct FakeSource {
        now: Rc<Cell<i64>>,
        size: (u32, u32),
        rect: Rect,
        frames: usize,
        fail: Option<String>,
        value: u8,
    }
    impl FrameSource for FakeSource {
        fn size(&self) -> (u32, u32) {
            self.size
        }
        fn rect(&self) -> Rect {
            self.rect
        }
        fn acquire_into(&mut self, out: &mut Vec<u8>, timeout_ms: u32) -> Result<bool, String> {
            if self.frames == 0 {
                if let Some(e) = self.fail.clone() {
                    self.now.set(self.now.get() + 1000);
                    return Err(e);
                }
                self.now.set(self.now.get() + timeout_ms as i64 * 1000);
                return Ok(false);
            }
            self.frames -= 1;
            self.now.set(self.now.get() + 1000);
            out.iter_mut().for_each(|b| *b = self.value);
            Ok(true)
        }
    }

    struct FakeOpener {
        now: Rc<Cell<i64>>,
        /// Each open attempt pops one answer.
        script: VecDeque<Result<(u32, u32), String>>,
        opened: usize,
    }
    impl Opener for FakeOpener {
        fn open(&mut self) -> Result<Box<dyn FrameSource>, String> {
            self.opened += 1;
            match self.script.pop_front().unwrap_or(Err("no more".into())) {
                Ok(size) => Ok(Box::new(FakeSource {
                    now: self.now.clone(),
                    size,
                    rect: Rect::new(0, 0, size.0 as i32, size.1 as i32),
                    frames: 1000,
                    fail: None,
                    value: 7,
                })),
                Err(e) => Err(e),
            }
        }
    }

    #[derive(Default)]
    struct VecSink {
        rows: Vec<Row>,
        stop_after: usize,
        stop: Option<Arc<AtomicBool>>,
        data: Vec<u8>,
    }
    impl FrameSink for VecSink {
        fn push(&mut self, bytes: &[u8], row: Row) -> Push {
            self.rows.push(row);
            self.data.push(bytes[0]);
            if self.rows.len() >= self.stop_after {
                if let Some(stop) = &self.stop {
                    stop.store(true, Ordering::SeqCst);
                }
            }
            Push::Written
        }
    }

    fn setup(
        now: &Rc<Cell<i64>>,
        source_frames: usize,
        ticks: usize,
    ) -> (Box<FakeSource>, VecSink, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let source = Box::new(FakeSource {
            now: now.clone(),
            size: (8, 4),
            rect: Rect::new(0, 0, 8, 4),
            frames: source_frames,
            fail: Some("DXGI_ERROR_ACCESS_LOST (0x887A0026)".into()),
            value: 1,
        });
        let sink = VecSink {
            stop_after: ticks,
            stop: Some(stop.clone()),
            ..Default::default()
        };
        (source, sink, stop)
    }

    #[test]
    fn a_dropout_keeps_the_60_fps_timeline_and_marks_lost_then_reopened() {
        let now = Rc::new(Cell::new(1_000_000));
        let clock = FakeClock { now: now.clone() };
        let (source, mut sink, stop) = setup(&now, 40, 240);
        // Reopen fails three times (back-off 100, 200, 400 ms) and then succeeds.
        let mut opener = FakeOpener {
            now: now.clone(),
            script: VecDeque::from(vec![
                Err("DXGI_ERROR_NOT_CURRENTLY_AVAILABLE".to_string()),
                Err("DXGI_ERROR_NOT_CURRENTLY_AVAILABLE".to_string()),
                Err("DXGI_ERROR_NOT_CURRENTLY_AVAILABLE".to_string()),
                Ok((8, 4)),
            ]),
            opened: 0,
        };
        let events = Events::default();
        let cfg = TimelineCfg::new(2, 60, (8, 4));
        let stats =
            run_timeline(&cfg, source, &mut opener, &mut sink, &events, &clock, &stop).unwrap();
        // Every tick has a row: no gap in the timeline.
        assert_eq!(sink.rows.len(), 240);
        assert_eq!(stats.dropped, 0);
        for pair in sink.rows.windows(2) {
            let gap = pair[1].present_us - pair[0].present_us;
            assert!(
                (15_000..=19_000).contains(&gap),
                "gap {gap} us between rows {} and {}",
                pair[0].tick,
                pair[1].tick
            );
        }
        let states: Vec<FrameState> = sink.rows.iter().map(|r| r.state).collect();
        let first_lost = states
            .iter()
            .position(|s| *s == FrameState::Lost)
            .expect("lost rows");
        let reopened = states
            .iter()
            .position(|s| *s == FrameState::Reopened)
            .expect("a reopened row");
        assert!(first_lost > 0 && first_lost < reopened);
        assert!(states[..first_lost].iter().all(|s| *s == FrameState::Ok));
        assert!(
            states[first_lost..reopened]
                .iter()
                .all(|s| *s == FrameState::Lost)
        );
        assert!(states[reopened + 1..].iter().all(|s| *s == FrameState::Ok));
        // Waits of 100, 200, 400 and 800 ms (back-off doubles after each failed attempt) before the
        // fourth attempt succeeds: about 1.5 s = 90 rows at 60 fps.
        let lost_rows = reopened - first_lost;
        assert!((85..=95).contains(&lost_rows), "{lost_rows} lost rows");
        assert_eq!(opener.opened, 4);
        // While lost the last picture is repeated (value 1), after the reopen the new one is shown (7).
        assert_eq!(sink.data[first_lost], 1);
        assert_eq!(sink.data[reopened], 7);
        // Events: lost, reopen_failed (first attempt), reopened.
        let events = events.0.borrow();
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| e["event"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"lost"), "{kinds:?}");
        assert!(kinds.contains(&"reopen_failed"), "{kinds:?}");
        let reopened_event = events
            .iter()
            .find(|e| e["event"] == "reopened")
            .expect("reopened event");
        assert_eq!(reopened_event["kind"], "capture");
        assert_eq!(reopened_event["monitor"], 2);
        let lost_ms = reopened_event["lost_ms"].as_i64().unwrap();
        assert!((1400..=1700).contains(&lost_ms), "lost_ms {lost_ms}");
        assert_eq!(
            reopened_event["reason"],
            "DXGI_ERROR_ACCESS_LOST (0x887A0026)"
        );
        assert_eq!(stats.reopens, 1);
        assert_eq!(stats.lost_periods, 1);
    }

    #[test]
    fn a_reopen_with_another_size_is_reported_as_topology_and_stays_lost() {
        let now = Rc::new(Cell::new(0));
        let clock = FakeClock { now: now.clone() };
        let (source, mut sink, stop) = setup(&now, 10, 120);
        let mut opener = FakeOpener {
            now: now.clone(),
            script: VecDeque::from(vec![Ok((16, 8)), Ok((16, 8)), Ok((16, 8)), Ok((16, 8))]),
            opened: 0,
        };
        let events = Events::default();
        let cfg = TimelineCfg::new(0, 60, (8, 4));
        let stats =
            run_timeline(&cfg, source, &mut opener, &mut sink, &events, &clock, &stop).unwrap();
        assert_eq!(sink.rows.len(), 120);
        assert_eq!(stats.reopens, 0);
        assert!(
            sink.rows
                .iter()
                .skip(12)
                .all(|r| r.state == FrameState::Lost)
        );
        let events = events.0.borrow();
        let topo: Vec<&Value> = events.iter().filter(|e| e["event"] == "topology").collect();
        assert_eq!(topo.len(), 1, "reported once per new size");
        assert_eq!(topo[0]["new_size"], json!([16, 8]));
        assert_eq!(topo[0]["new_rect"], json!([0, 0, 16, 8]));
        assert!(events.iter().any(|e| e["event"] == "lost_at_stop"));
    }

    #[test]
    fn a_topology_epoch_bump_reopens_a_healthy_source() {
        let now = Rc::new(Cell::new(0));
        let clock = FakeClock { now: now.clone() };
        let stop = Arc::new(AtomicBool::new(false));
        let epoch = Arc::new(AtomicU64::new(0));
        let source = Box::new(FakeSource {
            now: now.clone(),
            size: (8, 4),
            rect: Rect::new(0, 0, 8, 4),
            frames: 10_000,
            fail: None,
            value: 1,
        });
        // Bump the epoch from the sink after 30 rows.
        struct BumpSink {
            inner: VecSink,
            epoch: Arc<AtomicU64>,
        }
        impl FrameSink for BumpSink {
            fn push(&mut self, bytes: &[u8], row: Row) -> Push {
                if row.tick == 30 {
                    self.epoch.fetch_add(1, Ordering::SeqCst);
                }
                self.inner.push(bytes, row)
            }
        }
        let mut sink = BumpSink {
            inner: VecSink {
                stop_after: 90,
                stop: Some(stop.clone()),
                ..Default::default()
            },
            epoch: epoch.clone(),
        };
        let mut opener = FakeOpener {
            now: now.clone(),
            script: VecDeque::from(vec![Ok((8, 4))]),
            opened: 0,
        };
        let events = Events::default();
        let mut cfg = TimelineCfg::new(1, 60, (8, 4));
        cfg.topology_epoch = Some(epoch);
        let stats =
            run_timeline(&cfg, source, &mut opener, &mut sink, &events, &clock, &stop).unwrap();
        assert_eq!(opener.opened, 1);
        assert_eq!(stats.reopens, 1);
        assert_eq!(sink.inner.rows.len(), 90);
        assert!(
            sink.inner
                .rows
                .iter()
                .any(|r| r.state == FrameState::Reopened)
        );
        let events = events.0.borrow();
        assert!(events.iter().any(|e| e["event"] == "topology_reopen"));
        assert!(events.iter().any(|e| e["event"] == "reopened"));
    }

    #[test]
    fn a_slow_encoder_drops_frames_and_never_grows_the_queue_without_bound() {
        struct SlowWriter(Arc<AtomicU64>);
        impl Write for SlowWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(std::time::Duration::from_millis(30));
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        struct Shared(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let frames_written = Arc::new(AtomicU64::new(0));
        let csv = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut sink = QueueSink::new(
            Box::new(SlowWriter(frames_written.clone())),
            Box::new(Shared(csv.clone())),
            3,
        );
        let mut pushed = 0u64;
        let mut dropped = 0u64;
        for tick in 0..60u64 {
            let row = Row {
                tick,
                present_us: tick as i64 * 1000,
                changed: true,
                state: FrameState::Ok,
                rotate_stall_us: 0,
            };
            match sink.push(&vec![0u8; 1024], row) {
                Push::Written => pushed += 1,
                Push::Dropped => dropped += 1,
                Push::Closed => panic!("closed"),
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let stats = sink.finish();
        assert!(
            dropped > 20,
            "a 30 ms encoder fed every 2 ms must drop, dropped {dropped}"
        );
        assert_eq!(stats.written, pushed);
        assert_eq!(frames_written.load(Ordering::SeqCst), pushed);
        // Only accepted frames have a CSV row.
        let csv = String::from_utf8(csv.lock().unwrap().clone()).unwrap();
        assert_eq!(
            csv.lines().count() as u64,
            pushed + 1,
            "header plus one row per written frame"
        );
        assert!(csv.starts_with(CSV_HEADER));
    }

    #[test]
    fn csv_rows_carry_the_state_column() {
        let row = Row {
            tick: 5,
            present_us: 123,
            changed: false,
            state: FrameState::Lost,
            rotate_stall_us: 0,
        };
        assert_eq!(row.csv_line(), "5,123,0,lost,0\n");
        let stalled = Row {
            tick: 6,
            present_us: 200,
            changed: true,
            state: FrameState::Ok,
            rotate_stall_us: 34_000,
        };
        assert_eq!(stalled.csv_line(), "6,200,1,ok,34000\n");
        assert_eq!(
            CSV_HEADER,
            "index,present_qpc_us,changed,state,rotate_stall_us\n"
        );
        assert_eq!(FrameState::parse("reopened"), FrameState::Reopened);
        assert_eq!(FrameState::parse(""), FrameState::Ok);
    }
}

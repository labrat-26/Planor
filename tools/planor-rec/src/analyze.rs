//! Offline analysis: decode the video with FFmpeg, walk it against `frames-*.csv`, the OS window
//! snapshots and Planor's EXPECT trace, and write `report.json`, `report.md` and `suspects/`.
//!
//! Pixel maths only. The analyzer never calls an LLM.

use crate::{
    capture::{self, MonitorInfo},
    config::{BORDER_COLOURS, Config},
    events::OsWindow,
    image::{self, Image, Interior},
    record::{self, AudioMeta, CaptureStats, HotkeyStatus, Meta},
    timeline::FrameState,
    trace::{self, Trace},
    util::{self, Rect},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

// ---- Report types ------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    Unmeasurable,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::Unmeasurable => "unmeasurable",
        }
    }
    fn severity(self) -> u8 {
        match self {
            Verdict::Fail => 2,
            Verdict::Unmeasurable => 1,
            Verdict::Pass => 0,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub id: u64,
    pub kind: String,
    pub verdict: String,
    pub monitor: u32,
    pub hwnd: Option<i64>,
    pub frame_index: Option<usize>,
    pub time_qpc_us: i64,
    pub time_ms: i64,
    pub expected: Option<Rect>,
    pub reported: Option<Rect>,
    pub measured: Option<Rect>,
    pub diff_expected_reported_px: Option<i32>,
    pub diff_expected_measured_px: Option<i32>,
    pub diff_reported_measured_px: Option<i32>,
    pub detail: String,
    pub note: Option<String>,
    pub suspect: Option<String>,
    /// Length of a stretch (a lost capture) in ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    /// Lag: ms from Planor's EXPECT line to the first frame showing the window at its target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arrival_ms: Option<i64>,
    /// Lag: ms the window was late relative to the end of the configured animation (negative: early).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub late_ms: Option<i64>,
    #[serde(skip)]
    severity: u8,
}

#[derive(Clone, Debug, Serialize)]
pub struct Suspect {
    pub check_id: u64,
    pub monitor: u32,
    pub frame_index: Option<usize>,
    pub time_qpc_us: i64,
    pub time_ms: i64,
    pub kind: String,
    pub verdict: String,
    pub detail: String,
    pub note: Option<String>,
    pub expected: Option<Rect>,
    pub measured: Option<Rect>,
    pub crop: Option<String>,
    pub monitor_png: Option<String>,
    #[serde(default)]
    pub nearby_events: Vec<Value>,
}

#[derive(Serialize)]
struct Summary {
    checks: usize,
    fail: usize,
    unmeasurable: usize,
    pass: usize,
    suspects: usize,
}

#[derive(Serialize)]
struct RecordingInfo {
    dir: String,
    started_utc: String,
    fps: u32,
    qpc_frequency: i64,
    monitors: Vec<MonitorInfo>,
    trace: Option<String>,
    trace_lines: usize,
    trace_expectations: usize,
    config: Option<String>,
    /// Monitors with no Planor EXPECT/PLACE rectangles, analyzed against OS rectangles only (only when
    /// the trace has no EXPECT lines at all).
    os_fallback_monitors: Vec<u32>,
    /// Monitors where Planor has no tiles while the trace has EXPECT lines for other monitors: no position
    /// or border check is made there (Planor claims nothing there); black/frozen/pacing/capture still run.
    monitors_without_planor_tiles: Vec<u32>,
    analysis_version: String,
    /// Where the microphone file sits on the recorder's clock (a dictated note can be lined up with frames).
    audio: Option<AudioMeta>,
    capture: Vec<CaptureStats>,
    hotkeys: Vec<HotkeyStatus>,
    stop_reason: Option<String>,
    /// Per monitor: audio start minus that monitor's first video frame, in ms (positive: the audio
    /// starts after the first frame). A spoken note at audio time `a` ms is at video time `a + offset`.
    audio_offsets_to_first_frame_ms: Vec<AudioOffset>,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct AudioOffset {
    pub monitor: u32,
    pub first_frame_qpc_us: i64,
    pub offset_ms: i64,
}

/// Audio start relative to each monitor's first video frame.
pub fn audio_offsets(audio: &AudioMeta, first_frames: &[(u32, i64)]) -> Vec<AudioOffset> {
    first_frames
        .iter()
        .map(|&(monitor, first)| AudioOffset {
            monitor,
            first_frame_qpc_us: first,
            offset_ms: (audio.start_qpc_us - first) / 1000,
        })
        .collect()
}

#[derive(Serialize)]
struct Report {
    recording: RecordingInfo,
    summary: Summary,
    checks: Vec<Check>,
    suspects: Vec<Suspect>,
}

// ---- Inputs ------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct FrameRow {
    pub present_us: i64,
    pub state: FrameState,
    /// Recorder delay that is inside `present_us` (see `timeline::Row::rotate_stall_us`); 0 when not recorded.
    pub rotate_stall_us: i64,
}

#[derive(Clone, Debug)]
struct OsSnapshot {
    t_us: i64,
    windows: Vec<OsWindow>,
}

#[derive(Clone, Debug)]
struct Recording {
    meta: Meta,
    trace: Trace,
    trace_path: Option<PathBuf>,
    events: Vec<Value>,
    os: Vec<OsSnapshot>,
    marks: Vec<(i64, String)>,
    notes: Vec<trace::Note>,
    config: Config,
    dir: PathBuf,
}

fn load_events(path: &Path) -> util::anyhow_lite::Result<Vec<Value>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| util::fmt_err(format!("reading {}: {e}", path.display())))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if v.get("t").and_then(|t| t.as_i64()).is_some() {
                out.push(v);
            }
        }
    }
    out.sort_by_key(|v| v["t"].as_i64().unwrap_or(0));
    Ok(out)
}

fn extract_os(events: &[Value]) -> Vec<OsSnapshot> {
    let mut out = Vec::new();
    for e in events {
        if e.get("kind").and_then(|k| k.as_str()) != Some("os") {
            continue;
        }
        let Some(t_us) = e.get("t").and_then(|t| t.as_i64()) else {
            continue;
        };
        let windows = e
            .get("windows")
            .and_then(|w| w.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|w| serde_json::from_value::<OsWindow>(w.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        out.push(OsSnapshot { t_us, windows });
    }
    out.sort_by_key(|s| s.t_us);
    out
}

fn extract_marks(events: &[Value]) -> Vec<(i64, String)> {
    events
        .iter()
        .filter(|e| e.get("kind").and_then(|k| k.as_str()) == Some("mark"))
        .filter_map(|e| {
            Some((
                e.get("t")?.as_i64()?,
                e.get("text")?.as_str().unwrap_or("mark").to_string(),
            ))
        })
        .collect()
}

fn load_frames(path: &Path) -> util::anyhow_lite::Result<Vec<FrameRow>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| util::fmt_err(format!("reading {}: {e}", path.display())))?;
    let out = parse_frame_rows(&text)?;
    if out.is_empty() {
        return util::anyhow_lite::err(format!("{} has no frames", path.display()));
    }
    Ok(out)
}

/// The rows of a `frames-N.csv` file. Older recordings lack the `state` and `rotate_stall_us` columns.
fn parse_frame_rows(text: &str) -> util::anyhow_lite::Result<Vec<FrameRow>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || (i == 0 && line.starts_with("index")) {
            continue;
        }
        let mut parts = line.split(',');
        let _index = parts.next();
        let present: i64 = parts
            .next()
            .and_then(|v| v.trim().parse().ok())
            .ok_or_else(|| util::fmt_err(format!("bad frames csv line: {line}")))?;
        let _changed = parts.next().map(|v| v.trim() == "1").unwrap_or(false);
        // Older recordings have no `state` column; every row is then a healthy frame.
        let state = parts
            .next()
            .map(FrameState::parse)
            .unwrap_or(FrameState::Ok);
        let rotate_stall_us = parts
            .next()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        out.push(FrameRow {
            present_us: present,
            state,
            rotate_stall_us,
        });
    }
    Ok(out)
}

/// Lag limit of one move: `lag_ms`, widened by the recorder's own delay on the frame that showed the window at its
/// target. That frame's timestamp is taken after its rotation (a rotated monitor's frame costs 20 to 70 ms of
/// recorder CPU), so the real arrival can be that much earlier than measured. `stall_us` is zero for an unrotated
/// monitor, so only rotated monitors get the wider limit.
pub fn lag_limit_us(lag_ms: i64, stall_us: i64) -> i64 {
    lag_ms * 1000 + stall_us.max(0)
}

// ---- Expectation timeline ----------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Exp {
    hwnd: i64,
    rect: Rect,
    kind: String,
    hidden: bool,
    t_us: i64,
    animate_ms: u64,
    settle_us: i64,
    end_us: i64,
    /// The rectangle was already live when recording started (from a trace line older than the
    /// first frame); lag must not be measured for it.
    clamped: bool,
    /// First scene after a daemon start (see `trace::Expectation::startup`).
    startup: bool,
    /// Finger-driven scene (`trace::Expectation::live`): never lag-tracked.
    live: bool,
}

fn monitor_index_of(monitors: &[MonitorInfo], rect: Rect) -> Option<usize> {
    capture::monitor_of(monitors, rect)
}

fn merge_expectations(
    all: &[trace::Expectation],
    monitors: &[MonitorInfo],
    start_us: i64,
    end_us: i64,
    cfg: &Config,
) -> Vec<Exp> {
    let mut out: Vec<Exp> = Vec::new();
    let mut last: HashMap<i64, usize> = HashMap::new();
    let has_expect = all.iter().any(|e| e.kind != "place");
    // The windows each numbered scene lists. A scene is the complete tile list, so a window that was live and
    // is missing from the next scene is no longer expected (it is being dragged, floats, or was closed): its
    // old rectangle must not be measured any more.
    let mut scene_windows: HashMap<u64, Vec<i64>> = HashMap::new();
    for e in all {
        if e.kind != "place" && e.scene != 0 {
            scene_windows.entry(e.scene).or_default().push(e.hwnd);
        }
    }
    let mut current_scene = 0u64;
    for e in all {
        if has_expect && e.kind == "place" {
            // EXPECT is Planor's intended tile rectangle; PLACE is a side effect. Mixing both would
            // turn animation intermediate placements into expectations.
            continue;
        }
        if e.scene != 0 && e.scene != current_scene {
            current_scene = e.scene;
            let listed = &scene_windows[&e.scene];
            let gone: Vec<i64> = last
                .keys()
                .copied()
                .filter(|h| !listed.contains(h))
                .collect();
            for hwnd in gone {
                if let Some(idx) = last.remove(&hwnd) {
                    out[idx].end_us = e.t_us;
                }
            }
        }
        let on_monitor = monitor_index_of(monitors, e.rect).is_some();
        if let Some(&idx) = last.get(&e.hwnd) {
            let prev = &out[idx];
            if prev.rect == e.rect && prev.kind == e.kind && prev.hidden == e.hidden {
                // The same scene repeated; nothing changed. Keep the earlier settle time.
                continue;
            }
            out[idx].end_us = e.t_us;
            last.remove(&e.hwnd);
        }
        if !on_monitor {
            // Moved somewhere off every monitor (a parked/off-screen peek): the old lifetime is
            // already closed and there is nothing to measure here.
            continue;
        }
        let settle_us = e.t_us + e.animate_ms as i64 * 1000 + cfg.settle_margin_ms as i64 * 1000;
        out.push(Exp {
            hwnd: e.hwnd,
            rect: e.rect,
            kind: e.kind.clone(),
            hidden: e.hidden,
            t_us: e.t_us,
            animate_ms: e.animate_ms,
            settle_us,
            end_us,
            clamped: false,
            startup: e.startup,
            live: e.live,
        });
        last.insert(e.hwnd, out.len() - 1);
    }
    // The trace file also holds everything the daemon logged before this recording started. Keep
    // only the layout that was live when recording began, and never schedule a check before the
    // first frame.
    out.retain(|e| e.end_us > start_us && e.t_us < end_us);
    for e in &mut out {
        if e.t_us < start_us {
            e.t_us = start_us;
            // The animation happened before the recording; wait only the settle margin.
            e.settle_us = start_us + cfg.settle_margin_ms as i64 * 1000;
            e.clamped = true;
        }
        e.end_us = e.end_us.min(end_us);
        if e.end_us <= e.settle_us && e.settle_us >= end_us {
            e.end_us = end_us;
        }
    }
    out.sort_by_key(|e| e.t_us);
    out
}

/// Text of the marker that says "the daemon stops now" (`planor-rec mark daemon-stopped`; the test helpers
/// add it when a test stops its daemon). Every expectation ends at the first such marker.
pub const DAEMON_STOP_MARK: &str = "daemon-stopped";

fn daemon_stop_us(marks: &[(i64, String)]) -> Option<i64> {
    marks
        .iter()
        .filter(|(_, text)| text.trim() == DAEMON_STOP_MARK)
        .map(|(t, _)| *t)
        .min()
}

/// End every expectation at `stop_us`; those that had not begun by then are dropped.
fn end_expectations_at(exps: &mut Vec<Exp>, stop_us: i64) {
    exps.retain(|e| e.t_us < stop_us);
    for e in exps.iter_mut() {
        e.end_us = e.end_us.min(stop_us);
    }
}

/// OS-reported rectangles stand in for Planor's only when there is no Planor EXPECT trace at all. When
/// the trace has EXPECT lines but none for this monitor, Planor claims nothing there (the windows on it
/// are the user's own, and their DWM frames are not their visible pixels), so no position or border
/// check is made against them.
pub fn use_os_fallback(trace_has_expect: bool, monitor_has_expectations: bool) -> bool {
    !monitor_has_expectations && !trace_has_expect
}
/// When no Planor trace is available, treat OS-reported rectangles as expectations. This lets the
/// analyzer still say whether the screen matches what Windows says (labelled `os`, never claiming to
/// know Planor's intent).
fn synth_expectations_from_os(
    os: &[OsSnapshot],
    monitors: &[MonitorInfo],
    end_us: i64,
) -> Vec<Exp> {
    let ignored = [
        "Progman",
        "WorkerW",
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
    ];
    let mut out: Vec<Exp> = Vec::new();
    let mut last: HashMap<i64, usize> = HashMap::new();
    for snap in os {
        for w in &snap.windows {
            if w.cloaked || w.minimized || ignored.contains(&w.class.as_str()) {
                continue;
            }
            let on_monitor = monitor_index_of(monitors, w.rect).is_some();
            if let Some(&idx) = last.get(&w.hwnd) {
                if out[idx].rect == w.rect {
                    continue;
                }
                out[idx].end_us = snap.t_us;
                last.remove(&w.hwnd);
            }
            if !on_monitor {
                continue;
            }
            if w.rect.w < 16 || w.rect.h < 16 {
                // DWM ghost/utility windows with 1x1 frames are not worth measuring.
                continue;
            }
            out.push(Exp {
                hwnd: w.hwnd,
                rect: w.rect,
                kind: "os".into(),
                hidden: false,
                t_us: snap.t_us,
                animate_ms: 0,
                settle_us: snap.t_us,
                end_us,
                clamped: false,
                startup: false,
                live: false,
            });
            last.insert(w.hwnd, out.len() - 1);
        }
    }
    for e in &mut out {
        if e.end_us <= e.t_us {
            e.end_us = end_us;
        }
    }
    out.truncate(400);
    out
}

// ---- Jobs and measurement helpers --------------------------------------------------------------

#[derive(Clone, Debug)]
enum Job {
    Position { exp: usize, t_us: i64 },
    Interior { exp: usize, t_us: i64 },
    Border { t_us: i64 },
    Lag { track: usize },
}

struct LagTrack {
    target: Rect,
    /// Time the window should already be at `target` (after an animation, when applicable).
    event_us: i64,
    /// When Planor wrote the EXPECT line (the animation starts here); `None` for OS-rect lag.
    expect_us: Option<i64>,
    animate_ms: u64,
    max_score: f32,
    /// First matching frame's present time minus `event_us`; negative when the window arrived
    /// before the animation was due to end.
    latency_us: Option<i64>,
    arrival_us: Option<i64>,
    /// Recorder delay inside the arrival frame's timestamp (`lag_limit_us` widens the limit by it).
    arrival_stall_us: i64,
    done: bool,
    reported: bool,
    /// First scene after a daemon start: judged by `startup_latency` (arrival after EXPECT, limit
    /// `STARTUP_LATENCY_MS`), not by the strict lag-against-animation threshold.
    startup: bool,
}

/// The first scene after the daemon starts builds the GPU overlay and the window capture sources before it
/// can place anything (the trace logs "SLOW render scene handling: hosts X ms, sources Y ms"); that latency
/// is accepted behaviour but is measured: it fails only above this many ms after Planor's EXPECT line.
pub const STARTUP_LATENCY_MS: i64 = 1500;
/// How long after EXPECT the arrival of a window is watched: 1 s for a normal move, the startup limit plus
/// half a second for the startup scene.
const LAG_WATCH_US: i64 = 1_000_000;
const STARTUP_WATCH_US: i64 = (STARTUP_LATENCY_MS + 500) * 1000;

impl LagTrack {
    fn new(target: Rect, event_us: i64) -> Self {
        Self {
            target,
            event_us,
            expect_us: None,
            animate_ms: 0,
            max_score: 0.0,
            latency_us: None,
            arrival_us: None,
            arrival_stall_us: 0,
            done: false,
            reported: false,
            startup: false,
        }
    }

    fn with_expect(mut self, expect_us: i64, animate_ms: u64) -> Self {
        self.expect_us = Some(expect_us);
        self.animate_ms = animate_ms;
        self
    }

    fn with_startup(mut self, startup: bool) -> Self {
        self.startup = startup;
        self
    }

    /// How long after the EXPECT line (or the OS change) arrival is watched.
    fn watch_us(&self) -> i64 {
        if self.startup {
            STARTUP_WATCH_US
        } else {
            LAG_WATCH_US
        }
    }

    /// The moment after which the arrival question is settled even if the window never matched.
    fn watch_end_us(&self) -> i64 {
        self.expect_us.unwrap_or(self.event_us) + self.watch_us()
    }

    /// Has this expectation's arrival been resolved at `t_us`: the window matched its target, or the watch
    /// window has passed (then the `lag` / `startup_latency` finding says why it never did)?
    fn arrival_resolved(&self, t_us: i64) -> bool {
        self.done || t_us >= self.watch_end_us()
    }

    /// Feed one frame; records the latency the first time the target fits within tolerance.
    fn observe(
        &mut self,
        img: &Image,
        origin: (i32, i32),
        present_us: i64,
        stall_us: i64,
        cfg: &Config,
        layout: &image::LayoutContext,
    ) {
        if self.done {
            return;
        }
        let local = Rect::new(
            self.target.x - origin.0,
            self.target.y - origin.1,
            self.target.w,
            self.target.h,
        );
        let fit = image::fit_outline(img, local, cfg.edge_search_px, cfg.min_edge_score, layout);
        self.max_score = self.max_score.max(fit.score);
        if let Some(m) = fit.measured {
            // A side that was not measured keeps its expected coordinate in `measured`, so a fit that
            // only verified (say) the top and bottom of a tile would "match" while the tile is still a
            // hundred pixels away. Every side that lies inside the frame must really be measured.
            let required = [
                local.x >= 0,
                local.right() <= img.w,
                local.y >= 0,
                local.bottom() <= img.h,
            ];
            let verified = (0..4).all(|i| !required[i] || fit.sides[i]);
            if verified && m.max_side_diff(local) <= cfg.position_tolerance_px {
                self.latency_us = Some(present_us - self.event_us);
                self.arrival_us = self.expect_us.map(|e| present_us - e);
                self.arrival_stall_us = stall_us;
                self.done = true;
            }
        }
    }
}

fn frame_index(times: &[i64], t_us: i64) -> usize {
    times.partition_point(|&x| x < t_us)
}

fn active_expectations(exps: &[Exp], t_us: i64) -> impl Iterator<Item = &Exp> {
    exps.iter()
        .filter(move |e| e.t_us <= t_us && t_us < e.end_us)
}

/// True when no expected tile that is live at `t_us` is still inside its animation (plus margin).
fn all_settled(exps: &[Exp], t_us: i64) -> bool {
    active_expectations(exps, t_us).all(|e| t_us >= e.settle_us)
}
/// A frozen monitor: the picture has been identical since `static_since_us`, and activity (a window
/// moved, the pointer moved over this monitor) began at some `a >= static_since_us` that is already
/// `threshold_us` or more in the past at `now_us`. Returns that first activity time. Activity that only
/// just started cannot be judged yet: the pixels update a frame or two later, and a screen that sat idle
/// before the activity is not frozen.
pub fn frozen_since(
    activity: &[i64],
    static_since_us: i64,
    now_us: i64,
    threshold_us: i64,
) -> Option<i64> {
    let i = activity.partition_point(|&x| x < static_since_us);
    let first = *activity.get(i)?;
    (now_us - first >= threshold_us).then_some(first)
}
fn state_at(os: &[OsSnapshot], t_us: i64) -> Option<&[OsWindow]> {
    let idx = os.partition_point(|s| s.t_us <= t_us);
    if idx == 0 {
        None
    } else {
        Some(&os[idx - 1].windows)
    }
}

fn os_rect_at(os: &[OsSnapshot], hwnd: i64, t_us: i64) -> Option<Rect> {
    state_at(os, t_us)?
        .iter()
        .find(|w| w.hwnd == hwnd)
        .map(|w| w.rect)
}

/// Moments where windows on this monitor changed rectangle or z-order, or the (composited) mouse
/// pointer moved over it, i.e. moments when the recorder should have produced different pixels.
/// Keyboard input alone is not used: typing into an overlay can repaint without changing the
/// captured desktop and would otherwise look like a freeze.
fn monitor_activity_times(monitor: &MonitorInfo, events: &[Value], os: &[OsSnapshot]) -> Vec<i64> {
    let mut out = Vec::new();
    for e in events {
        if e.get("kind").and_then(|k| k.as_str()) == Some("input")
            && e.get("input").and_then(|i| i.as_str()) == Some("mouse")
        {
            let x = e.get("x").and_then(|v| v.as_i64()).unwrap_or(i64::MIN) as i32;
            let y = e.get("y").and_then(|v| v.as_i64()).unwrap_or(i64::MIN) as i32;
            if monitor.rect.contains(x, y) {
                if let Some(t) = e.get("t").and_then(|t| t.as_i64()) {
                    out.push(t);
                }
            }
        }
    }
    let mut previous: HashMap<i64, Rect> = HashMap::new();
    for (n, snap) in os.iter().enumerate() {
        let mut current: HashMap<i64, Rect> = HashMap::new();
        for w in &snap.windows {
            current.insert(w.hwnd, w.rect);
        }
        let mut changed = false;
        for (hwnd, rect) in &current {
            if rect.overlap_area(monitor.rect) > 0 && previous.get(hwnd).is_none_or(|p| p != rect) {
                changed = true;
            }
        }
        for (hwnd, rect) in &previous {
            if !current.contains_key(hwnd) && rect.overlap_area(monitor.rect) > 0 {
                changed = true;
            }
        }
        // The first snapshot only establishes what exists; it is not a window change (otherwise every
        // recording would start with a "windows changed" moment that the unchanged screen cannot show).
        if changed && n > 0 {
            out.push(snap.t_us);
        }
        previous = current;
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Position verdict, pure and unit-testable.
pub fn position_verdict(
    expected: Rect,
    reported: Option<Rect>,
    measured: Option<Rect>,
    tolerance: i32,
    edge_score: f32,
) -> (Verdict, String) {
    let Some(measured) = measured else {
        return (
            Verdict::Unmeasurable,
            format!(
                "outline not measurable (best edge score {edge_score:.3}); dark window on dark background?"
            ),
        );
    };
    let dm = expected.max_side_diff(measured);
    let dr = reported.map(|r| expected.max_side_diff(r));
    let rd = reported.map(|r| r.max_side_diff(measured));
    let mut bad = Vec::new();
    if dm > tolerance {
        bad.push(format!("expected vs measured differ by {dm} px"));
    }
    match dr {
        Some(d) if d > tolerance => bad.push(format!("expected vs OS differ by {d} px")),
        None => bad.push("no OS-reported rectangle for this window".into()),
        _ => {}
    }
    if let Some(d) = rd {
        if d > tolerance {
            bad.push(format!("OS vs measured differ by {d} px"));
        }
    }
    if reported.is_none() {
        (
            Verdict::Unmeasurable,
            format!("measured but no OS rectangle; {}", bad.join("; ")),
        )
    } else if bad.is_empty() {
        (Verdict::Pass, String::new())
    } else {
        (Verdict::Fail, bad.join("; "))
    }
}

/// A run of identical (black, flat, frozen) samples longer than a threshold.
#[derive(Default, Clone, Copy, Debug)]
pub struct RunTracker {
    start_us: Option<i64>,
    reported: bool,
}

impl RunTracker {
    fn reset(&mut self) {
        self.start_us = None;
        self.reported = false;
    }
    /// Observe `active`; returns true exactly once when the run crosses `threshold_ms`.
    fn observe(&mut self, active: bool, t_us: i64, threshold_ms: u64) -> bool {
        if !active {
            self.reset();
            return false;
        }
        let start = *self.start_us.get_or_insert(t_us);
        if !self.reported && t_us - start >= (threshold_ms as i64) * 1000 {
            self.reported = true;
            return true;
        }
        false
    }
}

/// Verdict of the `startup_latency` check: the arrival (ms after Planor's EXPECT line) of a window of the
/// first scene after a daemon start. Fails only above `STARTUP_LATENCY_MS`.
pub fn startup_verdict(arrival_ms: Option<i64>, lost_during: bool, edge_seen: bool) -> Verdict {
    match arrival_ms {
        Some(a) if a <= STARTUP_LATENCY_MS => Verdict::Pass,
        Some(_) if lost_during => Verdict::Unmeasurable,
        Some(_) => Verdict::Fail,
        None if lost_during || !edge_seen => Verdict::Unmeasurable,
        None => Verdict::Fail,
    }
}

/// A `slow_acquire` event of `monitor` (one capture call that blocked the recorder's tick loop) that overlaps
/// `[from_us, to_us]`, described for a pacing finding. A gap with such an event is the recorder stalling
/// itself, not the monitor or Windows.
pub fn slow_acquire_in(events: &[Value], monitor: u32, from_us: i64, to_us: i64) -> Option<String> {
    events.iter().find_map(|e| {
        if e.get("kind").and_then(|k| k.as_str()) != Some("capture")
            || e.get("event").and_then(|k| k.as_str()) != Some("slow_acquire")
            || e.get("monitor").and_then(|m| m.as_u64()) != Some(monitor as u64)
        {
            return None;
        }
        let t = e.get("t")?.as_i64()?;
        let ms = e.get("ms")?.as_i64()?;
        if t > to_us || t + ms * 1000 < from_us {
            return None;
        }
        let phase = |k: &str| e.get(k).and_then(|v| v.as_i64()).map(|v| format!("{v} ms")).unwrap_or_else(|| "?".into());
        Some(format!(
            "the recorder itself was inside one capture call for {ms} ms then (AcquireNextFrame {}, GPU copy+map {}, CPU copy/rotate {}): a recorder stall, not a display stall",
            phase("acquire_next_frame_ms"),
            phase("gpu_copy_map_ms"),
            phase("cpu_copy_rotate_ms")
        ))
    })
}

pub fn pacing_gaps(times: &[i64], threshold_ms: u64) -> Vec<(usize, i64)> {
    times
        .windows(2)
        .enumerate()
        .filter_map(|(i, w)| {
            let gap = w[1] - w[0];
            (gap > (threshold_ms as i64) * 1000).then_some((i + 1, gap))
        })
        .collect()
}

/// A run of timeline rows during which the recorder had no desktop duplication (the monitor went off,
/// changed mode, a UAC prompt, a driver reset). The pictures in these rows are a repeat of the last
/// good frame, so no pixel check may be run on them and they must not be read as a frozen or black
/// monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LostStretch {
    pub start_us: i64,
    pub end_us: i64,
    pub first_frame: usize,
    pub frames: usize,
}

impl LostStretch {
    pub fn duration_ms(&self) -> i64 {
        (self.end_us - self.start_us) / 1000
    }
}

pub fn lost_stretches(rows: &[FrameRow]) -> Vec<LostStretch> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        if rows[i].state != FrameState::Lost {
            i += 1;
            continue;
        }
        let first = i;
        while i < rows.len() && rows[i].state == FrameState::Lost {
            i += 1;
        }
        let end_us = if i < rows.len() {
            rows[i].present_us
        } else {
            // Lost until the recording ended: one more tick past the last row.
            let last = rows[rows.len() - 1].present_us;
            let prev = if rows.len() >= 2 {
                rows[rows.len() - 2].present_us
            } else {
                last
            };
            last + (last - prev).max(0)
        };
        out.push(LostStretch {
            start_us: rows[first].present_us,
            end_us,
            first_frame: first,
            frames: i - first,
        });
    }
    out
}

/// Stretches where the timeline has no rows for more than `max_gap_us` (frames the recorder dropped
/// because the encoder was behind). Pixel timing inside such a stretch is only an upper bound.
pub fn gap_stretches(times: &[i64], max_gap_us: i64) -> Vec<LostStretch> {
    times
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[1] - w[0] > max_gap_us)
        .map(|(i, w)| LostStretch {
            start_us: w[0],
            end_us: w[1],
            first_frame: i + 1,
            frames: 0,
        })
        .collect()
}
/// The reason the recorder logged for a loss starting near `t_us` on `monitor` (a `capture` event).
fn lost_reason(events: &[Value], monitor: u32, t_us: i64) -> Option<String> {
    events
        .iter()
        .filter(|e| {
            e.get("kind").and_then(|k| k.as_str()) == Some("capture")
                && e.get("event").and_then(|k| k.as_str()) == Some("lost")
                && e.get("monitor").and_then(|m| m.as_u64()) == Some(monitor as u64)
        })
        .filter_map(|e| {
            Some((
                e.get("t")?.as_i64()?,
                e.get("reason")?.as_str()?.to_string(),
            ))
        })
        .filter(|(t, _)| (t - t_us).abs() <= 250_000)
        .min_by_key(|(t, _)| (t - t_us).abs())
        .map(|(_, r)| r)
}

/// The first moment a `capture` event says this monitor's desktop rectangle is no longer the one in
/// `meta.json`. Frames from then on cannot be compared with the old rectangle.
pub fn rect_change(events: &[Value], monitor: &MonitorInfo) -> Option<(i64, Rect)> {
    let arr = |v: &Value| -> Option<Rect> {
        let a = v.as_array()?;
        Some(Rect::new(
            a.first()?.as_i64()? as i32,
            a.get(1)?.as_i64()? as i32,
            a.get(2)?.as_i64()? as i32,
            a.get(3)?.as_i64()? as i32,
        ))
    };
    for e in events {
        if e.get("kind").and_then(|k| k.as_str()) != Some("capture") {
            continue;
        }
        let Some(t) = e.get("t").and_then(|t| t.as_i64()) else {
            continue;
        };
        match e.get("event").and_then(|k| k.as_str()) {
            Some("reopened") | Some("topology")
                if e.get("monitor").and_then(|m| m.as_u64()) == Some(monitor.index as u64) =>
            {
                let rect = e.get("rect").or_else(|| e.get("new_rect")).and_then(arr);
                if let Some(r) = rect {
                    if r != monitor.rect {
                        return Some((t, r));
                    }
                }
            }
            Some("topology") => {
                if let Some(list) = e.get("monitors").and_then(|m| m.as_array()) {
                    for m in list {
                        let same = m.get("device").and_then(|d| d.as_str())
                            == Some(monitor.device.as_str());
                        if same {
                            if let Some(r) = m.get("rect").and_then(arr) {
                                if r != monitor.rect {
                                    return Some((t, r));
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// `encoder_behind` events of a monitor: the recorder dropped frames because FFmpeg fell behind.
fn dropped_frames(events: &[Value], monitor: u32) -> (u64, Option<i64>) {
    let mut total = 0;
    let mut first = None;
    for e in events {
        if e.get("kind").and_then(|k| k.as_str()) == Some("capture")
            && e.get("event").and_then(|k| k.as_str()) == Some("encoder_behind")
            && e.get("monitor").and_then(|m| m.as_u64()) == Some(monitor as u64)
        {
            total += e.get("dropped").and_then(|d| d.as_u64()).unwrap_or(0);
            if first.is_none() {
                first = e.get("t").and_then(|t| t.as_i64());
            }
        }
    }
    (total, first)
}

// ---- Runner ------------------------------------------------------------------------------------

/// Optional numbers for the next reported finding (set just before `Runner::report`).
#[derive(Default, Clone, Copy)]
struct Extra {
    duration_ms: Option<i64>,
    arrival_ms: Option<i64>,
    late_ms: Option<i64>,
}

struct Finding {
    kind: &'static str,
    verdict: Verdict,
    hwnd: Option<i64>,
    t_us: i64,
    frame: Option<usize>,
    expected: Option<Rect>,
    reported: Option<Rect>,
    measured: Option<Rect>,
    detail: String,
    crop: Option<Rect>,
}

struct Runner<'a> {
    cfg: &'a Config,
    monitor: &'a MonitorInfo,
    times: &'a [i64],
    exps: &'a [Exp],
    os: &'a [OsSnapshot],
    activity: &'a [i64],
    notes: &'a [trace::Note],
    marks: &'a [(i64, String)],
    taskbars: Vec<Rect>,
    layout: image::LayoutContext,
    base_us: i64,
    origin: (i32, i32),
    checks: Vec<Check>,
    suspects: Vec<Suspect>,
    suspects_dir: PathBuf,
    next_id: &'a mut u64,
    has_planor_expectations: bool,
    saw_border: bool,
    dedupe: HashMap<String, ()>,
    interior_run: HashMap<i64, (String, i64)>,
    interior_reported: HashMap<i64, i64>,
    black: RunTracker,
    prev_summary: Option<image::FrameSummary>,
    prev_t: i64,
    frozen_start: Option<i64>,
    frozen_reported: bool,
    lag: Vec<LagTrack>,
    /// The lag track that follows each expectation's arrival (`None`: nothing to wait for).
    exp_track: Vec<Option<usize>>,
    /// Position samples whose expectation had not arrived yet when they were due; run once it has.
    deferred: Vec<usize>,
    env: EnvTracker,
    extra: Extra,
    lost: Vec<LostStretch>,
}

/// Part of a free work area (outside every expected tile) that is not wallpaper for this share of the
/// samples marks a frame as "dirty": another program's window is showing where Planor shows the wallpaper.
pub const ENV_FOREIGN_FRACTION: f32 = 0.20;

/// Running tally of the "is the desktop clean" check of one monitor.
#[derive(Default, Clone, Debug)]
pub struct EnvTracker {
    frames: usize,
    dirty: usize,
    first_dirty: Option<(i64, usize, f32)>,
}

impl EnvTracker {
    pub fn observe(&mut self, t_us: i64, frame: usize, foreign: Option<f32>) {
        let Some(f) = foreign else { return };
        self.frames += 1;
        if f >= ENV_FOREIGN_FRACTION {
            self.dirty += 1;
            self.first_dirty.get_or_insert((t_us, frame, f));
        }
    }
    /// The environment is not clean when at least half of the measured settled frames were dirty.
    pub fn not_clean(&self) -> bool {
        self.dirty > 0 && self.dirty * 2 >= self.frames
    }
}

/// A window that is not one of Planor's tiles but sits above one of them in the z-order and covers at least a
/// quarter of it (the developer's own app raised over the scenario). `tiles` are (hwnd, rect) of the live
/// expectations. Returns that window's class. Planor's own overlay windows, the shell and tiny or cloaked or
/// minimised windows do not count.
pub fn occluding_foreign_window(windows: &[OsWindow], tiles: &[(i64, Rect)]) -> Option<String> {
    for (hwnd, rect) in tiles {
        let Some(tile_z) = windows.iter().find(|w| w.hwnd == *hwnd).map(|w| w.z) else {
            continue;
        };
        for w in windows {
            let shell = matches!(
                w.class.as_str(),
                "Shell_TrayWnd"
                    | "Shell_SecondaryTrayWnd"
                    | "Progman"
                    | "WorkerW"
                    | "XamlExplorerHostIslandWindow"
                    | "Windows.UI.Core.CoreWindow"
                    | "TopLevelWindowForOverflowXamlIsland"
            ) || w.class.starts_with("Planor");
            if w.z < tile_z
                && !shell
                && !w.cloaked
                && !w.minimized
                && w.rect.w >= 100
                && w.rect.h >= 100
                && !tiles.iter().any(|(h, _)| *h == w.hwnd)
                && w.rect.overlap_area(*rect) * 4 >= rect.area().max(1)
            {
                return Some(w.class.clone());
            }
        }
    }
    None
}

/// Turn failing pixel findings of a monitor whose environment is not clean into `unmeasurable` ones (a
/// foreign window showing through is not a Planor placement fault). Returns how many were downgraded.
fn downgrade_for_environment(
    checks: &mut [Check],
    suspects: &mut [Suspect],
    monitor: u32,
    reason: &str,
) -> usize {
    let mut n = 0;
    for c in checks.iter_mut() {
        if c.monitor == monitor
            && c.verdict == "fail"
            && matches!(
                c.kind.as_str(),
                "position" | "border" | "border_missing" | "lag" | "startup_latency"
            )
        {
            c.verdict = Verdict::Unmeasurable.as_str().into();
            c.severity = Verdict::Unmeasurable.severity();
            c.detail = format!("environment_not_clean ({reason}); not judged: {}", c.detail);
            for s in suspects.iter_mut().filter(|s| s.check_id == c.id) {
                s.verdict = Verdict::Unmeasurable.as_str().into();
            }
            n += 1;
        }
    }
    n
}

impl<'a> Runner<'a> {
    /// Has the arrival of expectation `exp_idx` been resolved at `t_us` (matched, or its watch time over)?
    /// Until then only the lag finding applies; a position or border check would just repeat it.
    fn arrived(&self, exp_idx: usize, t_us: i64) -> bool {
        match self.exp_track.get(exp_idx).copied().flatten() {
            Some(track) => self.lag[track].arrival_resolved(t_us),
            None => true,
        }
    }

    fn all_arrived(&self, t_us: i64) -> bool {
        self.exps
            .iter()
            .enumerate()
            .filter(|(_, e)| e.t_us <= t_us && t_us < e.end_us)
            .all(|(i, _)| self.arrived(i, t_us))
    }

    /// A position sample is due: run it now when the tile has arrived, otherwise hold it until it has.
    fn position_job(&mut self, img: &Image, exp_idx: usize, t_us: i64, frame: usize) {
        if !self.arrived(exp_idx, t_us) {
            if !self.deferred.contains(&exp_idx) {
                self.deferred.push(exp_idx);
            }
            return;
        }
        self.sample_environment(img, t_us, frame);
        self.check_position(img, exp_idx, t_us, frame);
    }

    /// Run the position samples that were waiting for an arrival, now that it may have resolved.
    fn run_deferred(&mut self, img: &Image, t_us: i64, frame: usize) {
        if self.deferred.is_empty() {
            return;
        }
        let waiting = std::mem::take(&mut self.deferred);
        for exp_idx in waiting {
            if t_us >= self.exps[exp_idx].end_us {
                continue; // superseded by a newer expectation of the same window
            }
            if self.arrived(exp_idx, t_us) {
                self.sample_environment(img, t_us, frame);
                self.check_position(img, exp_idx, t_us, frame);
            } else {
                self.deferred.push(exp_idx);
            }
        }
    }

    /// Tally how much of the free work area (outside every live tile) is foreign content.
    fn sample_environment(&mut self, img: &Image, t_us: i64, frame: usize) {
        if !self.has_planor_expectations {
            return;
        }
        let mut excluded: Vec<Rect> = active_expectations(self.exps, t_us)
            .filter(|e| !e.hidden && e.kind != "parked")
            .map(|e| self.local(e.rect).expand(24))
            .collect();
        excluded.extend(self.taskbars.iter().copied());
        let work = self.local(self.monitor.work);
        let mut fraction = image::foreign_content_fraction(img, work, &excluded, 8);
        // A foreign window raised above a tile is just as bad as one showing beside it.
        let tiles: Vec<(i64, Rect)> = active_expectations(self.exps, t_us)
            .filter(|e| !e.hidden && e.kind != "parked")
            .map(|e| (e.hwnd, e.rect))
            .collect();
        if let Some(windows) = state_at(self.os, t_us) {
            if occluding_foreign_window(windows, &tiles).is_some() {
                fraction = Some(1.0);
            }
        }
        self.env.observe(t_us, frame, fraction);
    }

    /// A timeline row with no live capture: the picture is a repeat of the last good one. Forget every
    /// running "black / flat / frozen" run so the stretch is never mistaken for a monitor fault.
    fn on_lost_frame(&mut self) {
        self.black.reset();
        self.prev_summary = None;
        self.frozen_start = None;
        self.frozen_reported = false;
        self.interior_run.clear();
    }

    fn overlaps_lost(&self, from_us: i64, to_us: i64) -> bool {
        self.lost
            .iter()
            .any(|l| l.start_us < to_us && l.end_us > from_us)
    }

    fn local(&self, r: Rect) -> Rect {
        Rect::new(r.x - self.origin.0, r.y - self.origin.1, r.w, r.h)
    }

    fn screen(&self, r: Rect) -> Rect {
        Rect::new(r.x + self.origin.0, r.y + self.origin.1, r.w, r.h)
    }

    fn note_near(&self, t_us: i64) -> Option<String> {
        let mut best: Option<(i64, String)> = None;
        for (mt, text) in self.marks {
            let d = (mt - t_us).abs();
            if d <= 5_000_000 && best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                best = Some((
                    d,
                    format!("mark [+{} ms] {text}", (mt - self.base_us) / 1000),
                ));
            }
        }
        for n in self.notes {
            let d = (n.t_us - t_us).abs();
            if d <= 5_000_000 && best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                best = Some((
                    d,
                    format!("NOTE [+{} ms] {}", (n.t_us - self.base_us) / 1000, n.text),
                ));
            }
        }
        best.map(|(_, s)| s)
    }

    fn report(&mut self, f: Finding, img: Option<&Image>) {
        let key = if f.kind == "border" {
            // One report per distinct stray border location; a static false positive must not
            // reappear every sample.
            format!(
                "border|{}|{:?}",
                f.verdict.as_str(),
                f.measured.map(|r| r.to_array())
            )
        } else if f.kind == "lag" || f.kind == "startup_latency" {
            format!(
                "lag|{}|{:?}|{}",
                f.verdict.as_str(),
                f.expected.map(|r| r.to_array()),
                f.t_us / 500_000
            )
        } else if f.kind == "border_missing" {
            format!(
                "border_missing|{:?}|{:?}",
                f.hwnd,
                f.expected.map(|r| r.to_array())
            )
        } else {
            format!(
                "{}|{:?}|{}|{}",
                f.kind,
                f.hwnd,
                f.verdict.as_str(),
                f.t_us / 500_000
            )
        };
        if f.verdict != Verdict::Pass && self.dedupe.insert(key, ()).is_some() {
            return;
        }
        if self.checks.len() >= 6000 {
            return;
        }
        let id = *self.next_id;
        *self.next_id += 1;
        let note = self.note_near(f.t_us);
        let extra = std::mem::take(&mut self.extra);
        let mut suspect = None;
        let mut monitor_png = None;
        if f.verdict != Verdict::Pass {
            if let Some(img) = img {
                let crop = f
                    .crop
                    .unwrap_or(Rect::new(0, 0, img.w, img.h))
                    .expand(48)
                    .clamp_to(Rect::new(0, 0, img.w, img.h));
                if !crop.is_empty() {
                    let crop_img = img.crop(crop);
                    let path = self.suspects_dir.join(format!("suspect-{id}.png"));
                    if image::write_png(&path, &crop_img.scaled_to_width(1200)).is_ok() {
                        suspect = Some(format!(
                            "suspects/{}",
                            path.file_name().unwrap().to_string_lossy()
                        ));
                    }
                }
                let thumb = img.scaled_to_width(640);
                let thumb_path = self.suspects_dir.join(format!("suspect-{id}-monitor.png"));
                if image::write_png(&thumb_path, &thumb).is_ok() {
                    monitor_png = Some(format!(
                        "suspects/{}",
                        thumb_path.file_name().unwrap().to_string_lossy()
                    ));
                }
            }
            self.suspects.push(Suspect {
                check_id: id,
                monitor: self.monitor.index,
                frame_index: f.frame,
                time_qpc_us: f.t_us,
                time_ms: (f.t_us - self.base_us) / 1000,
                kind: f.kind.to_string(),
                verdict: f.verdict.as_str().to_string(),
                detail: f.detail.clone(),
                note: note.clone(),
                expected: f.expected,
                measured: f.measured,
                crop: suspect.clone(),
                monitor_png,
                nearby_events: Vec::new(),
            });
        }
        let diff_er = match (f.expected, f.reported) {
            (Some(e), Some(r)) => Some(e.max_side_diff(r)),
            _ => None,
        };
        let diff_em = match (f.expected, f.measured) {
            (Some(e), Some(m)) => Some(e.max_side_diff(m)),
            _ => None,
        };
        let diff_rm = match (f.reported, f.measured) {
            (Some(r), Some(m)) => Some(r.max_side_diff(m)),
            _ => None,
        };
        self.checks.push(Check {
            id,
            kind: f.kind.to_string(),
            verdict: f.verdict.as_str().to_string(),
            monitor: self.monitor.index,
            hwnd: f.hwnd,
            frame_index: f.frame,
            time_qpc_us: f.t_us,
            time_ms: (f.t_us - self.base_us) / 1000,
            expected: f.expected,
            reported: f.reported,
            measured: f.measured,
            diff_expected_reported_px: diff_er,
            diff_expected_measured_px: diff_em,
            diff_reported_measured_px: diff_rm,
            detail: f.detail,
            note,
            suspect,
            duration_ms: extra.duration_ms,
            arrival_ms: extra.arrival_ms,
            late_ms: extra.late_ms,
            severity: f.verdict.severity(),
        });
    }

    fn check_position(&mut self, img: &Image, exp_idx: usize, t_us: i64, frame: usize) {
        let exp = self.exps[exp_idx].clone();
        if exp.hidden || exp.kind == "parked" {
            return;
        }
        let expected = exp.rect;
        let reported = os_rect_at(self.os, exp.hwnd, t_us);
        // Planor parks the real window off every monitor and shows a stand-in image of it (screen-edge peeks,
        // group previews). The OS rectangle is then the parking spot by design; only the picture can be
        // compared with the expected rectangle.
        let mon = Rect::new(
            self.monitor.rect.x,
            self.monitor.rect.y,
            self.monitor.rect.w,
            self.monitor.rect.h,
        );
        let parked =
            reported.is_some_and(|r| r.overlap_area(mon) == 0) && expected.overlap_area(mon) > 0;
        let reported_for_verdict = if parked { Some(expected) } else { reported };
        let local = self.local(expected);
        let frame_rect = Rect::new(0, 0, img.w, img.h);
        let visible = local.intersection(frame_rect);
        if local.w < 16 || local.h < 16 || visible.area() * 2 < local.area().max(1) {
            let percent = if local.area() > 0 {
                visible.area() * 100 / local.area()
            } else {
                0
            };
            self.report(
                Finding {
                    kind: "position",
                    verdict: Verdict::Unmeasurable,
                    hwnd: Some(exp.hwnd),
                    t_us,
                    frame: Some(frame),
                    expected: Some(expected),
                    reported,
                    measured: None,
                    detail: format!(
                        "expected rectangle is mostly off this monitor ({percent}% visible); its parked stand-in is not measurable here"
                    ),
                    crop: Some(local),
                },
                Some(img),
            );
            return;
        }
        let fit = image::fit_outline(
            img,
            local,
            self.cfg.edge_search_px,
            self.cfg.min_edge_score,
            &self.layout,
        );
        let measured = fit.measured.map(|m| self.screen(m));
        let (mut verdict, mut detail) = position_verdict(
            expected,
            reported_for_verdict,
            measured,
            self.cfg.position_tolerance_px,
            fit.score,
        );
        if parked {
            if verdict == Verdict::Fail {
                // A parked window's stand-in may be occluded or drawn elsewhere; a mismatch is not
                // proof that Planor's placement is wrong. Prefer unmeasurable over a wrong fail.
                verdict = Verdict::Unmeasurable;
                detail = format!(
                    "stand-in image does not sit at the expected rectangle ({detail}); the real window is parked off-screen"
                );
            } else if verdict == Verdict::Unmeasurable && fit.measured.is_none() {
                detail = "stand-in image not visible at the expected rectangle".into();
            } else if verdict == Verdict::Pass {
                detail = "(window parked off-screen; measured its stand-in image)".into();
            }
        }
        if verdict == Verdict::Pass {
            // Keep the evidence for a pass too: which sides were measured and how.
            if !fit.notes.is_empty() || !fit.reason.is_empty() {
                let mut evidence = format!("measured: {}", fit.notes.join("; "));
                if !fit.reason.is_empty() {
                    evidence.push_str(&format!("; unmeasured: {}", fit.reason));
                }
                detail = evidence;
            }
        } else if !fit.reason.is_empty() {
            detail = if detail.is_empty() {
                fit.reason.clone()
            } else {
                format!("{detail}; {}", fit.reason)
            };
        }
        if verdict == Verdict::Fail && !fit.notes.is_empty() {
            detail = format!("{detail} [{}]", fit.notes.join(", "));
        }
        self.report(
            Finding {
                kind: "position",
                verdict,
                hwnd: Some(exp.hwnd),
                t_us,
                frame: Some(frame),
                expected: Some(expected),
                reported,
                measured,
                detail,
                crop: Some(local),
            },
            Some(img),
        );
    }

    fn check_interior(&mut self, img: &Image, exp_idx: usize, t_us: i64, frame: usize) {
        let exp = self.exps[exp_idx].clone();
        if exp.hidden || exp.kind == "parked" {
            return;
        }
        // A window that has not been placed yet shows the desktop where its interior will be: that is the
        // lag (or startup latency) finding, not a window that "may not be painting".
        if !self.arrived(exp_idx, t_us) {
            self.interior_run.remove(&exp.hwnd);
            return;
        }
        let local = self.local(exp.rect).inset(6);
        if local.is_empty() || local.intersection(Rect::new(0, 0, img.w, img.h)).is_empty() {
            return;
        }
        let background = image::surrounding_luminance(img, local);
        let (class, stats) = image::classify_interior(img, local, background);
        let state = match class {
            Interior::Black => "black",
            Interior::Flat => "flat",
            Interior::Ok => {
                self.interior_run.remove(&exp.hwnd);
                return;
            }
            Interior::Unmeasurable => {
                self.interior_run.remove(&exp.hwnd);
                if self
                    .interior_reported
                    .get(&exp.hwnd)
                    .is_none_or(|&last| t_us - last > 2_000_000)
                {
                    self.interior_reported.insert(exp.hwnd, t_us);
                    self.report(
                        Finding {
                            kind: "interior",
                            verdict: Verdict::Unmeasurable,
                            hwnd: Some(exp.hwnd),
                            t_us,
                            frame: Some(frame),
                            expected: Some(exp.rect),
                            reported: os_rect_at(self.os, exp.hwnd, t_us),
                            measured: None,
                            detail: format!(
                                "interior does not separate from the background (mean {:.1}, sd {:.1})",
                                stats.mean, stats.std_dev
                            ),
                            crop: Some(local),
                        },
                        Some(img),
                    );
                }
                return;
            }
        };
        let entry = self
            .interior_run
            .entry(exp.hwnd)
            .or_insert((state.to_string(), t_us));
        if entry.0 != state {
            *entry = (state.to_string(), t_us);
        }
        let duration = t_us - entry.1;
        if duration >= (self.cfg.black_flat_ms as i64) * 1000
            && self
                .interior_reported
                .get(&exp.hwnd)
                .is_none_or(|&last| t_us - last > 2_000_000)
        {
            self.interior_reported.insert(exp.hwnd, t_us);
            self.report(
                Finding {
                    kind: "interior",
                    verdict: Verdict::Fail,
                    hwnd: Some(exp.hwnd),
                    t_us,
                    frame: Some(frame),
                    expected: Some(exp.rect),
                    reported: os_rect_at(self.os, exp.hwnd, t_us),
                    measured: None,
                    detail: format!(
                        "interior {state} for {} ms (mean {:.1}, sd {:.1}); window may not be painting",
                        duration / 1000,
                        stats.mean,
                        stats.std_dev
                    ),
                    crop: Some(local),
                },
                Some(img),
            );
        }
    }

    fn check_borders(&mut self, img: &Image, t_us: i64, frame: usize) {
        if !self.has_planor_expectations {
            return;
        }
        // A border animates with its tile; while any expected tile is still moving it is not on the
        // target outline yet, which is not a misplaced border.
        if !all_settled(self.exps, t_us) {
            return;
        }
        // Nor is a window that has not arrived yet (the first scene after the daemon start) on its outline.
        if !self.all_arrived(t_us) {
            return;
        }
        self.sample_environment(img, t_us, frame);
        let boxes = image::find_border_boxes(
            img,
            &BORDER_COLOURS,
            self.cfg.border_colour_tolerance,
            40,
            24,
        );
        if boxes.is_empty() {
            return;
        }
        let expected: Vec<Rect> = active_expectations(self.exps, t_us)
            .filter(|e| !e.hidden && e.kind != "parked")
            .map(|e| self.local(e.rect))
            .collect();
        let work = self.local(self.monitor.work);
        // `taskbars` is already stored in monitor-local coordinates.
        let taskbars: Vec<Rect> = self.taskbars.clone();
        let mut any_on_outline = false;
        for b in &boxes {
            // A thin line in a border colour (an Explorer selection row, an underline) is only called a stray
            // border when it is over the taskbar or outside the work area; "not on any tile outline" needs a
            // box that is large in both directions and has at least 3 of its 4 sides, because lines and
            // partial frames of that colour are common window content (`image::judge_border_box`).
            let name = BORDER_COLOURS[b.colour].0;
            let hex = hex(BORDER_COLOURS[b.colour].1);
            let (geometry, stray) = image::judge_border_box(
                img,
                b,
                BORDER_COLOURS[b.colour].1,
                &expected,
                work,
                &taskbars,
                self.cfg.border_tolerance_px,
                self.cfg.border_colour_tolerance,
            );
            let (verdict, detail) = match geometry {
                image::BorderVerdict::OnOutline => {
                    any_on_outline = true;
                    continue;
                }
                image::BorderVerdict::OverTaskbar => (
                    Verdict::Fail,
                    format!(
                        "{name} border ({hex}) painted over the taskbar at {},{} {}x{}",
                        b.rect.x, b.rect.y, b.rect.w, b.rect.h
                    ),
                ),
                image::BorderVerdict::OutsideWorkArea => (
                    Verdict::Fail,
                    format!(
                        "{name} border ({hex}) outside the monitor work area at {},{} {}x{}",
                        b.rect.x, b.rect.y, b.rect.w, b.rect.h
                    ),
                ),
                image::BorderVerdict::NotOnOutline if !stray => continue,
                image::BorderVerdict::NotOnOutline => (
                    Verdict::Fail,
                    format!(
                        "{name} border ({hex}) not on any expected tile outline at {},{} {}x{}",
                        b.rect.x, b.rect.y, b.rect.w, b.rect.h
                    ),
                ),
            };
            self.report(
                Finding {
                    kind: "border",
                    verdict,
                    hwnd: None,
                    t_us,
                    frame: Some(frame),
                    expected: None,
                    reported: None,
                    measured: Some(self.screen(b.rect)),
                    detail,
                    crop: Some(b.rect),
                },
                Some(img),
            );
        }
        if any_on_outline {
            self.saw_border = true;
        }
        // Missing border (only once Planor has been seen painting borders at all, so a config with
        // borders disabled is not reported as broken): the foreground managed tile should carry a
        // border in one of Planor's colours on its outline.
        if self.saw_border {
            if let Some(fg_hwnd) = state_at(self.os, t_us)
                .and_then(|w| w.iter().find(|w| w.foreground))
                .map(|w| w.hwnd)
            {
                if let Some(exp) = active_expectations(self.exps, t_us).find(|e| {
                    e.hwnd == fg_hwnd
                        && !e.hidden
                        && e.kind == "tile"
                        && e.rect.w >= 64
                        && e.rect.h >= 64
                        && t_us >= e.settle_us
                }) {
                    let local = self.local(exp.rect);
                    let covered = boxes.iter().any(|b| {
                        image::outline_coverage(
                            img,
                            b.rect,
                            BORDER_COLOURS[b.colour].1,
                            self.cfg.border_colour_tolerance,
                            &[local],
                            self.cfg.border_tolerance_px,
                        ) >= 0.9
                    });
                    if !covered {
                        self.report(
                            Finding {
                                kind: "border_missing",
                                verdict: Verdict::Fail,
                                hwnd: Some(exp.hwnd),
                                t_us,
                                frame: Some(frame),
                                expected: Some(exp.rect),
                                reported: None,
                                measured: None,
                                detail: "the focused tile has no Planor border colour on its expected outline"
                                    .into(),
                                crop: Some(local),
                            },
                            Some(img),
                        );
                    }
                }
            }
        }
    }

    fn observe_frame(&mut self, img: &Image, t_us: i64, frame: usize) {
        let summary = image::summarize(img);
        let active = active_expectations(self.exps, t_us).next().is_some();
        if self
            .black
            .observe(summary.mean < 3.0 && active, t_us, self.cfg.black_flat_ms)
        {
            self.report(
                Finding {
                    kind: "monitor_black",
                    verdict: Verdict::Fail,
                    hwnd: None,
                    t_us,
                    frame: Some(frame),
                    expected: None,
                    reported: None,
                    measured: None,
                    detail: format!(
                        "monitor is black (mean luminance {:.1}) for at least {} ms while windows should be visible",
                        summary.mean, self.cfg.black_flat_ms
                    ),
                    crop: None,
                },
                Some(img),
            );
        }
        if let Some(prev) = &self.prev_summary {
            if image::is_static(prev, &summary) {
                let start = *self.frozen_start.get_or_insert(self.prev_t);
                if !self.frozen_reported && t_us - start >= (self.cfg.black_flat_ms as i64) * 1000 {
                    // A window moved/resized (or the pointer moved here) and the monitor still did not
                    // change for the threshold afterwards: that is a freeze.
                    let frozen = frozen_since(
                        self.activity,
                        start,
                        t_us,
                        (self.cfg.black_flat_ms as i64) * 1000,
                    );
                    if let Some(start) = frozen {
                        self.frozen_reported = true;
                        let start_frame = frame_index(self.times, start);
                        self.report(
                            Finding {
                                kind: "monitor_frozen",
                                verdict: Verdict::Fail,
                                hwnd: None,
                                t_us: start,
                                frame: Some(start_frame),
                                expected: None,
                                reported: None,
                                measured: None,
                                detail: format!(
                                    "no pixel change for at least {} ms while input or windows were changing",
                                    self.cfg.black_flat_ms
                                ),
                                crop: None,
                            },
                            Some(img),
                        );
                    }
                }
            } else {
                self.frozen_start = None;
                self.frozen_reported = false;
            }
        }
        self.prev_summary = Some(summary);
        self.prev_t = t_us;
    }

    fn observe_lag(&mut self, img: &Image, track: usize, present_us: i64, stall_us: i64) {
        let origin = self.origin;
        let cfg = self.cfg;
        let layout = &self.layout;
        self.lag[track].observe(img, origin, present_us, stall_us, cfg, layout);
    }

    fn finish_lag(&mut self) {
        for i in 0..self.lag.len() {
            if self.lag[i].reported {
                continue;
            }
            let from_us = self.lag[i].expect_us.unwrap_or(self.lag[i].event_us);
            let until_us = match self.lag[i].latency_us {
                Some(latency) => self.lag[i].event_us + latency,
                None => self.lag[i].event_us + 1_000_000,
            };
            let until_us = if self.lag[i].startup {
                self.lag[i].watch_end_us()
            } else {
                until_us
            };
            let lost_during = self.overlaps_lost(from_us, until_us);
            if self.lag[i].startup {
                // First scene after the daemon start: the arrival time is reported as a number and fails only
                // above STARTUP_LATENCY_MS; it is never held to the strict lag-against-animation threshold.
                let arrival_ms = self.lag[i].arrival_us.map(|a| a / 1000);
                let verdict = startup_verdict(
                    arrival_ms,
                    lost_during,
                    self.lag[i].max_score >= self.cfg.min_edge_score,
                );
                let detail = match arrival_ms {
                    Some(a) if lost_during && a > STARTUP_LATENCY_MS => format!(
                        "startup: the window reached its rectangle {a} ms after Planor's EXPECT line, but frames are missing in that window, so this is only an upper bound"
                    ),
                    Some(a) => format!(
                        "startup (first scene after the daemon start): the window reached the expected rectangle {a} ms after Planor's EXPECT line (limit {STARTUP_LATENCY_MS} ms; the GPU overlay and window capture sources are built before the first placement)"
                    ),
                    None if lost_during => "startup: capture was lost or frames were dropped while the first scene was being measured; the arrival is not known".to_string(),
                    None => format!(
                        "startup: the window was not measured at the expected rectangle within {} ms of Planor's EXPECT line (limit {STARTUP_LATENCY_MS} ms, best edge score {:.3})",
                        STARTUP_WATCH_US / 1000,
                        self.lag[i].max_score
                    ),
                };
                self.extra.arrival_ms = arrival_ms;
                let target = self.lag[i].target;
                let expect_us = self.lag[i].expect_us.unwrap_or(self.lag[i].event_us);
                self.lag[i].reported = true;
                self.report(
                    Finding {
                        kind: "startup_latency",
                        verdict,
                        hwnd: None,
                        t_us: expect_us,
                        frame: None,
                        expected: Some(target),
                        reported: None,
                        measured: None,
                        detail,
                        crop: None,
                    },
                    None,
                );
                continue;
            }
            // The rotated-monitor allowance is the recorder's own stall on the arrival frame (zero when unrotated).
            let stall_us = self.lag[i].arrival_stall_us;
            let limit_us = lag_limit_us(self.cfg.lag_ms, stall_us);
            let limit_detail = if stall_us > 0 {
                format!(
                    "threshold {} ms + {} ms recorder stall on the arrival frame (rotated monitor)",
                    self.cfg.lag_ms,
                    stall_us / 1000
                )
            } else {
                format!("threshold {} ms", self.cfg.lag_ms)
            };
            let verdict = match self.lag[i].latency_us {
                Some(latency) if latency <= limit_us => Verdict::Pass,
                // Frames are missing in the window: the arrival is only an upper bound.
                Some(_) if lost_during => Verdict::Unmeasurable,
                Some(_) => Verdict::Fail,
                None if lost_during => Verdict::Unmeasurable,
                None if self.lag[i].max_score < self.cfg.min_edge_score => Verdict::Unmeasurable,
                None => Verdict::Fail,
            };
            let detail = match self.lag[i].latency_us {
                Some(latency) if lost_during && latency > limit_us => format!(
                    "first matching frame was {} ms after the end of the animation, but frames are missing in that window (lost capture or dropped frames), so the arrival is only an upper bound",
                    latency / 1000
                ),
                Some(latency) => match self.lag[i].arrival_us {
                    Some(arrival) => format!(
                        "window reached the expected rectangle {} ms after Planor's EXPECT line (animation {} ms), i.e. {:+} ms against the end of the animation ({limit_detail})",
                        arrival / 1000,
                        self.lag[i].animate_ms,
                        latency / 1000
                    ),
                    None => format!(
                        "window reached the expected rectangle after {} ms ({limit_detail})",
                        latency / 1000
                    ),
                },
                None if lost_during => "capture was lost or frames were dropped while this move was being measured; the arrival is not known".to_string(),
                None => format!(
                    "window was not measured at the expected rectangle within 1 s (best edge score {:.3})",
                    self.lag[i].max_score
                ),
            };
            self.extra.arrival_ms = self.lag[i].arrival_us.map(|a| a / 1000);
            self.extra.late_ms = self.lag[i].latency_us.map(|l| l / 1000);
            let target = self.lag[i].target;
            let event_us = self.lag[i].event_us;
            self.lag[i].reported = true;
            self.report(
                Finding {
                    kind: "lag",
                    verdict,
                    hwnd: None,
                    t_us: event_us,
                    frame: None,
                    expected: Some(target),
                    reported: None,
                    measured: None,
                    detail,
                    crop: None,
                },
                None,
            );
        }
    }

    fn finish_black(&mut self, end_us: i64) {
        if self.black.reported {
            return;
        }
        if let Some(start) = self.black.start_us {
            if end_us - start >= (self.cfg.black_flat_ms as i64) * 1000
                && active_expectations(self.exps, start).next().is_some()
            {
                self.black.reported = true;
                self.report(
                    Finding {
                        kind: "monitor_black",
                        verdict: Verdict::Fail,
                        hwnd: None,
                        t_us: start,
                        frame: None,
                        expected: None,
                        reported: None,
                        measured: None,
                        detail: format!(
                            "monitor was black for at least {} ms while windows should be visible",
                            self.cfg.black_flat_ms
                        ),
                        crop: None,
                    },
                    None,
                );
            }
        }
    }

    fn finish_frozen(&mut self, end_us: i64) {
        if self.frozen_reported {
            return;
        }
        if let Some(start) = self.frozen_start {
            if end_us - start >= (self.cfg.black_flat_ms as i64) * 1000 {
                let frozen = frozen_since(
                    self.activity,
                    start,
                    end_us,
                    (self.cfg.black_flat_ms as i64) * 1000,
                );
                if let Some(start) = frozen {
                    self.frozen_reported = true;
                    self.report(
                        Finding {
                            kind: "monitor_frozen",
                            verdict: Verdict::Fail,
                            hwnd: None,
                            t_us: start,
                            frame: None,
                            expected: None,
                            reported: None,
                            measured: None,
                            detail: format!(
                                "no pixel change for at least {} ms while input or windows were changing",
                                self.cfg.black_flat_ms
                            ),
                            crop: None,
                        },
                        None,
                    );
                }
            }
        }
    }
}

fn hex(c: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

fn push_job(jobs: &mut BTreeMap<usize, Vec<Job>>, times: &[i64], t_us: i64, job: Job) {
    let i = frame_index(times, t_us);
    if i < times.len() {
        jobs.entry(i).or_default().push(job);
    }
}

fn build_jobs(
    exps: &[Exp],
    os: &[OsSnapshot],
    times: &[i64],
    end_us: i64,
    cfg: &Config,
    monitor_rect: Rect,
) -> (BTreeMap<usize, Vec<Job>>, Vec<LagTrack>, Vec<Option<usize>>) {
    let mut jobs: BTreeMap<usize, Vec<Job>> = BTreeMap::new();
    let mut lag: Vec<LagTrack> = Vec::new();
    // The lag track that follows each expectation's arrival (None: it was already in place, nothing to wait for).
    let mut exp_track: Vec<Option<usize>> = vec![None; exps.len()];

    for (ei, e) in exps.iter().enumerate() {
        if e.settle_us >= end_us || e.hidden || e.kind == "parked" {
            continue;
        }
        // Three position samples after the animation has settled.
        let mut t = e.settle_us + 80_000;
        let mut n = 0;
        while t < e.end_us && n < 3 {
            push_job(&mut jobs, times, t, Job::Position { exp: ei, t_us: t });
            t += 400_000;
            n += 1;
        }
        // Interior sampling every 150 ms, capped, to catch black/flat for > 300 ms.
        let mut t = e.settle_us;
        // The startup scene may take up to STARTUP_LATENCY_MS to arrive; sample past that so the interior is
        // still looked at after the window is in place.
        let extra = if e.startup { STARTUP_WATCH_US } else { 0 };
        let limit = e.end_us.min(e.t_us + 2_500_000 + extra);
        let mut n = 0;
        while t < limit && n < 18 + (extra / 150_000) as usize {
            push_job(&mut jobs, times, t, Job::Interior { exp: ei, t_us: t });
            t += 150_000;
            n += 1;
        }
    }

    // Border colour checks: a global scan every 400 ms while anything is expected.
    let start = exps
        .first()
        .map(|e| e.settle_us)
        .unwrap_or_else(|| times.first().copied().unwrap_or(0));
    let mut t = start;
    let mut n = 0;
    while t < end_us && n < 240 {
        push_job(&mut jobs, times, t, Job::Border { t_us: t });
        t += 400_000;
        n += 1;
    }

    // Lag from EXPECT changes: the window should be at the new rect after its animation.
    let mut last: HashMap<i64, (Rect, i64)> = HashMap::new();
    let mut changes = 0;
    for (ei, e) in exps.iter().enumerate() {
        if e.hidden || e.kind == "parked" || e.kind == "os" || e.clamped {
            continue;
        }
        // A window that was not expected for a while (dragged by hand, then dropped back where it was)
        // has to arrive again, even at the same rectangle.
        let changed = last.get(&e.hwnd).is_none_or(|(r, until_us)| {
            *until_us < e.t_us || r.max_side_diff(e.rect) > cfg.position_tolerance_px
        });
        last.insert(e.hwnd, (e.rect, e.end_us));
        // Under the fingers the target moves every few milliseconds with no animation: there is nothing to
        // arrive at. The scene that ends the gesture (not live) is tracked against the last live rectangle.
        if e.live {
            continue;
        }
        if !changed || e.rect.overlap_area(monitor_rect) == 0 {
            continue;
        }
        // A target that is mostly off this monitor (a strip scrolled out of view) cannot be measured
        // here; the position check reports it as unmeasurable.
        if e.rect.overlap_area(monitor_rect) * 2 < e.rect.area().max(1) {
            continue;
        }
        if changes >= 80 {
            break;
        }
        changes += 1;
        let event_us = e.t_us + e.animate_ms as i64 * 1000;
        let track = lag.len();
        let tr = LagTrack::new(e.rect, event_us)
            .with_expect(e.t_us, e.animate_ms)
            .with_startup(e.startup);
        let watch = tr.watch_us();
        lag.push(tr);
        exp_track[ei] = Some(track);
        // Watch from the EXPECT line on, so the arrival time is measured and not only the part of
        // it that falls after the animation should have ended.
        let start = e.t_us;
        let from = frame_index(times, start);
        let to = frame_index(times, start + watch);
        for i in from..to.min(times.len()) {
            jobs.entry(i).or_default().push(Job::Lag { track });
        }
    }

    // Lag from OS rect changes for windows Planor does not manage (no EXPECT for that window).
    let expected_hwnds: HashMap<i64, ()> = exps.iter().map(|e| (e.hwnd, ())).collect();
    let mut last_os: HashMap<i64, (Rect, i64)> = HashMap::new();
    let mut os_changes = 0;
    for snap in os {
        for w in &snap.windows {
            if expected_hwnds.contains_key(&w.hwnd) || w.cloaked || w.minimized {
                continue;
            }
            if w.rect.overlap_area(monitor_rect) == 0 {
                continue;
            }
            if let Some((prev, since)) = last_os.get(&w.hwnd).copied() {
                if prev.max_side_diff(w.rect) > cfg.position_tolerance_px
                    && snap.t_us - since >= 200_000
                {
                    if os_changes >= 40 {
                        break;
                    }
                    os_changes += 1;
                    let track = lag.len();
                    lag.push(LagTrack::new(w.rect, snap.t_us));
                    let from = frame_index(times, snap.t_us);
                    let to = frame_index(times, snap.t_us + 1_000_000);
                    for i in from..to.min(times.len()) {
                        jobs.entry(i).or_default().push(Job::Lag { track });
                    }
                    last_os.insert(w.hwnd, (w.rect, snap.t_us));
                } else if prev.max_side_diff(w.rect) > cfg.position_tolerance_px {
                    last_os.insert(w.hwnd, (w.rect, snap.t_us));
                }
            } else {
                last_os.insert(w.hwnd, (w.rect, snap.t_us));
            }
        }
    }

    (jobs, lag, exp_track)
}

fn decode_frames(
    path: &Path,
    w: i32,
    h: i32,
    mut on_frame: impl FnMut(&Image) -> bool,
) -> util::anyhow_lite::Result<usize> {
    let ffmpeg = util::find_ffmpeg().ok_or_else(|| util::fmt_err("FFmpeg not found"))?;
    let mut child = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-an", "-f", "rawvideo", "-pix_fmt", "bgra", "-"])
        .stdout(Stdio::piped())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| util::fmt_err(format!("starting FFmpeg: {e}")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| util::fmt_err("FFmpeg stdout"))?;
    let mut img = Image::new(w, h, vec![0u8; (w.max(1) * h.max(1) * 4) as usize]);
    let mut count = 0usize;
    let mut keep_going = true;
    loop {
        match stdout.read_exact(&mut img.bgra) {
            Ok(()) => {
                count += 1;
                if !on_frame(&img) {
                    keep_going = false;
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return util::anyhow_lite::err(format!("decoding video: {e}")),
        }
    }
    if !keep_going {
        let _ = child.kill();
    }
    let status = child.wait();
    if count == 0 {
        return util::anyhow_lite::err(format!(
            "FFmpeg decoded no frames from {} ({status:?})",
            path.display()
        ));
    }
    Ok(count)
}

fn analyze_monitor(
    rec: &Recording,
    monitor: &MonitorInfo,
    rows: Vec<FrameRow>,
    next_id: &mut u64,
) -> util::anyhow_lite::Result<(Vec<Check>, Vec<Suspect>, bool)> {
    let times: Vec<i64> = rows.iter().map(|r| r.present_us).collect();
    let lost = lost_stretches(&rows);
    let change = rect_change(&rec.events, monitor);
    let skip_from = change.map(|(t, _)| frame_index(&times, t));
    let start_us = times.first().copied().unwrap_or(rec.meta.qpc_start_us);
    let end_us = *times.last().unwrap_or(&rec.meta.qpc_start_us);
    let monitor_pos = rec
        .meta
        .monitors
        .iter()
        .position(|m| m.index == monitor.index)
        .unwrap_or(0);
    let mut exps = merge_expectations(
        &rec.trace.expectations,
        &rec.meta.monitors,
        start_us,
        end_us,
        &rec.config,
    );
    exps.retain(|e| monitor_index_of(&rec.meta.monitors, e.rect) == Some(monitor_pos));
    let has_planor_expectations = !exps.is_empty();
    let trace_has_expect = rec.trace.expectations.iter().any(|e| e.kind != "place");
    if use_os_fallback(trace_has_expect, has_planor_expectations) {
        let mut syn = synth_expectations_from_os(&rec.os, &rec.meta.monitors, end_us);
        syn.retain(|e| monitor_index_of(&rec.meta.monitors, e.rect) == Some(monitor_pos));
        exps = syn;
    }
    // Nothing is expected once the daemon has stopped: its windows go back where they were (or away) while the
    // recording is still running.
    if let Some(stop_us) = daemon_stop_us(&rec.marks) {
        end_expectations_at(&mut exps, stop_us);
    }

    let origin = (monitor.rect.x, monitor.rect.y);
    let work_local = Rect::new(
        monitor.work.x - origin.0,
        monitor.work.y - origin.1,
        monitor.work.w,
        monitor.work.h,
    );
    let taskbars_local: Vec<Rect> = rec
        .meta
        .taskbars
        .iter()
        .filter(|t| t.overlap_area(monitor.rect) > 0)
        .map(|t| Rect::new(t.x - origin.0, t.y - origin.1, t.w, t.h))
        .collect();
    let layout = image::LayoutContext::new(work_local, taskbars_local, rec.config.gap_px);
    let (jobs, lag, exp_track) =
        build_jobs(&exps, &rec.os, &times, end_us, &rec.config, monitor.rect);
    let activity = monitor_activity_times(monitor, &rec.events, &rec.os);
    let suspects_dir = rec.dir.join("suspects");
    util::ensure_dir(&suspects_dir)?;

    let mut runner = Runner {
        cfg: &rec.config,
        monitor,
        times: &times,
        exps: &exps,
        os: &rec.os,
        activity: &activity,
        notes: &rec.notes,
        marks: &rec.marks,
        taskbars: layout.taskbars.clone(),
        layout,
        base_us: rec.meta.qpc_start_us,
        origin,
        checks: Vec::new(),
        suspects: Vec::new(),
        suspects_dir,
        next_id,
        has_planor_expectations,
        saw_border: false,
        dedupe: HashMap::new(),
        interior_run: HashMap::new(),
        interior_reported: HashMap::new(),
        black: RunTracker::default(),
        prev_summary: None,
        prev_t: end_us,
        frozen_start: None,
        frozen_reported: false,
        lag,
        exp_track,
        deferred: Vec::new(),
        env: EnvTracker::default(),
        extra: Extra::default(),
        // Lost captures and dropped-frame gaps: no pixels to look at, so timing there is not a measurement.
        lost: {
            let period_us = 1_000_000 / rec.meta.fps.max(1) as i64;
            let mut blind = lost.clone();
            let gaps = gap_stretches(&times, period_us * 3);
            if !blind.is_empty() || !gaps.is_empty() {
                println!(
                    "planor-rec: monitor {}: {} lost-capture stretch(es), {} timeline gap(s) of missing frames (timing inside them is not measured)",
                    monitor.index,
                    blind.len(),
                    gaps.len()
                );
            }
            blind.extend(gaps);
            blind
        },
    };

    let video = rec.dir.join(format!("video-{}.mp4", monitor.index));
    let width = monitor.rect.w.max(1);
    let height = monitor.rect.h.max(1);
    let mut frame_no = 0usize;
    let decoded = decode_frames(&video, width, height, |img| {
        if frame_no >= times.len() {
            return false;
        }
        let t_us = times[frame_no];
        // No live capture (or a monitor that is no longer where `meta.json` says): the picture
        // proves nothing, so no check runs on it.
        if rows[frame_no].state == FrameState::Lost || skip_from.is_some_and(|s| frame_no >= s) {
            runner.on_lost_frame();
            frame_no += 1;
            return true;
        }
        runner.observe_frame(img, t_us, frame_no);
        if let Some(list) = jobs.get(&frame_no) {
            for job in list {
                match *job {
                    Job::Position { exp, t_us } => runner.position_job(img, exp, t_us, frame_no),
                    Job::Interior { exp, t_us } => runner.check_interior(img, exp, t_us, frame_no),
                    Job::Border { t_us } => runner.check_borders(img, t_us, frame_no),
                    Job::Lag { track } => {
                        runner.observe_lag(img, track, t_us, rows[frame_no].rotate_stall_us)
                    }
                }
            }
        }
        runner.run_deferred(img, t_us, frame_no);
        frame_no += 1;
        true
    })?;

    runner.finish_lag();
    runner.finish_black(end_us);
    runner.finish_frozen(end_us);
    if runner.env.not_clean() {
        // Another program's window shows where the wallpaper should be: position and border pixel checks on
        // this monitor are not trustworthy, so they are reported as unmeasurable with that reason.
        let (t, frame, fraction) = runner.env.first_dirty.unwrap_or((start_us, 0, 0.0));
        let detail = format!(
            "{} of {} settled frames on monitor {} show non-wallpaper content in {:.0}% or more of the free work area outside every expected tile (first at {:.0}%); another window is behind or beside the tiles, so position and border findings on this monitor are not judged. Run the scenario with the backdrop (PLANOR_RECORD=1 test guard) or clear the desktop.",
            runner.env.dirty,
            runner.env.frames,
            monitor.index,
            ENV_FOREIGN_FRACTION * 100.0,
            fraction * 100.0
        );
        let reason = format!("{}/{} frames", runner.env.dirty, runner.env.frames);
        let Runner {
            checks, suspects, ..
        } = &mut runner;
        let n = downgrade_for_environment(checks, suspects, monitor.index, &reason);
        println!(
            "planor-rec: monitor {}: environment_not_clean ({reason}); {n} position/border finding(s) downgraded to unmeasurable",
            monitor.index
        );
        runner.report(
            Finding {
                kind: "environment_not_clean",
                verdict: Verdict::Unmeasurable,
                hwnd: None,
                t_us: t,
                frame: Some(frame),
                expected: None,
                reported: None,
                measured: None,
                detail,
                crop: None,
            },
            None,
        );
    }

    for s in &lost {
        let reason = lost_reason(&rec.events, monitor.index, s.start_us)
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        runner.extra.duration_ms = Some(s.duration_ms());
        runner.report(
            Finding {
                kind: "capture_lost",
                verdict: Verdict::Fail,
                hwnd: None,
                t_us: s.start_us,
                frame: Some(s.first_frame),
                expected: None,
                reported: None,
                measured: None,
                detail: format!(
                    "desktop capture of monitor {} was lost for {} ms{reason}; {} timeline row(s) repeat the last good frame and were not checked. This is a capture/display event, not a Planor position or black-monitor measurement",
                    monitor.index,
                    s.duration_ms(),
                    s.frames
                ),
                crop: None,
            },
            None,
        );
    }
    if let Some((t, r)) = change {
        runner.report(
            Finding {
                kind: "topology_change",
                verdict: Verdict::Unmeasurable,
                hwnd: None,
                t_us: t,
                frame: Some(frame_index(&times, t)),
                expected: Some(monitor.rect),
                reported: None,
                measured: Some(r),
                detail: format!(
                    "the monitor's desktop rectangle changed from ({},{},{},{}) to ({},{},{},{}) during the recording; later frames were not compared with the old rectangle",
                    monitor.rect.x, monitor.rect.y, monitor.rect.w, monitor.rect.h, r.x, r.y, r.w, r.h
                ),
                crop: None,
            },
            None,
        );
    }
    let (dropped, first_drop) = dropped_frames(&rec.events, monitor.index);
    if dropped > 0 {
        runner.report(
            Finding {
                kind: "frames_dropped",
                verdict: Verdict::Unmeasurable,
                hwnd: None,
                t_us: first_drop.unwrap_or(start_us),
                frame: None,
                expected: None,
                reported: None,
                measured: None,
                detail: format!(
                    "the encoder fell behind and the recorder dropped {dropped} frame(s) of monitor {} rather than stall; those instants are not in the video",
                    monitor.index
                ),
                crop: None,
            },
            None,
        );
    }

    for (i, gap_us) in pacing_gaps(&times, rec.config.frame_gap_ms) {
        let why = slow_acquire_in(&rec.events, monitor.index, times[i - 1], times[i])
            .map(|w| format!("; {w}"))
            .unwrap_or_default();
        runner.report(
            Finding {
                kind: "pacing",
                verdict: Verdict::Fail,
                hwnd: None,
                t_us: times[i],
                frame: Some(i),
                expected: None,
                reported: None,
                measured: None,
                detail: format!(
                    "frame pacing gap of {} ms between encoded frames (threshold {} ms){why}",
                    gap_us / 1000,
                    rec.config.frame_gap_ms
                ),
                crop: None,
            },
            None,
        );
    }

    println!(
        "planor-rec: analyzed monitor {} - {} frames decoded of {} timeline rows, {} checks, {} suspects{}",
        monitor.index,
        decoded,
        times.len(),
        runner.checks.len(),
        runner.suspects.len(),
        if has_planor_expectations {
            ""
        } else {
            " (no EXPECT trace: OS rects used, border checks skipped)"
        }
    );
    Ok((runner.checks, runner.suspects, has_planor_expectations))
}

// ---- Report ------------------------------------------------------------------------------------

fn trim_event(e: &Value) -> Value {
    match e.get("kind").and_then(|k| k.as_str()) {
        Some("os") => json!({
            "t": e.get("t"),
            "kind": "os",
            "windows": e.get("windows").and_then(|w| w.as_array()).map(|a| a.len()).unwrap_or(0),
        }),
        _ => e.clone(),
    }
}

fn fill_nearby_events(suspects: &mut [Suspect], events: &[Value]) {
    for s in suspects.iter_mut() {
        let t = s.time_qpc_us;
        let from = events.partition_point(|e| e["t"].as_i64().unwrap_or(0) < t - 600_000);
        let mut nearby = Vec::new();
        for e in events.iter().skip(from) {
            let et = e["t"].as_i64().unwrap_or(0);
            if et > t + 600_000 || nearby.len() >= 20 {
                break;
            }
            nearby.push(trim_event(e));
        }
        s.nearby_events = nearby;
    }
}

fn rect_str(r: Option<Rect>) -> String {
    match r {
        Some(r) => format!("({},{},{},{})", r.x, r.y, r.w, r.h),
        None => "-".into(),
    }
}

fn write_markdown(path: &Path, report: &Report) -> std::io::Result<()> {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "# Planor recording report\n");
    let _ = writeln!(s, "- Recording: `{}`", report.recording.dir);
    let _ = writeln!(s, "- Started (UTC): {}", report.recording.started_utc);
    let _ = writeln!(s, "- FPS: {}", report.recording.fps);
    let _ = writeln!(
        s,
        "- Monitors: {}",
        report
            .recording
            .monitors
            .iter()
            .map(|m| format!(
                "#{} {} {}x{} at {},{}{}",
                m.index,
                m.device,
                m.rect.w,
                m.rect.h,
                m.rect.x,
                m.rect.y,
                if m.primary { " (primary)" } else { "" }
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let _ = writeln!(
        s,
        "- Planor trace: {} ({} lines, {} EXPECT/PLACE)",
        report.recording.trace.as_deref().unwrap_or("none"),
        report.recording.trace_lines,
        report.recording.trace_expectations
    );
    if let Some(c) = &report.recording.config {
        let _ = writeln!(s, "- Thresholds: `{c}`");
    }
    if let Some(reason) = &report.recording.stop_reason {
        let _ = writeln!(s, "- Stopped because: {reason}");
    }
    for h in report.recording.hotkeys.iter().filter(|h| !h.ok) {
        let _ = writeln!(s, "- Hotkey: {}", h.detail);
    }
    for c in &report.recording.capture {
        let _ = writeln!(
            s,
            "- Monitor {} capture: {} timeline ticks, {} frames encoded, {} dropped, {} lost ({} loss period(s), longest {} ms, {} reopen(s))",
            c.monitor,
            c.ticks,
            c.written,
            c.dropped,
            c.lost_ticks,
            c.lost_periods,
            c.max_lost_ms,
            c.reopens
        );
    }
    if let Some(a) = &report.recording.audio {
        let _ = writeln!(
            s,
            "- Audio `{}`: {} ms, {} Hz mono, **starts {:+} ms from the recording start** (audio start QPC {} µs). Video frames of a monitor begin later than the recording start, so line a note up with frames through the per-monitor offsets below.",
            a.file, a.duration_ms, a.sample_rate, a.offset_to_video_start_ms, a.start_qpc_us
        );
        for o in &report.recording.audio_offsets_to_first_frame_ms {
            let _ = writeln!(
                s,
                "  - monitor {}: audio starts {:+} ms from this monitor's first video frame (QPC {} µs); the video frame for audio time `a` ms is the first row of `frames-{}.csv` with `present_qpc_us >= {} + a*1000`",
                o.monitor, o.offset_ms, o.first_frame_qpc_us, o.monitor, a.start_qpc_us
            );
        }
    }
    let _ = writeln!(
        s,
        "\n**{} checks: {} fail, {} unmeasurable, {} pass. {} suspect image set(s).**\n",
        report.summary.checks,
        report.summary.fail,
        report.summary.unmeasurable,
        report.summary.pass,
        report.summary.suspects
    );
    if report.recording.trace_expectations == 0 {
        let _ = writeln!(
            s,
            "> No EXPECT/PLACE lines were found for this recording. OS-reported rectangles were used as\n\
             > expectations (labelled `position`, never claiming to know Planor's intent) and Planor border\n\
             > checks were skipped.\n"
        );
    } else if !report.recording.monitors_without_planor_tiles.is_empty() {
        let _ = writeln!(
            s,
            "> Planor has no tiles on monitor(s) {:?}; no position or border check was made there (Planor claims\n\
             > nothing for the windows on them). Black, frozen, pacing and capture checks still ran.\n",
            report.recording.monitors_without_planor_tiles
        );
    } else if !report.recording.os_fallback_monitors.is_empty() {
        let _ = writeln!(
            s,
            "> No EXPECT/PLACE rectangles were found on monitor(s) {:?}; OS-reported rectangles were used\n\
             > there and border checks were skipped for them.\n",
            report.recording.os_fallback_monitors
        );
    }

    for (title, want) in [
        ("Failures", Verdict::Fail),
        ("Unmeasurable", Verdict::Unmeasurable),
    ] {
        let list: Vec<&Check> = report
            .checks
            .iter()
            .filter(|c| c.verdict == want.as_str())
            .collect();
        if list.is_empty() {
            continue;
        }
        let _ = writeln!(s, "## {title}\n");
        for c in list {
            let _ = writeln!(
                s,
                "- **{}** `{}` at {} ms (monitor {}{}) — {}",
                c.verdict,
                c.kind,
                c.time_ms,
                c.monitor,
                c.hwnd.map(|h| format!(", hwnd {h}")).unwrap_or_default(),
                c.detail
            );
            if c.expected.is_some() || c.reported.is_some() || c.measured.is_some() {
                let _ = writeln!(
                    s,
                    "  - expected {} · os {} · measured {}",
                    rect_str(c.expected),
                    rect_str(c.reported),
                    rect_str(c.measured)
                );
            }
            if let Some(n) = &c.note {
                let _ = writeln!(s, "  - nearest note: {n}");
            }
            if let Some(img) = &c.suspect {
                let _ = writeln!(s, "  - image: `{img}`");
            }
        }
        let _ = writeln!(s);
    }
    let lags: Vec<&Check> = report
        .checks
        .iter()
        .filter(|c| c.kind == "lag" || c.kind == "startup_latency")
        .collect();
    if !lags.is_empty() {
        let _ = writeln!(s, "## Lag measurements\n");
        let _ = writeln!(
            s,
            "Arrival = time from Planor's EXPECT line to the first video frame showing the window at its target; late = arrival minus the configured animation (negative: early).\n"
        );
        let mut sorted = lags.clone();
        sorted.sort_by_key(|c| (c.monitor, c.time_qpc_us));
        for c in sorted {
            let _ = writeln!(
                s,
                "- monitor {} {} at {} ms: {} arrival {} ms, late {} ms — {}",
                c.monitor,
                rect_str(c.expected),
                c.time_ms,
                if c.kind == "startup_latency" {
                    "STARTUP (limit 1500 ms after EXPECT),"
                } else {
                    ""
                },
                c.arrival_ms
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into()),
                c.late_ms
                    .map(|v| format!("{v:+}"))
                    .unwrap_or_else(|| "-".into()),
                c.verdict
            );
        }
        let _ = writeln!(s);
    }
    let _ = writeln!(s, "## Passes\n");
    let _ = writeln!(
        s,
        "{} checks passed and are listed in `report.json`.",
        report.summary.pass
    );
    std::fs::write(path, s)
}

pub fn run(dir: &Path, trace_override: Option<&Path>) -> util::anyhow_lite::Result<()> {
    let dir = dir.to_path_buf();
    let dir = std::path::absolute(&dir).unwrap_or(dir);
    let meta = record::meta_from_dir(&dir)?;
    let (config, config_path) = Config::load(&dir);
    let trace_file = match trace_override {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => Some((path.to_path_buf(), text)),
            Err(e) => {
                // An isolated test daemon deletes its trace when it exits; the recorder kept its own copy
                // (`trace.log` in the recording) while it ran.
                match trace::load(&dir) {
                    Some(found) => {
                        eprintln!(
                            "planor-rec: --trace {}: {e}; using the recording's own copy {}",
                            path.display(),
                            found.0.display()
                        );
                        Some(found)
                    }
                    None => {
                        return util::anyhow_lite::err(format!("--trace {}: {e}", path.display()));
                    }
                }
            }
        },
        None => trace::load(&dir),
    };
    let trace = match &trace_file {
        Some((_, text)) => trace::parse(text, meta.qpc_start_us, meta.utc_start_unix_ms),
        None => Trace::default(),
    };
    let events_path = dir.join("events.jsonl");
    let events = if events_path.is_file() {
        load_events(&events_path)?
    } else {
        eprintln!("planor-rec: no events.jsonl; analyzing video only");
        Vec::new()
    };
    let os = extract_os(&events);
    let marks = extract_marks(&events);
    let notes = trace.notes.clone();
    println!(
        "planor-rec: {} events, {} OS snapshots, {} markers, {} trace expectations",
        events.len(),
        os.len(),
        marks.len(),
        trace.expectations.len()
    );
    if let Some((path, _)) = &trace_file {
        println!("planor-rec: Planor trace: {}", path.display());
    } else {
        println!("planor-rec: no Planor trace found; using OS rectangles as expectations");
    }
    if let Some(path) = &config_path {
        println!("planor-rec: thresholds: {}", path.display());
    }

    let rec = Recording {
        meta: meta.clone(),
        trace: trace.clone(),
        trace_path: trace_file.as_ref().map(|(p, _)| p.clone()),
        events,
        os,
        marks,
        notes,
        config,
        dir: dir.clone(),
    };

    let suspects_dir = dir.join("suspects");
    if suspects_dir.exists() {
        let _ = std::fs::remove_dir_all(&suspects_dir);
    }
    util::ensure_dir(&suspects_dir)?;

    let mut checks = Vec::new();
    let mut suspects = Vec::new();
    let mut os_fallback_monitors = Vec::new();
    let mut monitors_without_planor_tiles: Vec<u32> = Vec::new();
    let mut next_id = 1u64;
    let mut analyzed = 0usize;
    let mut first_frames: Vec<(u32, i64)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for monitor in &meta.monitors {
        let video = dir.join(format!("video-{}.mp4", monitor.index));
        let frames_path = dir.join(format!("frames-{}.csv", monitor.index));
        if !video.is_file() || !frames_path.is_file() {
            continue;
        }
        println!(
            "planor-rec: analyzing monitor {} ({}) ...",
            monitor.index,
            video.display()
        );
        // A monitor whose capture failed (no frames) is reported and skipped; the others are still analyzed.
        let rows = match load_frames(&frames_path) {
            Ok(rows) => rows,
            Err(e) => {
                println!("planor-rec: skipping monitor {}: {e}", monitor.index);
                skipped.push(format!("monitor {}: {e}", monitor.index));
                continue;
            }
        };
        first_frames.push((monitor.index, rows[0].present_us));
        let (mut c, mut s, has_expect) = analyze_monitor(&rec, monitor, rows, &mut next_id)?;
        checks.append(&mut c);
        suspects.append(&mut s);
        if !has_expect {
            if trace.expectations.iter().any(|e| e.kind != "place") {
                monitors_without_planor_tiles.push(monitor.index);
            } else {
                os_fallback_monitors.push(monitor.index);
            }
        }
        analyzed += 1;
    }
    if analyzed == 0 {
        return util::anyhow_lite::err(format!(
            "no video-<monitor>.mp4 + frames-<monitor>.csv pairs found in {}",
            dir.display()
        ));
    }

    fill_nearby_events(&mut suspects, &rec.events);
    checks.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.time_qpc_us.cmp(&b.time_qpc_us))
            .then_with(|| a.id.cmp(&b.id))
    });
    let fail = checks.iter().filter(|c| c.verdict == "fail").count();
    let unmeasurable = checks
        .iter()
        .filter(|c| c.verdict == "unmeasurable")
        .count();
    let pass = checks.iter().filter(|c| c.verdict == "pass").count();
    let summary = Summary {
        checks: checks.len(),
        fail,
        unmeasurable,
        pass,
        suspects: suspects.len(),
    };
    let report = Report {
        recording: RecordingInfo {
            dir: dir.display().to_string(),
            started_utc: meta.utc_start.clone(),
            fps: meta.fps,
            qpc_frequency: meta.qpc_frequency,
            monitors: meta.monitors.clone(),
            trace: rec.trace_path.as_ref().map(|p| p.display().to_string()),
            trace_lines: trace.lines,
            trace_expectations: trace.expectations.len(),
            config: config_path.as_ref().map(|p| p.display().to_string()),
            os_fallback_monitors,
            monitors_without_planor_tiles,
            analysis_version: env!("CARGO_PKG_VERSION").into(),
            audio: meta.audio.clone(),
            capture: meta.capture.clone(),
            hotkeys: meta.hotkeys.clone(),
            stop_reason: meta.stop_reason.clone(),
            audio_offsets_to_first_frame_ms: meta
                .audio
                .as_ref()
                .map(|a| audio_offsets(a, &first_frames))
                .unwrap_or_default(),
        },
        summary,
        checks,
        suspects,
    };
    util::write_json(&dir.join("report.json"), &report)?;
    write_markdown(&dir.join("report.md"), &report)
        .map_err(|e| util::fmt_err(format!("writing report.md: {e}")))?;
    util::write_json(&dir.join("suspects/suspects.json"), &report.suspects)?;
    println!(
        "planor-rec: report: {} checks ({} fail, {} unmeasurable, {} pass), {} suspects",
        report.summary.checks,
        report.summary.fail,
        report.summary.unmeasurable,
        report.summary.pass,
        report.summary.suspects
    );
    println!("planor-rec: wrote {}", dir.join("report.json").display());
    println!("planor-rec: wrote {}", dir.join("report.md").display());
    println!(
        "planor-rec: wrote {}",
        dir.join("suspects/suspects.json").display()
    );
    Ok(())
}

/// `planor-rec probe <dir> --monitor N --rect x,y,w,h --from-ms A --to-ms B`: print, frame by frame
/// (times in ms since the recording start), where the outline fit finds `rect` (desktop coordinates).
/// A diagnostic for checking the lag measurement against the pixels by hand.
pub fn probe(
    dir: &Path,
    monitor_index: u32,
    rect: Rect,
    from_ms: i64,
    to_ms: i64,
    scan_row: Option<i32>,
) -> util::anyhow_lite::Result<()> {
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let meta = record::meta_from_dir(&dir)?;
    let (config, _) = Config::load(&dir);
    let monitor = meta
        .monitors
        .iter()
        .find(|m| m.index == monitor_index)
        .ok_or_else(|| util::fmt_err(format!("monitor {monitor_index} is not in meta.json")))?
        .clone();
    let rows = load_frames(&dir.join(format!("frames-{monitor_index}.csv")))?;
    let origin = (monitor.rect.x, monitor.rect.y);
    let taskbars: Vec<Rect> = meta
        .taskbars
        .iter()
        .filter(|t| t.overlap_area(monitor.rect) > 0)
        .map(|t| Rect::new(t.x - origin.0, t.y - origin.1, t.w, t.h))
        .collect();
    let work = Rect::new(
        monitor.work.x - origin.0,
        monitor.work.y - origin.1,
        monitor.work.w,
        monitor.work.h,
    );
    let layout = image::LayoutContext::new(work, taskbars, config.gap_px);
    let local = Rect::new(rect.x - origin.0, rect.y - origin.1, rect.w, rect.h);
    let video = dir.join(format!("video-{monitor_index}.mp4"));
    let mut n = 0usize;
    println!("frame  t_ms  state     measured(desktop coords)           max_side_diff  score");
    decode_frames(&video, monitor.rect.w, monitor.rect.h, |img| {
        let Some(row) = rows.get(n) else {
            return false;
        };
        let t_ms = (row.present_us - meta.qpc_start_us) / 1000;
        if t_ms > to_ms {
            return false;
        }
        if t_ms >= from_ms {
            let fit = image::fit_outline(
                img,
                local,
                config.edge_search_px,
                config.min_edge_score,
                &layout,
            );
            let measured = fit
                .measured
                .map(|m| Rect::new(m.x + origin.0, m.y + origin.1, m.w, m.h));
            println!(
                "{n:5} {t_ms:5}  {:8}  {:34} {:>13}  {:.3}",
                row.state.as_str(),
                rect_str(measured),
                measured
                    .map(|m| m.max_side_diff(rect).to_string())
                    .unwrap_or_else(|| "-".into()),
                fit.score
            );
            if let Some(y) = scan_row {
                // Wallpaper / window runs along one row, in desktop x coordinates, long runs only.
                let runs: Vec<String> = image::class_runs_row(img, y - origin.1, &layout.taskbars)
                    .into_iter()
                    .filter(|(_, a, b)| b - a >= 3)
                    .map(|(c, a, b)| {
                        format!(
                            "{}[{},{})",
                            &format!("{c:?}")[..1],
                            a + origin.0,
                            b + origin.0
                        )
                    })
                    .collect();
                println!("        row {y}: {}", runs.join(" "));
            }
        }
        n += 1;
        true
    })?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn wall() -> Image {
        Image::solid(240, 180, [0, 0, 90])
    }

    #[test]
    fn position_verdict_never_passes_without_measurement() {
        let e = Rect::new(20, 20, 100, 80);
        let (v, d) = position_verdict(e, Some(e), None, 2, 0.01);
        assert_eq!(v, Verdict::Unmeasurable);
        assert!(d.contains("not measurable"), "{d}");
    }

    #[test]
    fn position_verdict_checks_every_pair() {
        let e = Rect::new(20, 20, 100, 80);
        assert_eq!(
            position_verdict(e, Some(e), Some(e), 2, 0.5).0,
            Verdict::Pass
        );
        let m = Rect::new(23, 20, 100, 80);
        assert_eq!(
            position_verdict(e, Some(e), Some(m), 2, 0.5).0,
            Verdict::Fail
        );
        let r = Rect::new(20, 25, 100, 80);
        assert_eq!(
            position_verdict(e, Some(r), Some(r), 2, 0.5).0,
            Verdict::Fail
        );
        // Measured but no OS rect: honest unmeasurable, not a pass.
        assert_eq!(
            position_verdict(e, None, Some(e), 2, 0.5).0,
            Verdict::Unmeasurable
        );
    }

    #[test]
    fn lag_finds_the_first_matching_frame() {
        let cfg = Config::default();
        let target = Rect::new(60, 40, 90, 70);
        let mut base = wall();
        let mut late = wall();
        late.fill(target, [220, 220, 220]);
        late.outline(target, BORDER_COLOURS[0].1, 2);
        let layout = image::LayoutContext::for_image(&base);
        let mut track = LagTrack::new(target, 1_000_000);
        // Three frames at the old position, then the window arrives.
        for i in 0..3 {
            track.observe(&base, (0, 0), 1_017_000 + i * 16_667, 0, &cfg, &layout);
        }
        assert_eq!(track.latency_us, None);
        let img = late.clone();
        track.observe(&img, (0, 0), 1_100_000, 0, &cfg, &layout);
        assert_eq!(track.latency_us, Some(100_000));
        assert!(track.done);
        // Any later frame cannot change the latency.
        base.fill(target, [200, 200, 200]);
        track.observe(&base, (0, 0), 1_500_000, 0, &cfg, &layout);
        assert_eq!(track.latency_us, Some(100_000));
    }

    #[test]
    fn lag_is_not_matched_by_a_fit_that_only_verified_the_flush_sides() {
        // Real case from a recording: the strip scrolls by 108 px. Before the move the target position is
        // inside another window, so its left and right sides cannot be measured; top and bottom are flush
        // with the frame edges and are. Those two alone must not count as arrival.
        let cfg = Config::default();
        let layout_img = Image::solid(320, 200, [0, 0, 90]);
        let layout = image::LayoutContext::for_image(&layout_img);
        let target = Rect::new(54, 0, 224, 200);
        let mut old = Image::solid(320, 200, [0, 0, 90]);
        old.fill(Rect::new(0, 0, 170, 200), [24, 24, 24]); // window A, scrolled left (starts off frame)
        old.fill(Rect::new(176, 0, 144, 200), [30, 30, 30]); // window B
        let mut new = Image::solid(320, 200, [0, 0, 90]);
        new.fill(Rect::new(54, 0, 224, 200), [24, 24, 24]); // window A at its target
        new.fill(Rect::new(284, 0, 36, 200), [30, 30, 30]);
        let fit_old = image::fit_outline(
            &old,
            target,
            cfg.edge_search_px,
            cfg.min_edge_score,
            &layout,
        );
        assert!(
            fit_old
                .measured
                .is_some_and(|m| m.max_side_diff(target) <= 2),
            "the old frame reproduces the false match: {fit_old:?}"
        );
        assert_eq!(fit_old.sides, [false, false, true, true]);
        let mut track = LagTrack::new(target, 1_180_000).with_expect(1_000_000, 180);
        track.observe(&old, (0, 0), 1_050_000, 0, &cfg, &layout);
        assert_eq!(track.latency_us, None, "not arrived yet");
        track.observe(&new, (0, 0), 1_187_000, 0, &cfg, &layout);
        assert_eq!(track.latency_us, Some(7_000));
        assert_eq!(track.arrival_us, Some(187_000));
    }

    #[test]
    fn lag_never_arriving_is_detectable_as_unmeasurable() {
        let cfg = Config::default();
        let target = Rect::new(60, 40, 90, 70);
        let wall = wall();
        let layout = image::LayoutContext::for_image(&wall);
        let mut track = LagTrack::new(target, 1_000_000);
        track.observe(&wall, (0, 0), 1_017_000, 0, &cfg, &layout);
        assert!(!track.done);
        assert!(track.max_score < cfg.min_edge_score);
    }

    #[test]
    fn run_tracker_fires_once_after_the_threshold() {
        let mut r = RunTracker::default();
        assert!(!r.observe(true, 0, 300));
        assert!(!r.observe(true, 200_000, 300));
        assert!(r.observe(true, 300_000, 300));
        assert!(!r.observe(true, 900_000, 300));
        // A change resets the run and allows a new report later.
        assert!(!r.observe(false, 1_000_000, 300));
        assert!(!r.observe(true, 1_100_000, 300));
        assert!(r.observe(true, 1_500_000, 300));
    }

    #[test]
    fn pacing_finds_gaps() {
        let times = [0, 16_667, 33_334, 250_000, 266_667];
        let gaps = pacing_gaps(&times, 100);
        assert_eq!(gaps, vec![(3, 216_666)]);
        assert!(pacing_gaps(&times, 300).is_empty());
    }

    #[test]
    fn frame_index_is_the_first_frame_at_or_after() {
        let times = [0, 10, 20, 30];
        assert_eq!(frame_index(&times, 0), 0);
        assert_eq!(frame_index(&times, 10), 1);
        assert_eq!(frame_index(&times, 15), 2);
        assert_eq!(frame_index(&times, 99), 4);
    }

    #[test]
    fn expectations_merge_identical_scenes_and_close_on_change() {
        let monitors = vec![MonitorInfo {
            index: 0,
            device: "DISPLAY1".into(),
            rect: Rect::new(0, 0, 1920, 1200),
            work: Rect::new(0, 0, 1920, 1160),
            dpi: 96,
            primary: true,
        }];
        let cfg = Config::default();
        let mk = |t, x| trace::Expectation {
            t_us: t,
            hwnd: 7,
            rect: Rect::new(x, 10, 100, 100),
            monitor: "m".into(),
            kind: "tile".into(),
            hidden: false,
            animate_ms: 180,
            startup: false,
            live: false,
            scene: 0,
        };
        let exps = merge_expectations(
            &[mk(1_000_000, 0), mk(1_100_000, 0), mk(1_200_000, 300)],
            &monitors,
            900_000,
            3_000_000,
            &cfg,
        );
        assert_eq!(exps.len(), 2);
        assert_eq!(exps[0].t_us, 1_000_000);
        assert_eq!(exps[0].end_us, 1_200_000);
        assert_eq!(exps[1].settle_us, 1_200_000 + 180_000 + 80_000);
    }

    fn mon0() -> Vec<MonitorInfo> {
        vec![MonitorInfo {
            index: 0,
            device: "DISPLAY1".into(),
            rect: Rect::new(0, 0, 1920, 1200),
            work: Rect::new(0, 0, 1920, 1160),
            dpi: 96,
            primary: true,
        }]
    }

    fn scene_exp(t: i64, hwnd: i64, x: i32, scene: u64) -> trace::Expectation {
        trace::Expectation {
            t_us: t,
            hwnd,
            rect: Rect::new(x, 10, 100, 100),
            monitor: "m".into(),
            kind: "tile".into(),
            hidden: false,
            animate_ms: 180,
            startup: false,
            live: false,
            scene,
        }
    }

    #[test]
    fn a_window_missing_from_the_next_scene_is_no_longer_expected_and_must_arrive_again() {
        // Scene 1 lists windows 1 and 2; scene 2 only window 2 (window 1 is being dragged); scene 3 both again,
        // window 1 back at the very same rectangle.
        let all = [
            scene_exp(1_000_000, 1, 0, 1),
            scene_exp(1_000_000, 2, 200, 1),
            scene_exp(2_000_000, 2, 200, 2),
            scene_exp(4_000_000, 1, 0, 3),
            scene_exp(4_000_000, 2, 200, 3),
        ];
        let exps = merge_expectations(&all, &mon0(), 900_000, 9_000_000, &Config::default());
        let w1: Vec<(i64, i64)> = exps
            .iter()
            .filter(|e| e.hwnd == 1)
            .map(|e| (e.t_us, e.end_us))
            .collect();
        assert_eq!(
            w1,
            vec![(1_000_000, 2_000_000), (4_000_000, 9_000_000)],
            "window 1 is expected only while a scene lists it"
        );
        let w2: Vec<(i64, i64)> = exps
            .iter()
            .filter(|e| e.hwnd == 2)
            .map(|e| (e.t_us, e.end_us))
            .collect();
        assert_eq!(
            w2,
            vec![(1_000_000, 9_000_000)],
            "window 2 stays in every scene: one expectation"
        );
        // Back in the same place after being away: a new arrival to wait for, not "already there".
        let times: Vec<i64> = (0..600).map(|i| 1_000_000 + i * 16_667).collect();
        let (_, _, exp_track) = build_jobs(
            &exps,
            &[],
            &times,
            9_000_000,
            &Config::default(),
            mon0()[0].rect,
        );
        let back = exps
            .iter()
            .position(|e| e.hwnd == 1 && e.t_us == 4_000_000)
            .unwrap();
        assert!(exp_track[back].is_some());
    }

    #[test]
    fn nothing_is_expected_after_the_daemon_stop_marker() {
        let marks = vec![
            (1_000_000, "scenario-start".to_string()),
            (5_000_000, "daemon-stopped".into()),
            (7_000_000, "daemon-stopped".into()),
        ];
        assert_eq!(
            daemon_stop_us(&marks),
            Some(5_000_000),
            "the first marker counts"
        );
        assert_eq!(daemon_stop_us(&marks[..1]), None);
        let mut exps = vec![
            exp_at(1, 1_000_000, 0, false),
            exp_at(2, 6_000_000, 200, false),
        ];
        end_expectations_at(&mut exps, 5_000_000);
        assert_eq!(
            exps.len(),
            1,
            "an expectation that begins after the stop is dropped"
        );
        assert_eq!(exps[0].end_us, 5_000_000);
    }

    #[test]
    fn black_and_frozen_monitors_are_detected() {
        // Monitor black: the summary mean is near zero.
        let black = image::summarize(&Image::black(320, 180));
        assert!(black.mean < 3.0);
        // Monitor frozen: identical frames plus activity for more than the threshold.
        let a = image::summarize(&Image::solid(320, 180, [0, 0, 90]));
        let b = image::summarize(&Image::solid(320, 180, [0, 0, 90]));
        assert!(image::is_static(&a, &b));
        let mut static_run = RunTracker::default();
        assert!(!static_run.observe(true, 0, 300));
        assert!(!static_run.observe(true, 250_000, 300));
        assert!(static_run.observe(true, 300_000, 300));
        // A single changed block ends the run.
        assert!(!static_run.observe(false, 330_000, 300));
        assert!(!static_run.observe(true, 400_000, 300));
        assert!(static_run.observe(true, 750_000, 300));
    }

    #[test]
    fn old_trace_lines_are_clamped_to_the_recording() {
        let monitors = vec![MonitorInfo {
            index: 0,
            device: "DISPLAY1".into(),
            rect: Rect::new(0, 0, 1920, 1200),
            work: Rect::new(0, 0, 1920, 1160),
            dpi: 96,
            primary: true,
        }];
        let cfg = Config::default();
        let mk = |t, x| trace::Expectation {
            t_us: t,
            hwnd: 7,
            rect: Rect::new(x, 10, 100, 100),
            monitor: "m".into(),
            kind: "tile".into(),
            hidden: false,
            animate_ms: 180,
            startup: false,
            live: false,
            scene: 0,
        };
        // Trace history from before the recording; only the layout live at the start survives.
        let exps = merge_expectations(
            &[mk(10_000, 0), mk(20_000, 300)],
            &monitors,
            1_000_000,
            3_000_000,
            &cfg,
        );
        assert_eq!(exps.len(), 1);
        assert_eq!(exps[0].rect.x, 300);
        assert!(exps[0].clamped);
        assert_eq!(exps[0].t_us, 1_000_000);
        assert_eq!(exps[0].settle_us, 1_000_000 + 80_000);
    }

    #[test]
    fn a_five_px_shifted_window_fails_the_position_verdict() {
        let cfg = Config::default();
        let mut img = Image::solid(320, 200, [0, 0, 90]);
        let actual = Rect::new(5, 0, 150, 200);
        let right = Rect::new(161, 0, 159, 200);
        img.fill(actual, [24, 24, 24]);
        img.fill(right, [30, 30, 30]);
        let expected = Rect::new(0, 0, 150, 200);
        let ctx = image::LayoutContext::for_image(&img);
        let fit = image::fit_outline(&img, expected, cfg.edge_search_px, cfg.min_edge_score, &ctx);
        assert_eq!(fit.measured, Some(actual), "{fit:?}");
        let (verdict, detail) = position_verdict(
            expected,
            Some(actual),
            fit.measured,
            cfg.position_tolerance_px,
            fit.score,
        );
        assert_eq!(verdict, Verdict::Fail, "{detail}");
    }

    fn row(t: i64, state: FrameState) -> FrameRow {
        FrameRow {
            present_us: t,
            state,
            rotate_stall_us: 0,
        }
    }

    #[test]
    fn frozen_needs_the_picture_to_stay_unchanged_after_the_activity_began() {
        let thr = 300_000;
        // Idle since 0, a window moves at 2.0 s: at 2.05 s it is too early to call it frozen ...
        assert_eq!(frozen_since(&[2_000_000], 0, 2_050_000, thr), None);
        // ... and only if the screen is still identical 300 ms after the activity is it a freeze,
        // reported at the time of the activity.
        assert_eq!(
            frozen_since(&[2_000_000], 0, 2_300_000, thr),
            Some(2_000_000)
        );
        // Activity before the static run began does not count.
        assert_eq!(frozen_since(&[100], 1_000_000, 9_000_000, thr), None);
        // No activity at all: an idle screen is not frozen.
        assert_eq!(frozen_since(&[], 0, 9_000_000, thr), None);
        // The first activity inside the run decides.
        assert_eq!(
            frozen_since(&[500_000, 900_000], 0, 900_000, thr),
            Some(500_000)
        );
    }

    #[test]
    fn os_rectangles_stand_in_only_when_there_is_no_planor_trace_at_all() {
        assert!(use_os_fallback(false, false));
        assert!(
            !use_os_fallback(true, false),
            "EXPECT exists for other monitors: nothing is claimed here"
        );
        assert!(!use_os_fallback(false, true));
        assert!(!use_os_fallback(true, true));
    }

    #[test]
    fn the_first_os_snapshot_is_a_baseline_not_window_activity() {
        let monitor = MonitorInfo {
            index: 0,
            device: "D".into(),
            rect: Rect::new(0, 0, 1920, 1200),
            work: Rect::new(0, 0, 1920, 1140),
            dpi: 96,
            primary: true,
        };
        let win = |x: i32| OsWindow {
            hwnd: 5,
            class: "App".into(),
            rect: Rect::new(x, 0, 500, 500),
            z: 0,
            cloaked: false,
            foreground: true,
            minimized: false,
        };
        let snaps = vec![
            OsSnapshot {
                t_us: 1_000,
                windows: vec![win(0)],
            },
            OsSnapshot {
                t_us: 2_000,
                windows: vec![win(0)],
            },
            OsSnapshot {
                t_us: 3_000,
                windows: vec![win(40)],
            },
        ];
        assert_eq!(monitor_activity_times(&monitor, &[], &snaps), vec![3_000]);
    }

    #[test]
    fn tiles_still_animating_are_not_settled() {
        let e = |settle| Exp {
            hwnd: 1,
            rect: Rect::new(0, 0, 10, 10),
            kind: "tile".into(),
            hidden: false,
            t_us: 1_000,
            animate_ms: 180,
            settle_us: settle,
            end_us: 10_000_000,
            clamped: false,
            startup: false,
            live: false,
        };
        let exps = vec![e(1_260_000), e(500)];
        assert!(!all_settled(&exps, 1_200_000));
        assert!(all_settled(&exps, 1_300_000));
        assert!(all_settled(&[], 5));
    }

    // ---- B: nothing is measured at a rectangle before the window has been placed there ----

    fn exp_at(hwnd: i64, t_us: i64, x: i32, startup: bool) -> Exp {
        Exp {
            hwnd,
            rect: Rect::new(x, 0, 100, 100),
            kind: "tile".into(),
            hidden: false,
            t_us,
            animate_ms: 180,
            settle_us: t_us + 260_000,
            end_us: 20_000_000,
            clamped: false,
            startup,
            live: false,
        }
    }

    #[test]
    fn a_tile_has_not_arrived_until_the_window_matched_or_the_watch_time_passed() {
        let cfg = Config::default();
        let target = Rect::new(60, 40, 90, 70);
        let base = wall();
        let mut placed = wall();
        placed.fill(target, [220, 220, 220]);
        placed.outline(target, BORDER_COLOURS[0].1, 2);
        let layout = image::LayoutContext::for_image(&base);
        let mut track = LagTrack::new(target, 1_180_000).with_expect(1_000_000, 180);
        // Not placed yet: the position/border checks must wait, however late the sample time is.
        track.observe(&base, (0, 0), 1_400_000, 0, &cfg, &layout);
        assert!(!track.arrival_resolved(1_400_000));
        assert!(!track.arrival_resolved(1_999_999));
        // The watch time (1 s after EXPECT) passing without a match also settles the question: the lag
        // finding reports it, and the position check may then say what it sees.
        assert!(track.arrival_resolved(2_000_000));
        // A match resolves it immediately.
        let mut track = LagTrack::new(target, 1_180_000).with_expect(1_000_000, 180);
        track.observe(&placed, (0, 0), 1_500_000, 0, &cfg, &layout);
        assert!(track.done);
        assert!(track.arrival_resolved(1_500_000));
    }

    #[test]
    fn every_changed_expectation_is_linked_to_its_lag_track_and_startup_waits_longer() {
        let cfg = Config::default();
        let times: Vec<i64> = (0..400).map(|i| 1_000_000 + i * 16_667).collect();
        let exps = vec![
            exp_at(1, 1_000_000, 0, true),    // first appearance, startup scene
            exp_at(2, 1_000_000, 200, false), // first appearance, ordinary
            exp_at(1, 4_000_000, 0, false), // same rect again for window 1 after a merge gap: unchanged
        ];
        let (jobs, lag, exp_track) = build_jobs(
            &exps,
            &[],
            &times,
            7_000_000,
            &cfg,
            Rect::new(0, 0, 1000, 1000),
        );
        assert_eq!(exp_track, vec![Some(0), Some(1), None]);
        assert!(lag[0].startup && !lag[1].startup);
        assert_eq!(lag[0].watch_end_us(), 1_000_000 + 2_000_000);
        assert_eq!(lag[1].watch_end_us(), 1_000_000 + 1_000_000);
        // Position samples are still scheduled; the runner defers them until the arrival resolved.
        assert!(
            jobs.values()
                .flatten()
                .any(|j| matches!(j, Job::Position { exp: 0, .. }))
        );
    }

    // ---- D: the first scene after a daemon start has its own latency check ----

    #[test]
    fn startup_latency_is_a_number_that_fails_only_above_1500_ms() {
        assert_eq!(startup_verdict(Some(0), false, true), Verdict::Pass);
        assert_eq!(
            startup_verdict(Some(740), false, true),
            Verdict::Pass,
            "the real 0.5 to 0.8 s startup passes"
        );
        assert_eq!(startup_verdict(Some(1500), false, true), Verdict::Pass);
        assert_eq!(startup_verdict(Some(1501), false, true), Verdict::Fail);
        assert_eq!(
            startup_verdict(Some(2400), true, true),
            Verdict::Unmeasurable,
            "frames missing: only an upper bound"
        );
        assert_eq!(
            startup_verdict(None, false, true),
            Verdict::Fail,
            "never arrived"
        );
        assert_eq!(
            startup_verdict(None, false, false),
            Verdict::Unmeasurable,
            "edge never visible"
        );
        // The strict threshold is untouched for every later scene: 61 ms late is a failure there.
        let cfg = Config::default();
        assert_eq!(cfg.lag_ms, 50);
    }

    // ---- C2: a desktop that is not clean is reported, and does not fail position/border ----

    #[test]
    fn foreign_windows_in_the_free_work_area_make_the_environment_dirty() {
        let tile = Rect::new(100, 60, 200, 120);
        let excluded = [tile.expand(24)];
        let area = Rect::new(0, 0, 640, 360);
        let mut clean = Image::solid(640, 360, [0, 0, 90]);
        clean.fill(tile, [200, 200, 200]);
        let f = image::foreign_content_fraction(&clean, area, &excluded, 8).unwrap();
        assert!(f < 0.01, "{f}");
        // A user's window (here: a big light block) behind the tiles and showing past them.
        let mut dirty = clean.clone();
        dirty.fill(Rect::new(320, 0, 320, 360), [230, 230, 235]);
        let f = image::foreign_content_fraction(&dirty, area, &excluded, 8).unwrap();
        assert!(f > 0.4, "{f}");
        // The tile itself (and the margin around it for shadow and border) never counts as foreign.
        let mut only_tile = Image::solid(640, 360, [0, 0, 90]);
        only_tile.fill(tile.expand(20), [10, 200, 10]);
        let f = image::foreign_content_fraction(&only_tile, area, &excluded, 8).unwrap();
        assert!(f < 0.01, "{f}");
        // Too small a free area says nothing.
        assert_eq!(
            image::foreign_content_fraction(&clean, Rect::new(0, 0, 16, 16), &[], 8),
            None
        );

        let mut env = EnvTracker::default();
        env.observe(10, 0, Some(0.0));
        assert!(!env.not_clean());
        env.observe(20, 1, Some(0.6));
        env.observe(30, 2, Some(0.7));
        assert!(env.not_clean());
        assert_eq!(env.first_dirty.map(|d| d.0), Some(20));
        // Frames that could not be measured are not counted either way.
        env.observe(40, 3, None);
        assert_eq!(env.frames, 3);
    }

    #[test]
    fn a_foreign_window_raised_over_a_tile_is_found_in_the_os_snapshot() {
        let win = |hwnd: i64, class: &str, z: i64, rect: Rect| OsWindow {
            hwnd,
            class: class.into(),
            rect,
            z,
            cloaked: false,
            foreground: false,
            minimized: false,
        };
        let tile = Rect::new(0, 0, 957, 1140);
        let tiles = [(10i64, tile), (11, Rect::new(963, 0, 957, 1140))];
        let tile_windows = [
            win(10, "Sentinel", 5, tile),
            win(11, "Sentinel", 6, Rect::new(963, 0, 957, 1140)),
        ];
        let with = |extra: OsWindow| {
            let mut v = vec![extra];
            v.extend(tile_windows.iter().cloned());
            v
        };
        // The developer's app, above the tiles and covering one of them.
        let app = win(99, "Chrome_WidgetWin_1", 0, Rect::new(100, 50, 1400, 1000));
        assert_eq!(
            occluding_foreign_window(&with(app.clone()), &tiles).as_deref(),
            Some("Chrome_WidgetWin_1")
        );
        // The same window below the tiles is hidden by them: fine.
        let below = OsWindow {
            z: 9,
            ..app.clone()
        };
        assert_eq!(occluding_foreign_window(&with(below), &tiles), None);
        // Planor's overlay, the taskbar, tiny, cloaked and minimised windows never count.
        for other in [
            OsWindow {
                class: "Planor.Composition".into(),
                ..app.clone()
            },
            OsWindow {
                class: "Shell_TrayWnd".into(),
                ..app.clone()
            },
            OsWindow {
                rect: Rect::new(100, 50, 60, 60),
                ..app.clone()
            },
            OsWindow {
                cloaked: true,
                ..app.clone()
            },
            OsWindow {
                minimized: true,
                ..app.clone()
            },
            OsWindow {
                rect: Rect::new(1900, 1100, 400, 400),
                ..app.clone()
            }, // barely touches a tile
        ] {
            assert_eq!(occluding_foreign_window(&with(other), &tiles), None);
        }
    }

    fn mk_check(id: u64, kind: &str, verdict: &str, monitor: u32) -> Check {
        Check {
            id,
            kind: kind.into(),
            verdict: verdict.into(),
            monitor,
            hwnd: None,
            frame_index: None,
            time_qpc_us: 0,
            time_ms: 0,
            expected: None,
            reported: None,
            measured: None,
            diff_expected_reported_px: None,
            diff_expected_measured_px: None,
            diff_reported_measured_px: None,
            detail: "9 px off".into(),
            note: None,
            suspect: None,
            duration_ms: None,
            arrival_ms: None,
            late_ms: None,
            severity: 2,
        }
    }

    #[test]
    fn a_dirty_monitor_downgrades_its_position_and_border_failures_only() {
        let mut checks = vec![
            mk_check(1, "position", "fail", 0),
            mk_check(2, "border_missing", "fail", 0),
            mk_check(3, "border", "fail", 0),
            mk_check(4, "position", "fail", 1), // another monitor: untouched
            mk_check(5, "lag", "fail", 0), // arrival is read from the same pixels: downgraded too
            mk_check(6, "position", "pass", 0), // a pass stays a pass
            mk_check(7, "pacing", "fail", 0),
        ];
        let mut suspects = vec![Suspect {
            check_id: 1,
            monitor: 0,
            frame_index: None,
            time_qpc_us: 0,
            time_ms: 0,
            kind: "position".into(),
            verdict: "fail".into(),
            detail: String::new(),
            note: None,
            expected: None,
            measured: None,
            crop: None,
            monitor_png: None,
            nearby_events: vec![],
        }];
        let n = downgrade_for_environment(&mut checks, &mut suspects, 0, "5/5 frames");
        assert_eq!(n, 4);
        let v: Vec<&str> = checks.iter().map(|c| c.verdict.as_str()).collect();
        assert_eq!(
            v,
            [
                "unmeasurable",
                "unmeasurable",
                "unmeasurable",
                "fail",
                "unmeasurable",
                "pass",
                "fail"
            ]
        );
        assert!(
            checks[0]
                .detail
                .starts_with("environment_not_clean (5/5 frames)"),
            "{}",
            checks[0].detail
        );
        assert!(checks[0].detail.contains("9 px off"));
        assert_eq!(checks[0].severity, 1);
        assert_eq!(suspects[0].verdict, "unmeasurable");
    }

    // ---- E: a pacing gap is attributed to the recorder when it stalled itself ----

    #[test]
    fn a_pacing_gap_is_explained_by_a_slow_capture_call_of_the_same_monitor() {
        let events = vec![
            json!({"kind":"capture","event":"slow_acquire","monitor":2,"t":1_000_000,"ms":70,
                   "acquire_next_frame_ms":2,"gpu_copy_map_ms":6,"cpu_copy_rotate_ms":62}),
            json!({"kind":"capture","event":"slow_acquire","monitor":1,"t":5_000_000,"ms":90}),
        ];
        let text =
            slow_acquire_in(&events, 2, 1_010_000, 1_090_000).expect("overlaps the 70 ms call");
        assert!(
            text.contains("70 ms") && text.contains("CPU copy/rotate 62 ms"),
            "{text}"
        );
        assert!(text.contains("recorder stall"));
        // Another monitor's call, or a gap that does not overlap, is not an explanation.
        assert_eq!(slow_acquire_in(&events, 0, 1_010_000, 1_090_000), None);
        assert_eq!(slow_acquire_in(&events, 2, 2_000_000, 2_100_000), None);
        assert_eq!(slow_acquire_in(&events, 2, 5_000_000, 5_090_000), None);
    }

    #[test]
    fn gaps_in_the_timeline_are_stretches_of_missing_frames() {
        let times = [0, 16_667, 33_334, 200_000, 216_667];
        let gaps = gap_stretches(&times, 50_000);
        assert_eq!(gaps.len(), 1);
        assert_eq!((gaps[0].start_us, gaps[0].end_us), (33_334, 200_000));
        assert!(gap_stretches(&times, 300_000).is_empty());
    }

    #[test]
    fn lost_stretches_are_found_with_start_duration_and_frame_count() {
        let p = 16_667;
        let mut rows = Vec::new();
        for i in 0..10 {
            rows.push(row(1_000_000 + i * p, FrameState::Ok));
        }
        for i in 10..40 {
            rows.push(row(1_000_000 + i * p, FrameState::Lost));
        }
        rows.push(row(1_000_000 + 40 * p, FrameState::Reopened));
        for i in 41..50 {
            rows.push(row(1_000_000 + i * p, FrameState::Ok));
        }
        let lost = lost_stretches(&rows);
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].first_frame, 10);
        assert_eq!(lost[0].frames, 30);
        assert_eq!(lost[0].start_us, 1_000_000 + 10 * p);
        assert_eq!(lost[0].duration_ms(), 500);
        // A loss that lasts until the recording ends still has a duration.
        let tail = vec![
            row(0, FrameState::Ok),
            row(16_000, FrameState::Lost),
            row(32_000, FrameState::Lost),
        ];
        let lost = lost_stretches(&tail);
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].frames, 2);
        assert_eq!(lost[0].duration_ms(), 32);
        assert!(lost_stretches(&[row(0, FrameState::Ok), row(1, FrameState::Reopened)]).is_empty());
    }

    #[test]
    fn a_rectangle_change_is_read_from_capture_events() {
        let monitor = MonitorInfo {
            index: 1,
            device: "\\\\.\\DISPLAY2".into(),
            rect: Rect::new(1920, 0, 2560, 1440),
            work: Rect::new(1920, 0, 2560, 1400),
            dpi: 96,
            primary: false,
        };
        let same = json!({"kind":"capture","event":"reopened","monitor":1,"t":5,"rect":[1920,0,2560,1440]});
        assert_eq!(rect_change(&[same], &monitor), None);
        let moved =
            json!({"kind":"capture","event":"reopened","monitor":1,"t":9,"rect":[0,0,2560,1440]});
        assert_eq!(
            rect_change(&[moved], &monitor),
            Some((9, Rect::new(0, 0, 2560, 1440)))
        );
        let other = json!({"kind":"capture","event":"reopened","monitor":0,"t":9,"rect":[0,0,1,1]});
        assert_eq!(rect_change(&[other], &monitor), None);
        let topo = json!({"kind":"capture","event":"topology","t":7,"monitors":[
            {"index":0,"device":"\\\\.\\DISPLAY1","rect":[0,0,1920,1200]},
            {"index":1,"device":"\\\\.\\DISPLAY2","rect":[1920,0,1920,1080]}]});
        assert_eq!(
            rect_change(&[topo], &monitor),
            Some((7, Rect::new(1920, 0, 1920, 1080)))
        );
        let ok = json!({"kind":"capture","event":"topology","t":7,"monitors":[
            {"index":1,"device":"\\\\.\\DISPLAY2","rect":[1920,0,2560,1440]}]});
        assert_eq!(rect_change(&[ok], &monitor), None);
    }

    #[test]
    fn audio_offsets_are_relative_to_each_monitors_first_frame() {
        let audio = AudioMeta {
            start_qpc_us: 10_000_000 + 1_603_000,
            ..AudioMeta::default()
        };
        let offsets = audio_offsets(&audio, &[(0, 10_000_000), (2, 10_050_000)]);
        assert_eq!(
            offsets[0],
            AudioOffset {
                monitor: 0,
                first_frame_qpc_us: 10_000_000,
                offset_ms: 1603
            }
        );
        assert_eq!(offsets[1].offset_ms, 1553);
        // Audio that began before the first frame has a negative offset.
        let early = AudioMeta {
            start_qpc_us: 9_900_000,
            ..AudioMeta::default()
        };
        assert_eq!(audio_offsets(&early, &[(0, 10_000_000)])[0].offset_ms, -100);
    }

    #[test]
    fn the_lost_reason_and_dropped_frames_come_from_capture_events() {
        let events = vec![
            json!({"kind":"capture","event":"lost","monitor":2,"t":1_000_000,"reason":"ACCESS_LOST"}),
            json!({"kind":"capture","event":"lost","monitor":1,"t":1_000_100,"reason":"other monitor"}),
            json!({"kind":"capture","event":"encoder_behind","monitor":2,"t":2_000_000,"dropped":7}),
            json!({"kind":"capture","event":"encoder_behind","monitor":2,"t":3_000_000,"dropped":3}),
        ];
        assert_eq!(
            lost_reason(&events, 2, 1_010_000).as_deref(),
            Some("ACCESS_LOST")
        );
        assert_eq!(lost_reason(&events, 2, 9_000_000), None);
        assert_eq!(dropped_frames(&events, 2), (10, Some(2_000_000)));
        assert_eq!(dropped_frames(&events, 0), (0, None));
    }

    #[test]
    fn os_fallback_ignores_the_desktop_and_taskbar() {
        let monitors = vec![MonitorInfo {
            index: 0,
            device: "DISPLAY1".into(),
            rect: Rect::new(0, 0, 1920, 1200),
            work: Rect::new(0, 0, 1920, 1160),
            dpi: 96,
            primary: true,
        }];
        let snap = OsSnapshot {
            t_us: 1_000_000,
            windows: vec![
                OsWindow {
                    hwnd: 1,
                    class: "Progman".into(),
                    rect: Rect::new(0, 0, 1920, 1200),
                    z: 0,
                    cloaked: false,
                    foreground: false,
                    minimized: false,
                },
                OsWindow {
                    hwnd: 2,
                    class: "Notepad".into(),
                    rect: Rect::new(100, 100, 500, 400),
                    z: 1,
                    cloaked: false,
                    foreground: true,
                    minimized: false,
                },
            ],
        };
        let exps = synth_expectations_from_os(&[snap], &monitors, 2_000_000);
        assert_eq!(exps.len(), 1);
        assert_eq!(exps[0].hwnd, 2);
        assert_eq!(exps[0].kind, "os");
    }
}

#[cfg(test)]
mod lag_slack_tests {
    use super::*;

    /// The 61 ms lag that failed the 50 ms threshold on the portrait monitor of run drag-to-other-landscape-monitor-15292:
    /// its arrival frame followed a 34 ms gap in the frame stream (no slow_acquire event is logged below 40 ms),
    /// so its timestamp carried about 34 ms of recorder work. Measured against 50 + 34 ms it passes.
    #[test]
    fn rotated_arrival_frame_is_judged_with_its_recorder_stall() {
        assert!(
            61_000 <= lag_limit_us(50, 34_000),
            "61 ms must pass when 34 ms of it is recorder stall"
        );
        assert!(
            61_000 > lag_limit_us(50, 0),
            "an unrotated monitor keeps the plain 50 ms threshold"
        );
    }

    #[test]
    fn stall_widens_the_limit_by_exactly_the_stall() {
        assert_eq!(lag_limit_us(50, 0), 50_000);
        assert_eq!(lag_limit_us(50, 34_000), 84_000);
        assert_eq!(
            lag_limit_us(50, -5),
            50_000,
            "a negative stall never tightens the threshold"
        );
        // A real lag beyond threshold + stall still fails.
        assert!(90_000 > lag_limit_us(50, 34_000));
    }

    #[test]
    fn frame_csv_reads_the_stall_column_and_older_layouts() {
        let rows = parse_frame_rows(
            "index,present_qpc_us,changed,state,rotate_stall_us\n0,1000,1,ok,34000\n1,2000,0,ok,0\n",
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].present_us, rows[0].rotate_stall_us),
            (1000, 34_000)
        );
        assert_eq!(rows[1].rotate_stall_us, 0);

        // Recordings made before the stall column existed: the stall is zero, nothing else changes.
        let old =
            parse_frame_rows("index,present_qpc_us,changed,state\n0,1000,1,ok\n1,2000,0,lost\n")
                .unwrap();
        assert_eq!(old[1].state, FrameState::Lost);
        assert!(old.iter().all(|r| r.rotate_stall_us == 0));
        let oldest = parse_frame_rows("index,present_qpc_us\n0,1000\n").unwrap();
        assert_eq!(
            (
                oldest[0].present_us,
                oldest[0].state,
                oldest[0].rotate_stall_us
            ),
            (1000, FrameState::Ok, 0)
        );
    }
}

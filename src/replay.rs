//! Deterministic replay of recorded touchpad input (`PAD` trace lines and `planor-rec` `events.jsonl`)
//! into Planor's gesture recognizer and `Strip` layout engine without hardware.

use crate::{Gesture, Rect, Strip, Swipe};

pub const REPLAY_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq)]
pub enum ReplayKind {
    Contacts(Vec<(f64, f64)>),
    RecordedScroll {
        axis: char,
        offset: i32,
        focus: usize,
        frac: f64,
    },
    Mark(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReplayEvent {
    pub t_us: i64,
    pub kind: ReplayKind,
}

#[derive(Clone, Debug)]
pub struct ReplayConfig {
    pub area: Rect,
    pub gap: i32,
    pub window_count: usize,
    pub grouped: usize,
    pub stack_peek: i32,
    pub deadzone: f64,
    pub step: f64,
    pub swipe_scroll_gain: f64,
    pub swipe_natural_direction: bool,
    pub swipe_inertia: bool,
    pub settle_after_end: bool,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            area: Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1152,
            },
            gap: 6,
            window_count: 5,
            grouped: 0,
            stack_peek: 28,
            deadzone: 12.0,
            step: 60.0,
            swipe_scroll_gain: 4.0,
            swipe_natural_direction: true,
            swipe_inertia: true,
            settle_after_end: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReplayStep {
    pub t_us: i64,
    pub swipe: String,
    pub offset: i32,
    pub focus: usize,
    pub frac: f64,
}

#[derive(Clone, Debug)]
pub struct ReplayResult {
    pub schema_version: u32,
    pub final_offset: i32,
    pub final_focus: usize,
    pub final_active_member: usize,
    pub final_frac: f64,
    pub drag_starts: usize,
    pub drag_moves: usize,
    pub drag_ends: usize,
    pub recorded_scrolls: Vec<(i64, char, i32, usize, f64)>,
    pub steps: Vec<ReplayStep>,
}

fn parse_stamp_us(stamp: &str) -> Option<i64> {
    let bytes = stamp.as_bytes();
    if bytes.len() < 12 || bytes[2] != b':' || bytes[5] != b':' || bytes[8] != b'.' {
        return None;
    }
    let h: i64 = stamp[0..2].parse().ok()?;
    let m: i64 = stamp[3..5].parse().ok()?;
    let s: i64 = stamp[6..8].parse().ok()?;
    let ms: i64 = stamp[9..12].parse().ok()?;
    Some((((h * 60 + m) * 60 + s) * 1000 + ms) * 1000)
}

fn kv<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|tok| tok.strip_prefix(key))
}

/// Parse `PAD` trace lines from `%LOCALAPPDATA%\planor\trace.log` (or golden `.pad.log` fixtures).
pub fn parse_pad_trace(text: &str) -> Vec<ReplayEvent> {
    let mut out = Vec::new();
    let mut prev_t: Option<i64> = None;
    let mut day_offset = 0i64;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((stamp, rest)) = line.split_once(' ') else {
            continue;
        };
        let Some(mut t_us) = parse_stamp_us(stamp) else {
            continue;
        };
        t_us += day_offset;
        if let Some(prev) = prev_t
            && t_us + 12 * 3_600_000_000 < prev
        {
            day_offset += 24 * 3_600_000_000;
            t_us += 24 * 3_600_000_000;
        }
        prev_t = Some(t_us);
        let Some(body) = rest.strip_prefix("PAD ") else {
            continue;
        };
        if let Some(after) = body.strip_prefix("contacts ") {
            let mut pts = Vec::new();
            if let Some(pts_str) = kv(after, "pts=")
                && !pts_str.is_empty()
                && pts_str != "-"
            {
                for pair in pts_str.split(';') {
                    if let Some((x, y)) = pair.split_once(',')
                        && let (Ok(px), Ok(py)) = (x.parse::<f64>(), y.parse::<f64>())
                    {
                        pts.push((px, py));
                    }
                }
            }
            out.push(ReplayEvent {
                t_us,
                kind: ReplayKind::Contacts(pts),
            });
        } else if let Some(after) = body.strip_prefix("scroll ") {
            let axis = kv(after, "axis=")
                .and_then(|s| s.chars().next())
                .unwrap_or('h');
            let offset = kv(after, "offset=")
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);
            let focus = kv(after, "focus=")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            let frac = kv(after, "frac=")
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            out.push(ReplayEvent {
                t_us,
                kind: ReplayKind::RecordedScroll {
                    axis,
                    offset,
                    focus,
                    frac,
                },
            });
        } else {
            out.push(ReplayEvent {
                t_us,
                kind: ReplayKind::Mark(body.to_string()),
            });
        }
    }
    out
}

/// Parse `planor-rec` `events.jsonl` streams containing recorded `contacts` or `pad` events.
pub fn parse_events_jsonl(text: &str) -> Vec<ReplayEvent> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let t_us = v.get("t").and_then(|t| t.as_i64()).unwrap_or(0);
        let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        match kind {
            "contacts" | "pad" => {
                let mut pts = Vec::new();
                if let Some(arr) = v.get("pts").and_then(|p| p.as_array()) {
                    for item in arr {
                        if let Some(pair) = item.as_array()
                            && pair.len() >= 2
                            && let (Some(x), Some(y)) = (pair[0].as_f64(), pair[1].as_f64())
                        {
                            pts.push((x, y));
                        }
                    }
                }
                out.push(ReplayEvent {
                    t_us,
                    kind: ReplayKind::Contacts(pts),
                });
            }
            "mark" => {
                if let Some(m) = v.get("text").and_then(|s| s.as_str()) {
                    out.push(ReplayEvent {
                        t_us,
                        kind: ReplayKind::Mark(m.to_string()),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

pub fn replay_gesture(events: &[ReplayEvent], cfg: &ReplayConfig) -> ReplayResult {
    let mut strip = Strip::new(cfg.area);
    strip.gap = cfg.gap;
    for id in 1..=cfg.window_count as isize {
        strip.insert(id, 1);
    }
    if cfg.grouped >= 2 && cfg.window_count >= cfg.grouped {
        for id in 2..=cfg.grouped as isize {
            strip.group(id, 1);
        }
    }
    strip.select(1);
    strip.reveal(0);

    let mut gesture = Gesture::default();
    let mut scroll_frac = 0.0f64;
    let mut samples: Vec<(i64, f64, bool)> = Vec::new();
    let mut drag_starts = 0usize;
    let mut drag_moves = 0usize;
    let mut drag_ends = 0usize;
    let mut recorded_scrolls = Vec::new();
    let mut steps = Vec::new();

    let apply_scroll = |strip: &mut Strip, scroll_frac: &mut f64, d: f64, vertical: bool| {
        let signed = if cfg.swipe_natural_direction { -d } else { d } * cfg.swipe_scroll_gain;
        if vertical {
            let depth = if strip.area.horizontal() {
                strip.area.h
            } else {
                strip.area.w
            } as f64;
            strip.roll(signed / (depth * 0.5));
        } else {
            *scroll_frac += signed;
            let whole = scroll_frac.trunc();
            *scroll_frac -= whole;
            strip.scroll(whole as i32);
            strip.focus_nearest_center();
        }
    };

    for ev in events {
        match &ev.kind {
            ReplayKind::RecordedScroll {
                axis,
                offset,
                focus,
                frac,
            } => {
                recorded_scrolls.push((ev.t_us, *axis, *offset, *focus, *frac));
            }
            ReplayKind::Mark(_) => {}
            ReplayKind::Contacts(pts) => {
                let swipe = gesture.sample(pts, cfg.deadzone, cfg.step);
                let label = format!("{swipe:?}");
                match swipe {
                    Swipe::Scroll(d) => {
                        samples.push((ev.t_us, d, false));
                        samples.retain(|s| ev.t_us - s.0 <= 150_000);
                        apply_scroll(&mut strip, &mut scroll_frac, d, false);
                    }
                    Swipe::ScrollV(d) => {
                        samples.push((ev.t_us, d, true));
                        samples.retain(|s| ev.t_us - s.0 <= 150_000);
                        apply_scroll(&mut strip, &mut scroll_frac, d, true);
                    }
                    Swipe::ScrollEnd => {
                        scroll_frac = 0.0;
                        let (mut sum, mut vertical) = (0.0f64, false);
                        for (t, d, v) in &samples {
                            if ev.t_us - *t <= 100_000 {
                                sum += *d;
                                vertical = *v;
                            }
                        }
                        samples.clear();
                        let mut speed = sum / 0.1;
                        if cfg.swipe_inertia && speed.abs() > 150.0 {
                            while speed.abs() >= 40.0 {
                                apply_scroll(&mut strip, &mut scroll_frac, speed * 0.032, vertical);
                                speed *= 0.9;
                            }
                        }
                        if cfg.settle_after_end {
                            strip.settle_roll();
                            strip.reveal(0);
                        }
                    }
                    Swipe::DragStart => drag_starts += 1,
                    Swipe::DragMove(_, _) => drag_moves += 1,
                    Swipe::DragEnd | Swipe::DragCancel => drag_ends += 1,
                    Swipe::None | Swipe::Step { .. } => {}
                }
                let frac = strip
                    .columns
                    .get(strip.focus)
                    .map(|c| c.frac)
                    .unwrap_or(0.0);
                steps.push(ReplayStep {
                    t_us: ev.t_us,
                    swipe: label,
                    offset: strip.offset,
                    focus: strip.focus,
                    frac,
                });
            }
        }
    }

    let (final_active_member, final_frac) = strip
        .columns
        .get(strip.focus)
        .map(|c| (c.active, c.frac))
        .unwrap_or((0, 0.0));

    ReplayResult {
        schema_version: REPLAY_SCHEMA_VERSION,
        final_offset: strip.offset,
        final_focus: strip.focus,
        final_active_member,
        final_frac,
        drag_starts,
        drag_moves,
        drag_ends,
        recorded_scrolls,
        steps,
    }
}

// ---- Splitting a recording into gestures and naming them (`planor gestures analyze`) ----

/// One touch gesture: from the first finger down to the last finger up.
#[derive(Clone, Debug)]
pub struct Segment {
    /// The guided step it was recorded in (the latest `mark`), or "".
    pub label: String,
    /// For a gesture made in the get-ready gap between two steps: the step before (it may be finishing that
    /// one rather than starting label).
    pub previous: String,
    pub start_us: i64,
    pub frames: Vec<(i64, Vec<(f64, f64)>)>,
}

/// Split recorded events into gestures. A lift is an empty frame or, in recordings that have none, a gap of
/// more than 50 ms between frames (frames arrive every ~7 ms while fingers touch; the owner's first full
/// recording had 5528 gaps under 10 ms, none between 40 and 80 ms, and 113 above 80 ms).
pub fn segments(events: &[ReplayEvent]) -> Vec<Segment> {
    const GAP_US: i64 = 50_000;
    let mut out: Vec<Segment> = vec![];
    let mut cur: Option<Segment> = None;
    let mut label = String::new();
    let mut last_t = i64::MIN;
    // Gestures started between "end step N" and "step N+1" (the guide's get-ready countdown, which already
    // shows the next instruction) belong to step N+1.
    let mut since_end: Option<(usize, i64, String)> = None;
    for e in events {
        match &e.kind {
            ReplayKind::Mark(m) if m.starts_with("end ") => {
                since_end = Some((
                    out.len() + usize::from(cur.is_some()),
                    e.t_us,
                    label.clone(),
                ));
            }
            ReplayKind::Mark(m) => {
                if let Some((from, at, prev)) = since_end.take() {
                    for s in out.iter_mut().skip(from).filter(|s| s.start_us >= at) {
                        s.label = m.clone();
                        s.previous = prev.clone();
                    }
                    if let Some(c) = cur.as_mut().filter(|c| c.start_us >= at) {
                        c.label = m.clone();
                        c.previous = prev.clone();
                    }
                }
                label = m.clone();
            }
            ReplayKind::Contacts(p) if p.is_empty() => {
                out.extend(cur.take());
            }
            ReplayKind::Contacts(p) => {
                if cur.is_some() && e.t_us - last_t > GAP_US {
                    out.extend(cur.take());
                }
                last_t = e.t_us;
                cur.get_or_insert_with(|| Segment {
                    label: label.clone(),
                    previous: String::new(),
                    start_us: e.t_us,
                    frames: vec![],
                })
                .frames
                .push((e.t_us, p.clone()));
            }
            _ => (),
        }
    }
    out.extend(cur);
    out
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Tap,
    /// One finger moving the cursor.
    Move,
    /// Two fingers moving together.
    Scroll,
    /// Two fingers moving apart (`out`) or together.
    Pinch {
        out: bool,
    },
    /// Three or more fingers moving one way.
    Swipe,
    /// Four or more fingers going round.
    Circle,
}

/// What a gesture was, worked out from its frames only.
#[derive(Clone, Debug)]
pub struct Identified {
    pub fingers: usize,
    pub kind: Kind,
    /// "left", "right", "up", "down", "up-left", ... ("" for taps, pinches, circles).
    pub direction: &'static str,
    pub dx: f64,
    pub dy: f64,
    pub path: f64,
    pub ms: f64,
    /// Fingers came off one at a time (at least two steps down before the end).
    pub staggered_lift: bool,
}

impl Identified {
    pub fn speed(&self) -> f64 {
        self.dx.hypot(self.dy) / (self.ms / 1000.).max(0.001)
    }
    pub fn name(&self) -> String {
        let f = self.fingers;
        let what = match &self.kind {
            Kind::Tap => "tap".to_string(),
            Kind::Move => format!("move {}", self.direction),
            Kind::Scroll => format!("scroll {}", self.direction),
            Kind::Pinch { out } => {
                if *out {
                    "pinch out".into()
                } else {
                    "pinch in".into()
                }
            }
            Kind::Swipe if f >= 4 => format!("drag {}", self.direction),
            Kind::Swipe => format!("swipe {}", self.direction),
            Kind::Circle => "circle".into(),
        };
        let pace = match &self.kind {
            Kind::Swipe | Kind::Scroll if self.ms > 1200. => " slow",
            Kind::Swipe if self.speed() > 900. && self.ms < 220. => " flick",
            _ => "",
        };
        let lift = if self.staggered_lift {
            ", lifted one by one"
        } else {
            ""
        };
        format!("{f}f {what}{pace}{lift}")
    }
}

fn direction(dx: f64, dy: f64) -> &'static str {
    let (ax, ay) = (dx.abs(), dy.abs());
    if ax >= 2. * ay {
        if dx < 0. { "left" } else { "right" }
    } else if ay >= 2. * ax {
        if dy < 0. { "up" } else { "down" }
    } else {
        match (dy < 0., dx < 0.) {
            (true, true) => "up-left",
            (true, false) => "up-right",
            (false, true) => "down-left",
            (false, false) => "down-right",
        }
    }
}

pub fn identify(seg: &Segment) -> Identified {
    let fingers = seg.frames.iter().map(|f| f.1.len()).max().unwrap_or(0);
    let centroid = |p: &Vec<(f64, f64)>| {
        let n = p.len().max(1) as f64;
        (
            p.iter().map(|q| q.0).sum::<f64>() / n,
            p.iter().map(|q| q.1).sum::<f64>() / n,
        )
    };
    let full: Vec<&(i64, Vec<(f64, f64)>)> =
        seg.frames.iter().filter(|f| f.1.len() == fingers).collect();
    let (mut path, mut prev) = (0., None);
    for f in &full {
        let c = centroid(&f.1);
        if let Some((x, y)) = prev {
            path += f64::hypot(c.0 - x, c.1 - y);
        }
        prev = Some(c);
    }
    let (a, b) = match (full.first(), full.last()) {
        (Some(a), Some(b)) => (centroid(&a.1), centroid(&b.1)),
        _ => ((0., 0.), (0., 0.)),
    };
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let first = seg.frames.first().map(|f| f.0).unwrap_or(0);
    let last = seg.frames.last().map(|f| f.0).unwrap_or(0);
    let ms = (last - first) as f64 / 1000.;
    // Finger count stepping down at the end (4 -> 3 -> 2 ...), at least two steps.
    let counts: Vec<usize> = seg.frames.iter().map(|f| f.1.len()).collect();
    let peak = counts.iter().rposition(|&c| c == fingers).unwrap_or(0);
    let steps_down = counts[peak..].windows(2).filter(|w| w[1] < w[0]).count();
    let staggered_lift = fingers >= 3 && steps_down >= 2;
    let net = dx.hypot(dy);
    let spread = |p: &Vec<(f64, f64)>| {
        if p.len() >= 2 {
            f64::hypot(p[0].0 - p[1].0, p[0].1 - p[1].1)
        } else {
            0.
        }
    };
    let (s0, s1) = match (full.first(), full.last()) {
        (Some(a), Some(b)) => (spread(&a.1), spread(&b.1)),
        _ => (0., 0.),
    };
    let pinch = fingers == 2 && (s1 - s0).abs() > 40. && (s1 - s0).abs() > net;
    let kind = if pinch {
        Kind::Pinch { out: s1 > s0 }
    } else if net < 15. && path < 40. {
        Kind::Tap
    } else if fingers >= 4 && path > 250. && path > 2.5 * net {
        Kind::Circle
    } else {
        match fingers {
            1 => Kind::Move,
            2 => Kind::Scroll,
            _ => Kind::Swipe,
        }
    };
    let direction = match kind {
        Kind::Move | Kind::Scroll | Kind::Swipe => direction(dx, dy),
        _ => "",
    };
    Identified {
        fingers,
        kind,
        direction,
        dx,
        dy,
        path,
        ms,
        staggered_lift,
    }
}

/// Does what was done fit the guided step's instruction? Steps name a finger count and what to do
/// ("THREE fingers: swipe LEFT", "TWO fingers: scroll UP and DOWN", "FOUR fingers: TAP", ...).
pub fn fits(step: &str, g: &Identified) -> bool {
    let s = step.to_ascii_uppercase();
    if s.contains("PRESS") {
        // Physical clicks: the finger pressing the pad shows as a short touch (the button lines prove the click).
        return g.fingers <= 2;
    }
    let want = [
        ("ONE", 1),
        ("TWO", 2),
        ("THREE", 3),
        ("FOUR", 4),
        ("FIVE", 5),
    ]
    .iter()
    .find(|(w, _)| s.contains(&format!("{w} FINGER")))
    .map(|w| w.1);
    if want.is_some_and(|w| w != g.fingers) {
        return false;
    }
    if want == Some(5) {
        return true;
    }
    if s.contains("TAP") {
        return g.kind == Kind::Tap;
    }
    if s.contains("PINCH") {
        return matches!(g.kind, Kind::Pinch { .. });
    }
    if s.contains("CIRCLE") {
        return g.kind == Kind::Circle;
    }
    if s.contains("MOVE THE CURSOR") {
        return g.kind == Kind::Move;
    }
    if s.contains("DIAGONAL") {
        return g.kind == Kind::Swipe && g.direction.contains('-');
    }
    if s.contains("ONE FINGER AT A TIME") {
        return g.kind != Kind::Tap && g.staggered_lift;
    }
    let dirs: Vec<String> = ["LEFT", "RIGHT", "UP", "DOWN"]
        .into_iter()
        .filter(|d| s.contains(d))
        .map(|d| d.to_ascii_lowercase())
        .collect();
    matches!(g.kind, Kind::Swipe | Kind::Scroll)
        && (dirs.is_empty() || dirs.iter().any(|d| g.direction.contains(d.as_str())))
}

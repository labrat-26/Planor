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
        if let Some(prev) = prev_t {
            if t_us + 12 * 3_600_000_000 < prev {
                day_offset += 24 * 3_600_000_000;
                t_us += 24 * 3_600_000_000;
            }
        }
        prev_t = Some(t_us);
        let Some(body) = rest.strip_prefix("PAD ") else {
            continue;
        };
        if let Some(after) = body.strip_prefix("contacts ") {
            let mut pts = Vec::new();
            if let Some(pts_str) = kv(after, "pts=") {
                if !pts_str.is_empty() && pts_str != "-" {
                    for pair in pts_str.split(';') {
                        if let Some((x, y)) = pair.split_once(',') {
                            if let (Ok(px), Ok(py)) = (x.parse::<f64>(), y.parse::<f64>()) {
                                pts.push((px, py));
                            }
                        }
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
                        if let Some(pair) = item.as_array() {
                            if pair.len() >= 2 {
                                if let (Some(x), Some(y)) =
                                    (pair[0].as_f64(), pair[1].as_f64())
                                {
                                    pts.push((x, y));
                                }
                            }
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
                    Swipe::DragEnd => drag_ends += 1,
                    Swipe::None | Swipe::Step { .. } => {}
                }
                let frac = strip.columns.get(strip.focus).map(|c| c.frac).unwrap_or(0.0);
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

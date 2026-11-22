//! Parse Planor's diagnostics trace (`%LOCALAPPDATA%\planor\trace.log`).
//!
//! Lines are `HH:MM:SS.mmm KIND ...` in UTC wall clock. `meta.json` carries the QPC frequency, the
//! QPC value and the UTC wall clock at recording start, which is enough to put every trace line on
//! the recorder's clock without any extra dependency.

use crate::util::Rect;

#[derive(Clone, Debug, PartialEq)]
pub struct Expectation {
    pub t_us: i64,
    pub hwnd: i64,
    pub rect: Rect,
    pub monitor: String,
    /// `tile`, `peek`, `parked` or `fullscreen` (or `place` / `os` for fallbacks).
    pub kind: String,
    pub hidden: bool,
    pub animate_ms: u64,
    /// The EXPECT belongs to the first scene after a daemon start (`SCENE seq=1`, the first `SCENE` line of
    /// the trace, or a `seq` that went back down). Startup builds the GPU overlay and window sources, which
    /// Planor accepts as slow; see the analyzer's `startup_latency` check.
    pub startup: bool,
    /// 1-based number of the `SCENE` line this EXPECT belongs to (0 for `PLACE` and unnumbered lines). A scene
    /// lists every tile Planor shows; a window missing from a later scene (it is being dragged, or was closed)
    /// is no longer expected anywhere.
    pub scene: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub t_us: i64,
    pub text: String,
}

#[derive(Default, Clone, Debug)]
pub struct Trace {
    pub expectations: Vec<Expectation>,
    pub notes: Vec<Note>,
    pub lines: usize,
}

fn parse_rect(s: &str) -> Option<Rect> {
    let mut it = s.split(',');
    Some(Rect::new(
        it.next()?.trim().parse().ok()?,
        it.next()?.trim().parse().ok()?,
        it.next()?.trim().parse().ok()?,
        it.next()?.trim().parse().ok()?,
    ))
}

fn parse_paren_rect(s: &str) -> Option<Rect> {
    let s = s.trim();
    let s = s.strip_prefix('(')?.strip_suffix(')')?;
    parse_rect(s)
}

fn between<'a>(s: &'a str, key: &str, next: &str) -> Option<&'a str> {
    let start = s.find(key)? + key.len();
    let rest = &s[start..];
    let end = rest.find(next).unwrap_or(rest.len());
    Some(rest[..end].trim())
}

/// Convert a trace time of day to QPC microseconds using the recording's start mapping.
fn trace_qpc_us(ms_of_day: i64, qpc_start_us: i64, utc_start_unix_ms: i64) -> i64 {
    let day = crate::util::utc_day_start_ms(utc_start_unix_ms);
    let mut wall = day + ms_of_day;
    // A recording that crosses UTC midnight: a time far before the start belongs to the next day.
    if wall < utc_start_unix_ms - 12 * 3_600_000 {
        wall += 24 * 3_600_000;
    }
    qpc_start_us + (wall - utc_start_unix_ms) * 1000
}

fn parse_stamp(stamp: &str) -> Option<i64> {
    // HH:MM:SS.mmm
    let bytes = stamp.as_bytes();
    if bytes.len() < 12 || bytes[2] != b':' || bytes[5] != b':' || bytes[8] != b'.' {
        return None;
    }
    let h: i64 = stamp[0..2].parse().ok()?;
    let m: i64 = stamp[3..5].parse().ok()?;
    let s: i64 = stamp[6..8].parse().ok()?;
    let ms: i64 = stamp[9..12].parse().ok()?;
    Some(((h * 60 + m) * 60 + s) * 1000 + ms)
}

/// Parse a trace. `qpc_start_us`/`utc_start_unix_ms` come from `meta.json`.
pub fn parse(text: &str, qpc_start_us: i64, utc_start_unix_ms: i64) -> Trace {
    let mut trace = Trace::default();
    // Indices of the EXPECT lines since the last SCENE line, and the previous SCENE's seq.
    let mut pending: Vec<usize> = Vec::new();
    let mut last_seq: Option<u64> = None;
    let mut scene_no = 0u64;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        trace.lines += 1;
        let Some((stamp, rest)) = line.split_once(' ') else {
            continue;
        };
        let Some(ms_of_day) = parse_stamp(stamp) else {
            continue;
        };
        let t_us = trace_qpc_us(ms_of_day, qpc_start_us, utc_start_unix_ms);
        let (kind, body) = rest.split_once(' ').unwrap_or((rest, ""));
        match kind {
            "EXPECT" => {
                let hwnd = body
                    .split_whitespace()
                    .find_map(|t| t.strip_prefix("id="))
                    .and_then(|v| v.parse().ok());
                let rect = body
                    .split_whitespace()
                    .find_map(|t| t.strip_prefix("rect="))
                    .and_then(parse_rect)
                    .or_else(|| between(body, "rect=", " monitor=").and_then(parse_rect));
                let hidden = body
                    .split_whitespace()
                    .find_map(|t| t.strip_prefix("hidden="))
                    .map(|v| v == "1" || v == "true")
                    .unwrap_or(false);
                let animate_ms = body
                    .split_whitespace()
                    .find_map(|t| t.strip_prefix("animate_ms="))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let monitor = between(body, "monitor=", " kind=")
                    .unwrap_or("")
                    .to_string();
                let tile_kind = between(body, "kind=", " hidden=")
                    .or_else(|| between(body, "kind=", " animate_ms="))
                    .unwrap_or("tile")
                    .to_string();
                if let (Some(hwnd), Some(rect)) = (hwnd, rect) {
                    trace.expectations.push(Expectation {
                        t_us,
                        hwnd,
                        rect,
                        monitor,
                        kind: tile_kind,
                        hidden,
                        animate_ms,
                        startup: false,
                        scene: 0,
                    });
                    pending.push(trace.expectations.len() - 1);
                }
            }
            "SCENE" => {
                // `SCENE seq=N ...` follows the EXPECT lines of that scene.
                let seq: Option<u64> = body
                    .split_whitespace()
                    .find_map(|t| t.strip_prefix("seq="))
                    .and_then(|v| v.parse().ok());
                let startup = match (seq, last_seq) {
                    (Some(_), None) => true,
                    (Some(s), Some(prev)) => s == 1 || s <= prev,
                    _ => false,
                };
                if let Some(s) = seq {
                    last_seq = Some(s);
                }
                scene_no += 1;
                for &i in &pending {
                    trace.expectations[i].scene = scene_no;
                    if startup {
                        trace.expectations[i].startup = true;
                    }
                }
                pending.clear();
            }
            "PLACE" => {
                // PLACE <hwnd> (old) -> (new): the new rectangle is the expected one.
                let mut words = body.split_whitespace();
                let hwnd = words.next().and_then(|v| v.parse::<i64>().ok());
                let target = body.split("->").nth(1).and_then(parse_paren_rect);
                if let (Some(hwnd), Some(rect)) = (hwnd, target) {
                    trace.expectations.push(Expectation {
                        t_us,
                        hwnd,
                        rect,
                        monitor: String::new(),
                        kind: "place".into(),
                        hidden: false,
                        animate_ms: 0,
                        startup: false,
                        scene: 0,
                    });
                }
            }
            "NOTE" | "MARK" => trace.notes.push(Note {
                t_us,
                text: body.to_string(),
            }),
            _ => {}
        }
    }
    trace.expectations.sort_by_key(|e| e.t_us);
    trace.notes.sort_by_key(|n| n.t_us);
    trace
}

/// The trace file for a recording: the copy the recorder made, else the live daemon trace.
pub fn load(dir: &std::path::Path) -> Option<(std::path::PathBuf, String)> {
    let mut candidates = vec![dir.join("trace.log"), dir.join("trace-start.log")];
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(std::path::PathBuf::from(local).join("planor/trace.log"));
    }
    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Some((path, text));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_expect_and_place_on_the_qpc_clock() {
        let utc = crate::util::parse_iso_utc("2026-11-22T05:30:43.000Z").unwrap();
        let qpc_start_us = 1_000_000;
        let text = "\
05:30:43.500 NOTE start typing
05:30:44.000 EXPECT id=4242 rect=10,20,300,200 monitor=\\.\\DISPLAY1 kind=tile hidden=0 animate_ms=180
05:30:45.000 PLACE 4242 (10,20,300,200) -> (400,20,300,200)
";
        let trace = parse(text, qpc_start_us, utc);
        assert_eq!(trace.lines, 3);
        assert_eq!(trace.notes.len(), 1);
        assert_eq!(trace.notes[0].text, "start typing");
        assert_eq!(trace.expectations.len(), 2);
        let e = &trace.expectations[0];
        assert_eq!(e.hwnd, 4242);
        assert_eq!(e.rect, Rect::new(10, 20, 300, 200));
        assert_eq!(e.kind, "tile");
        assert_eq!(e.animate_ms, 180);
        assert_eq!(e.t_us, qpc_start_us + 1_000_000);
        assert_eq!(trace.expectations[1].rect, Rect::new(400, 20, 300, 200));
        assert_eq!(trace.expectations[1].t_us, qpc_start_us + 2_000_000);
    }

    #[test]
    fn only_the_first_scene_after_a_daemon_start_is_marked_startup() {
        let utc = crate::util::parse_iso_utc("2026-11-22T05:30:43.000Z").unwrap();
        let line = |t: &str, id: i64, x: i32| {
            format!("{t} EXPECT id={id} rect={x},0,10,10 monitor=m kind=tile hidden=0 animate_ms=180\n")
        };
        let mut text = String::new();
        text += &line("05:30:44.000", 1, 0);
        text += "05:30:44.000 SCENE seq=1 tiles=1 holes=0\n";
        text += &line("05:30:44.030", 1, 0);
        text += "05:30:44.030 SCENE seq=2 tiles=1 holes=0\n";
        text += &line("05:30:45.000", 1, 50);
        text += "05:30:45.000 SCENE seq=3 tiles=1 holes=0\n";
        // The daemon restarted: seq goes back to 1.
        text += &line("05:30:50.000", 1, 70);
        text += "05:30:50.000 SCENE seq=1 tiles=1 holes=0\n";
        let trace = parse(&text, 0, utc);
        let flags: Vec<bool> = trace.expectations.iter().map(|e| e.startup).collect();
        assert_eq!(flags, vec![true, false, false, true]);
        let scenes: Vec<u64> = trace.expectations.iter().map(|e| e.scene).collect();
        assert_eq!(scenes, vec![1, 2, 3, 4], "each EXPECT carries the number of the SCENE line that follows it");
        // A trace that starts mid-session (first SCENE line seen is seq=40): that one is the first in the file.
        let text = format!("{}05:30:44.000 SCENE seq=40 tiles=1 holes=0\n{}05:30:45.000 SCENE seq=41 tiles=1 holes=0\n",
            line("05:30:44.000", 1, 0), line("05:30:45.000", 1, 9));
        let flags: Vec<bool> = parse(&text, 0, utc).expectations.iter().map(|e| e.startup).collect();
        assert_eq!(flags, vec![true, false]);
    }

    #[test]
    fn midnight_rollover_is_supported() {
        let utc = crate::util::parse_iso_utc("2026-11-22T23:59:59.000Z").unwrap();
        let text = "00:00:01.000 EXPECT id=1 rect=0,0,10,10 monitor=m kind=tile hidden=0 animate_ms=0\n";
        let trace = parse(text, 0, utc);
        assert_eq!(trace.expectations[0].t_us, 2_000_000);
    }
}

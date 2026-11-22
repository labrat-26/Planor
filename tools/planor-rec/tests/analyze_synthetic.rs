//! End-to-end analyzer test with a synthetic recording: a generated video, a Planor EXPECT trace,
//! an OS snapshot and a mark. No real screen is needed.
//!
//! This exercises the path that only exists once Planor emits EXPECT lines: trace-to-QPC mapping,
//! expected rectangles, border placement checks, position checks, suspects and the report files.

use planor_rec::{
    capture::MonitorInfo,
    image::Image,
    record::Meta,
    util::{self, Rect},
};
use serde_json::Value;
use std::{
    io::Write,
    process::{Command, Stdio},
};

const W: i32 = 320;
const H: i32 = 180;
const FPS: u32 = 60;
const FRAMES: usize = 12;

#[test]
fn analyzes_a_synthetic_expect_recording_end_to_end() {
    let Some(ffmpeg) = util::find_ffmpeg() else {
        eprintln!("skipping: FFmpeg not found");
        return;
    };
    let dir = std::env::temp_dir().join(format!("planor-rec-synth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let start_us = 5_000_000i64;
    let utc = util::parse_iso_utc("2026-11-22T05:00:00.000Z").unwrap();
    let window = Rect::new(40, 30, 120, 90);
    let period_us = 1_000_000 / FPS as i64;

    // ---- frames: wallpaper, a textured window with a Planor border, and a stray border over the
    // ---- taskbar (which the analyzer must flag).
    let mut base = Image::solid(W, H, [0, 0, 90]);
    base.fill(window, [200, 200, 200]);
    for y in window.y..window.bottom() {
        for x in window.x..window.right() {
            if ((x / 7) + (y / 5)) % 2 == 0 {
                base.fill(Rect::new(x, y, 1, 1), [45, 45, 45]);
            }
        }
    }
    base.outline(window, [0x3B, 0x82, 0xF6], 2);
    base.fill(Rect::new(60, 165, 40, 2), [0x3B, 0x82, 0xF6]);

    // ---- encode the video through FFmpeg (the same path the recorder uses).
    let mut child = Command::new(&ffmpeg)
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
            "320x180",
            "-framerate",
            "60",
            "-i",
            "pipe:0",
            "-an",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "16",
            "-pix_fmt",
            "yuv420p",
            "-r",
            "60",
        ])
        .arg(dir.join("video-0.mp4"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ffmpeg");
    {
        let mut stdin = child.stdin.take().unwrap();
        for _ in 0..FRAMES {
            stdin.write_all(&base.bgra).unwrap();
        }
    }
    let status = child.wait().unwrap();
    assert!(status.success(), "ffmpeg encode failed");

    // ---- frames csv
    let mut csv = String::from("index,present_qpc_us,changed\n");
    for i in 0..FRAMES {
        csv.push_str(&format!("{i},{},1\n", start_us + i as i64 * period_us));
    }
    std::fs::write(dir.join("frames-0.csv"), csv).unwrap();

    // ---- events: one OS snapshot with the window in the foreground, plus a mark.
    let os = serde_json::json!({
        "t": start_us,
        "kind": "os",
        "windows": [{
            "hwnd": 100,
            "class": "SyntheticWindow",
            "rect": {"x": 40, "y": 30, "w": 120, "h": 90},
            "z": 0,
            "cloaked": false,
            "foreground": true,
            "minimized": false
        }]
    });
    let mark = serde_json::json!({"t": start_us + 100_000, "kind": "mark", "text": "synthetic marker"});
    std::fs::write(
        dir.join("events.jsonl"),
        format!("{os}\n{mark}\n"),
    )
    .unwrap();

    // ---- Planor trace: one EXPECT for the window, on the same UTC wall clock.
    std::fs::write(
        dir.join("trace.log"),
        "05:00:00.000 NOTE synthetic note\n\
         05:00:00.000 EXPECT id=100 rect=40,30,120,90 monitor=m kind=tile hidden=0 animate_ms=0\n",
    )
    .unwrap();

    // ---- meta.json
    let meta = Meta {
        tool: "planor-rec".into(),
        version: "0.1.0".into(),
        qpc_frequency: 10_000_000,
        qpc_start: start_us * 10,
        qpc_start_us: start_us,
        utc_start: util::iso_utc(utc),
        utc_start_unix_ms: utc,
        fps: FPS,
        monitors: vec![MonitorInfo {
            index: 0,
            device: "SYNTH".into(),
            rect: Rect::new(0, 0, W, H),
            work: Rect::new(0, 0, W, 160),
            dpi: 96,
            primary: true,
        }],
        taskbars: vec![Rect::new(0, 160, W, 20)],
        mic: false,
        mic_device: None,
        qpc_end_us: Some(start_us + FRAMES as i64 * period_us),
        utc_end: Some(util::iso_utc(utc + 200)),
        frames: vec![FRAMES as u64],
        ..Meta::default()
    };
    util::write_json(&dir.join("meta.json"), &meta).unwrap();

    // ---- run the analyzer
    planor_rec::analyze::run(&dir, None).expect("analyze synthetic recording");

    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap()).unwrap();
    let checks = report["checks"].as_array().expect("checks array");
    let recording = &report["recording"];

    // The EXPECT path was used, not the OS fallback.
    assert_eq!(recording["trace_expectations"].as_u64(), Some(1));
    assert_eq!(
        recording["os_fallback_monitors"].as_array().map(|a| a.len()),
        Some(0),
        "EXPECT trace should be used for monitor 0"
    );

    // The stray Planor-coloured line over the taskbar is a failure.
    let border_fail = checks.iter().find(|c| {
        c["kind"] == "border"
            && c["verdict"] == "fail"
            && c["detail"].as_str().unwrap_or("").contains("taskbar")
    });
    assert!(border_fail.is_some(), "expected border-over-taskbar failure:\n{checks:#?}");

    // The window's expected, OS and measured rectangles agree.
    let position = checks
        .iter()
        .find(|c| c["kind"] == "position" && c["hwnd"].as_i64() == Some(100));
    let position = position.expect("position check for hwnd 100");
    assert_eq!(position["verdict"], "pass", "{position:#?}");

    // A suspect crop exists and is linked from the report.
    let suspects: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("suspects/suspects.json")).unwrap())
            .unwrap();
    let list = suspects.as_array().unwrap();
    assert!(!list.is_empty(), "expected at least one suspect");
    let crop = list
        .iter()
        .find_map(|s| s["crop"].as_str())
        .expect("suspect crop path");
    assert!(
        dir.join(crop.replace('/', std::path::MAIN_SEPARATOR_STR)).is_file(),
        "suspect crop {crop} should exist"
    );

    // The report mentions the nearest marker/note.
    let md = std::fs::read_to_string(dir.join("report.md")).unwrap();
    assert!(md.contains("synthetic marker") || md.contains("synthetic note"), "{md}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A monitor that drops out for half a second: the recorder repeats / blanks the picture and marks
/// those rows `lost`. The analyzer must report one `capture_lost` finding with its start, duration and
/// monitor, and must NOT call the stretch a black or frozen monitor, nor fail a position check.
#[test]
fn a_lost_capture_stretch_is_its_own_finding_and_not_black_or_frozen() {
    let Some(ffmpeg) = util::find_ffmpeg() else {
        eprintln!("skipping: FFmpeg not found");
        return;
    };
    let dir = std::env::temp_dir().join(format!("planor-rec-lost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let start_us = 5_000_000i64;
    let utc = util::parse_iso_utc("2026-11-22T05:00:00.000Z").unwrap();
    let window = Rect::new(40, 30, 120, 90);
    let period_us = 1_000_000 / FPS as i64;
    let total = 60usize;
    let (lost_from, lost_to) = (10usize, 40usize); // 30 rows = 500 ms

    let mut good = Image::solid(W, H, [0, 0, 90]);
    good.fill(window, [200, 200, 200]);
    for y in window.y..window.bottom() {
        for x in window.x..window.right() {
            if ((x / 7) + (y / 5)) % 2 == 0 {
                good.fill(Rect::new(x, y, 1, 1), [45, 45, 45]);
            }
        }
    }
    good.outline(window, [0x3B, 0x82, 0xF6], 2);
    let black = Image::black(W, H);

    let mut child = Command::new(&ffmpeg)
        .args([
            "-hide_banner", "-loglevel", "error", "-y", "-f", "rawvideo", "-pixel_format", "bgra",
            "-video_size", "320x180", "-framerate", "60", "-i", "pipe:0", "-an", "-c:v", "libx264",
            "-preset", "veryfast", "-crf", "16", "-pix_fmt", "yuv420p", "-r", "60",
        ])
        .arg(dir.join("video-0.mp4"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ffmpeg");
    {
        let mut stdin = child.stdin.take().unwrap();
        for i in 0..total {
            let img = if (lost_from..lost_to).contains(&i) { &black } else { &good };
            stdin.write_all(&img.bgra).unwrap();
        }
    }
    assert!(child.wait().unwrap().success());

    let mut csv = String::from("index,present_qpc_us,changed,state\n");
    for i in 0..total {
        let state = if (lost_from..lost_to).contains(&i) {
            "lost"
        } else if i == lost_to {
            "reopened"
        } else {
            "ok"
        };
        csv.push_str(&format!("{i},{},{},{state}\n", start_us + i as i64 * period_us, if state == "lost" { 0 } else { 1 }));
    }
    std::fs::write(dir.join("frames-0.csv"), csv).unwrap();

    let lost_t = start_us + lost_from as i64 * period_us;
    let os = serde_json::json!({
        "t": start_us, "kind": "os",
        "windows": [{"hwnd": 100, "class": "SyntheticWindow",
            "rect": {"x": 40, "y": 30, "w": 120, "h": 90}, "z": 0,
            "cloaked": false, "foreground": true, "minimized": false}]
    });
    let lost_event = serde_json::json!({"t": lost_t, "kind": "capture", "monitor": 0, "event": "lost",
        "reason": "DXGI_ERROR_ACCESS_LOST (0x887A0026)"});
    let reopened = serde_json::json!({"t": start_us + lost_to as i64 * period_us, "kind": "capture",
        "monitor": 0, "event": "reopened", "lost_ms": 500, "rect": [0, 0, W, H]});
    std::fs::write(dir.join("events.jsonl"), format!("{os}\n{lost_event}\n{reopened}\n")).unwrap();
    std::fs::write(
        dir.join("trace.log"),
        "05:00:00.000 EXPECT id=100 rect=40,30,120,90 monitor=m kind=tile hidden=0 animate_ms=0\n",
    )
    .unwrap();
    let meta = Meta {
        tool: "planor-rec".into(),
        version: "0.1.0".into(),
        qpc_frequency: 10_000_000,
        qpc_start: start_us * 10,
        qpc_start_us: start_us,
        utc_start: util::iso_utc(utc),
        utc_start_unix_ms: utc,
        fps: FPS,
        monitors: vec![MonitorInfo {
            index: 0,
            device: "SYNTH".into(),
            rect: Rect::new(0, 0, W, H),
            work: Rect::new(0, 0, W, 160),
            dpi: 96,
            primary: true,
        }],
        taskbars: vec![Rect::new(0, 160, W, 20)],
        qpc_end_us: Some(start_us + total as i64 * period_us),
        frames: vec![total as u64],
        ..Meta::default()
    };
    util::write_json(&dir.join("meta.json"), &meta).unwrap();

    planor_rec::analyze::run(&dir, None).expect("analyze");
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap()).unwrap();
    let checks = report["checks"].as_array().unwrap();

    let lost: Vec<&Value> = checks.iter().filter(|c| c["kind"] == "capture_lost").collect();
    assert_eq!(lost.len(), 1, "exactly one capture_lost finding: {checks:#?}");
    assert_eq!(lost[0]["verdict"], "fail");
    assert_eq!(lost[0]["monitor"], 0);
    assert_eq!(lost[0]["time_qpc_us"].as_i64(), Some(lost_t));
    let duration = lost[0]["duration_ms"].as_i64().unwrap();
    assert!((495..=505).contains(&duration), "duration {duration} ms");
    assert!(lost[0]["detail"].as_str().unwrap().contains("DXGI_ERROR_ACCESS_LOST"));

    for kind in ["monitor_black", "monitor_frozen", "interior"] {
        assert!(
            !checks.iter().any(|c| c["kind"] == kind && c["verdict"] == "fail"),
            "a lost stretch must not be reported as {kind}: {checks:#?}"
        );
    }
    assert!(
        !checks.iter().any(|c| c["kind"] == "position" && c["verdict"] == "fail"),
        "no position failure: {checks:#?}"
    );
    assert!(
        checks.iter().any(|c| c["kind"] == "position" && c["verdict"] == "pass"),
        "healthy frames are still measured: {checks:#?}"
    );
    let md = std::fs::read_to_string(dir.join("report.md")).unwrap();
    assert!(md.contains("capture_lost"), "{md}");
    let _ = std::fs::remove_dir_all(&dir);
}
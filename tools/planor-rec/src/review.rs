//! `planor-rec review-pack <dir>`: the small, image-only hand-off to a Haiku subagent.
//!
//! It copies the flagged suspect crops (never the video) into `<dir>\review\`, writes
//! `review\manifest.json` and `review\PROMPT.md`. It reads local files and writes local files; it has
//! no network code and never sends anything anywhere. Whoever runs the Haiku subagent does that
//! separately, on purpose.

use crate::{
    image::{self, Image},
    util,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Total size the review folder may reach; later items are left out beyond it.
pub const MAX_PACK_BYTES: u64 = 3 * 1024 * 1024;
const CROP_WIDTH: i32 = 800;
const OVERVIEW_WIDTH: i32 = 480;

/// Kinds that have no picture worth looking at (they are measured from the clock or the events).
const NO_IMAGE_KINDS: [&str; 5] = ["capture_lost", "frames_dropped", "topology_change", "pacing", "lag"];

pub const PROMPT: &str = "\
# Visual check of flagged Planor screen moments

You are a Haiku subagent. Look ONLY at the images listed in `manifest.json` (this folder). Do not read any
other file, do not run commands, do not browse, and do not send or upload anything.

For each item in `manifest.json`, look at its `image` (a crop around the window; `overview` is a small
picture of the whole monitor, for context only) and answer with exactly ONE line:

`<id>: <glitch|ok|cannot tell> - <what you see, at most 12 words>`

`glitch` is only one of these visible problems:

- a coloured border drawn over the taskbar,
- a window whose inside is blank or black,
- a half drawn window (parts missing, or old content left behind),
- a completely black monitor,
- windows overlapping where tiles should only touch at the gap.

Anything else is `ok`. If the picture is too small, too dark or too unclear to judge, answer `cannot tell`.
Do not guess. The numbers in the manifest are claims that are being checked; they are not evidence of what
is on screen. Write nothing except the lines.
";

fn scaled(img: &Image, width: i32) -> Image {
    img.scaled_to_width(width)
}

fn write_scaled(src: &Path, dst: &Path, width: i32) -> std::io::Result<u64> {
    let img = image::read_png(src)?;
    image::write_png(dst, &scaled(&img, width))?;
    Ok(std::fs::metadata(dst)?.len())
}

/// Trim an event to what a reviewer may need; mouse moves and OS snapshots are summarised.
fn brief_event(e: &Value) -> Option<Value> {
    match e.get("kind").and_then(|k| k.as_str()) {
        Some("input") if e.get("input").and_then(|i| i.as_str()) == Some("mouse") => None,
        Some("os") => None,
        _ => Some(e.clone()),
    }
}

/// One manifest item from a suspect, its check and the report. Pure, so it is unit-testable.
pub fn manifest_item(id: usize, suspect: &Value, check: Option<&Value>, image: &str, overview: Option<&str>) -> Value {
    let nearby: Vec<Value> = suspect
        .get("nearby_events")
        .and_then(|n| n.as_array())
        .map(|a| a.iter().filter_map(brief_event).take(8).collect())
        .unwrap_or_default();
    json!({
        "id": id,
        "image": image,
        "overview": overview,
        "kind": suspect.get("kind"),
        "verdict": suspect.get("verdict"),
        "monitor": suspect.get("monitor"),
        "time_ms": suspect.get("time_ms"),
        "time_qpc_us": suspect.get("time_qpc_us"),
        "detail": suspect.get("detail"),
        "expected": check.and_then(|c| c.get("expected")).or_else(|| suspect.get("expected")),
        "reported": check.and_then(|c| c.get("reported")),
        "measured": check.and_then(|c| c.get("measured")).or_else(|| suspect.get("measured")),
        "nearest_note": suspect.get("note"),
        "surrounding_events": nearby,
    })
}

/// Which suspects go into the pack: those with a crop, not of an image-less kind, failures first
/// (unmeasurable only on request), near-duplicates (same kind and monitor within 2 s) dropped, capped.
pub fn select_suspects(suspects: &[Value], max_images: usize, include_unmeasurable: bool) -> Vec<Value> {
    let mut candidates: Vec<&Value> = suspects
        .iter()
        .filter(|s| s.get("crop").and_then(|c| c.as_str()).is_some())
        .filter(|s| {
            let kind = s.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            !NO_IMAGE_KINDS.contains(&kind)
        })
        .filter(|s| {
            include_unmeasurable || s.get("verdict").and_then(|v| v.as_str()) == Some("fail")
        })
        .collect();
    candidates.sort_by_key(|s| {
        let fail = s.get("verdict").and_then(|v| v.as_str()) == Some("fail");
        (!fail, s.get("time_qpc_us").and_then(|t| t.as_i64()).unwrap_or(0))
    });
    let mut out: Vec<Value> = Vec::new();
    for s in candidates {
        let kind = s.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let monitor = s.get("monitor").and_then(|m| m.as_i64());
        let t = s.get("time_qpc_us").and_then(|t| t.as_i64()).unwrap_or(0);
        let duplicate = out.iter().any(|o| {
            o.get("kind").and_then(|k| k.as_str()) == Some(kind)
                && o.get("monitor").and_then(|m| m.as_i64()) == monitor
                && (o.get("time_qpc_us").and_then(|x| x.as_i64()).unwrap_or(0) - t).abs() < 2_000_000
        });
        if duplicate {
            continue;
        }
        out.push(s.clone());
        if out.len() >= max_images {
            break;
        }
    }
    out
}

pub fn run(dir: &Path, max_images: usize) -> util::anyhow_lite::Result<()> {
    run_with(dir, max_images, false)
}

pub fn run_with(dir: &Path, max_images: usize, include_unmeasurable: bool) -> util::anyhow_lite::Result<()> {
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let suspects_path = dir.join("suspects").join("suspects.json");
    let report_path = dir.join("report.json");
    let suspects: Vec<Value> = serde_json::from_str(
        &std::fs::read_to_string(&suspects_path).map_err(|e| {
            util::fmt_err(format!(
                "reading {}: {e} (run `planor-rec analyze {}` first)",
                suspects_path.display(),
                dir.display()
            ))
        })?,
    )
    .map_err(|e| util::fmt_err(format!("parsing {}: {e}", suspects_path.display())))?;
    let report: Value = std::fs::read_to_string(&report_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let checks: Vec<Value> = report
        .get("checks")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let review_dir: PathBuf = dir.join("review");
    if review_dir.exists() {
        std::fs::remove_dir_all(&review_dir)
            .map_err(|e| util::fmt_err(format!("clearing {}: {e}", review_dir.display())))?;
    }
    util::ensure_dir(&review_dir)?;

    let selected = select_suspects(&suspects, max_images, include_unmeasurable);
    let mut items = Vec::new();
    let mut bytes = 0u64;
    let mut skipped_for_size = 0usize;
    for (n, s) in selected.iter().enumerate() {
        let id = n + 1;
        if bytes >= MAX_PACK_BYTES {
            skipped_for_size += 1;
            continue;
        }
        let kind = s.get("kind").and_then(|k| k.as_str()).unwrap_or("finding");
        let monitor = s.get("monitor").and_then(|m| m.as_i64()).unwrap_or(0);
        let time_ms = s.get("time_ms").and_then(|t| t.as_i64()).unwrap_or(0);
        let crop_src = dir.join(s["crop"].as_str().unwrap_or("").replace('/', std::path::MAIN_SEPARATOR_STR));
        let crop_name = format!("{id:02}-{kind}-m{monitor}-{time_ms}ms.png");
        match write_scaled(&crop_src, &review_dir.join(&crop_name), CROP_WIDTH) {
            Ok(b) => bytes += b,
            Err(e) => {
                eprintln!("planor-rec: skipping {}: {e}", crop_src.display());
                continue;
            }
        }
        let mut overview_name = None;
        if let Some(rel) = s.get("monitor_png").and_then(|p| p.as_str()) {
            let src = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            let name = format!("{id:02}-overview-m{monitor}.png");
            if bytes < MAX_PACK_BYTES {
                if let Ok(b) = write_scaled(&src, &review_dir.join(&name), OVERVIEW_WIDTH) {
                    bytes += b;
                    overview_name = Some(name);
                }
            }
        }
        let check = s
            .get("check_id")
            .and_then(|c| c.as_u64())
            .and_then(|cid| checks.iter().find(|c| c.get("id").and_then(|i| i.as_u64()) == Some(cid)));
        items.push(manifest_item(id, s, check, &crop_name, overview_name.as_deref()));
    }

    let manifest = json!({
        "recording": dir.display().to_string(),
        "note": "Local files only. planor-rec never sends this anywhere; hand the folder to a Haiku subagent yourself.",
        "flagged_total": suspects.len(),
        "images_in_pack": items.len(),
        "skipped_duplicates_or_cap": selected.len().saturating_sub(items.len()) + skipped_for_size,
        "pack_bytes": bytes,
        "items": items,
    });
    util::write_json(&review_dir.join("manifest.json"), &manifest)?;
    std::fs::write(review_dir.join("PROMPT.md"), PROMPT)
        .map_err(|e| util::fmt_err(format!("writing PROMPT.md: {e}")))?;
    println!(
        "planor-rec: review pack: {} flagged item(s) -> {} ({:.2} MB, images only, nothing sent anywhere)",
        manifest["images_in_pack"],
        review_dir.display(),
        bytes as f64 / 1_048_576.0
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suspect(kind: &str, verdict: &str, monitor: i64, t: i64, crop: Option<&str>) -> Value {
        json!({
            "check_id": t, "kind": kind, "verdict": verdict, "monitor": monitor,
            "time_qpc_us": t, "time_ms": t / 1000, "detail": "d", "note": "NOTE x",
            "crop": crop, "monitor_png": crop.map(|_| "suspects/m.png"),
            "nearby_events": [
                {"kind": "input", "input": "mouse", "x": 1, "y": 2, "t": t},
                {"kind": "mark", "text": "look here", "t": t},
                {"kind": "os", "t": t, "windows": 4}
            ]
        })
    }

    #[test]
    fn selection_prefers_failures_skips_image_less_kinds_and_dedupes() {
        let list = vec![
            suspect("position", "unmeasurable", 0, 1_000_000, Some("suspects/a.png")),
            suspect("border", "fail", 0, 5_000_000, Some("suspects/b.png")),
            suspect("border", "fail", 0, 5_500_000, Some("suspects/c.png")), // duplicate of b
            suspect("capture_lost", "fail", 0, 6_000_000, Some("suspects/d.png")),
            suspect("interior", "fail", 1, 7_000_000, Some("suspects/e.png")),
            suspect("monitor_black", "fail", 1, 9_000_000, None), // no crop
        ];
        let sel = select_suspects(&list, 10, false);
        let kinds: Vec<&str> = sel.iter().map(|s| s["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["border", "interior"]);
        let with = select_suspects(&list, 10, true);
        assert_eq!(with.len(), 3);
        assert_eq!(with[2]["kind"], "position", "unmeasurable ones come last");
        assert_eq!(select_suspects(&list, 1, false).len(), 1, "capped");
    }

    #[test]
    fn manifest_item_carries_the_rects_note_and_trimmed_events() {
        let s = suspect("border", "fail", 2, 5_000_000, Some("suspects/b.png"));
        let check = json!({"id": 5_000_000, "expected": {"x":1,"y":2,"w":3,"h":4},
                           "reported": {"x":1,"y":2,"w":3,"h":5}, "measured": {"x":9,"y":2,"w":3,"h":4}});
        let item = manifest_item(3, &s, Some(&check), "03-border.png", Some("03-overview.png"));
        assert_eq!(item["id"], 3);
        assert_eq!(item["image"], "03-border.png");
        assert_eq!(item["overview"], "03-overview.png");
        assert_eq!(item["kind"], "border");
        assert_eq!(item["monitor"], 2);
        assert_eq!(item["expected"]["x"], 1);
        assert_eq!(item["reported"]["h"], 5);
        assert_eq!(item["measured"]["x"], 9);
        assert_eq!(item["nearest_note"], "NOTE x");
        let events = item["surrounding_events"].as_array().unwrap();
        assert_eq!(events.len(), 1, "mouse moves and os snapshots are left out: {events:?}");
        assert_eq!(events[0]["text"], "look here");
    }

    #[test]
    fn the_prompt_is_narrow_and_names_no_destination() {
        assert!(PROMPT.contains("ONE line"));
        assert!(PROMPT.contains("Do not guess"));
        assert!(PROMPT.contains("border drawn over the taskbar"));
        assert!(PROMPT.contains("black monitor"));
        assert!(PROMPT.contains("do not send or upload anything"));
    }

    #[test]
    fn a_pack_is_built_from_a_synthetic_analysis_folder_and_stays_small() {
        let dir = std::env::temp_dir().join(format!("planor-rec-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("suspects")).unwrap();
        // A big, noisy crop and overview, to prove downscaling.
        let mut crop = Image::solid(1200, 700, [0, 0, 90]);
        crop.fill(crate::util::Rect::new(100, 100, 600, 300), [200, 200, 200]);
        image::write_png(&dir.join("suspects/s1.png"), &crop).unwrap();
        image::write_png(&dir.join("suspects/s1-monitor.png"), &Image::solid(640, 360, [0, 0, 90])).unwrap();
        let suspects = json!([{
            "check_id": 1, "monitor": 0, "frame_index": 5, "time_qpc_us": 1_000, "time_ms": 1,
            "kind": "border", "verdict": "fail", "detail": "stray", "note": null,
            "expected": null, "measured": {"x": 1, "y": 1, "w": 5, "h": 5},
            "crop": "suspects/s1.png", "monitor_png": "suspects/s1-monitor.png", "nearby_events": []
        }]);
        std::fs::write(dir.join("suspects/suspects.json"), suspects.to_string()).unwrap();
        std::fs::write(dir.join("report.json"), json!({"checks": [{"id": 1, "expected": null}]}).to_string()).unwrap();
        // A video that must NOT be copied.
        std::fs::write(dir.join("video-0.mp4"), vec![0u8; 5000]).unwrap();

        run(&dir, 12).unwrap();
        let review = dir.join("review");
        let manifest: Value = serde_json::from_str(&std::fs::read_to_string(review.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["images_in_pack"], 1);
        let item = &manifest["items"][0];
        let img = image::read_png(&review.join(item["image"].as_str().unwrap())).unwrap();
        assert_eq!(img.w, CROP_WIDTH, "crop is downscaled");
        let ov = image::read_png(&review.join(item["overview"].as_str().unwrap())).unwrap();
        assert_eq!(ov.w, OVERVIEW_WIDTH, "overview is downscaled");
        assert!(review.join("PROMPT.md").is_file());
        let names: Vec<String> = std::fs::read_dir(&review).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert!(names.iter().all(|n| !n.ends_with(".mp4")), "{names:?}");
        assert!(manifest["pack_bytes"].as_u64().unwrap() < MAX_PACK_BYTES);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

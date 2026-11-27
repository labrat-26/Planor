//! `planor-rec prune <dir-of-recordings> --keep N`: delete the oldest recordings.
//!
//! Only direct sub-directories that contain a `meta.json` written by `planor-rec` are ever
//! considered recordings; nothing else under the folder is touched, and symlinks are skipped.

use crate::util;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Recording {
    pub path: PathBuf,
    /// Recording start (UTC unix ms from `meta.json`), else the folder's modified time.
    pub started_ms: i64,
    pub bytes: u64,
}

fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                total += dir_bytes(&e.path());
            } else if let Ok(m) = e.metadata() {
                total += m.len();
            }
        }
    }
    total
}

pub fn list_recordings(root: &Path) -> std::io::Result<Vec<Recording>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(root)? {
        let e = e?;
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let path = e.path();
        let meta_path = path.join("meta.json");
        let Ok(text) = std::fs::read_to_string(&meta_path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if value.get("tool").and_then(|t| t.as_str()) != Some("planor-rec") {
            continue;
        }
        let started_ms = value
            .get("utc_start_unix_ms")
            .and_then(|v| v.as_i64())
            .filter(|v| *v > 0)
            .or_else(|| {
                e.metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
            })
            .unwrap_or(0);
        out.push(Recording {
            bytes: dir_bytes(&path),
            path,
            started_ms,
        });
    }
    Ok(out)
}

/// Split into (kept, to_remove): the `keep` newest are kept.
pub fn plan(mut all: Vec<Recording>, keep: usize) -> (Vec<Recording>, Vec<Recording>) {
    all.sort_by(|a, b| {
        b.started_ms
            .cmp(&a.started_ms)
            .then_with(|| a.path.cmp(&b.path))
    });
    let remove = all.split_off(keep.min(all.len()));
    (all, remove)
}

pub fn run(root: &Path, keep: usize, dry_run: bool) -> util::anyhow_lite::Result<()> {
    if !root.is_dir() {
        return util::anyhow_lite::err(format!("{} is not a folder", root.display()));
    }
    let all = list_recordings(root)
        .map_err(|e| util::fmt_err(format!("reading {}: {e}", root.display())))?;
    let found = all.len();
    let (kept, remove) = plan(all, keep);
    let mut freed = 0u64;
    for r in &remove {
        println!(
            "planor-rec: {} {} ({:.1} MB)",
            if dry_run { "would delete" } else { "deleting" },
            r.path.display(),
            r.bytes as f64 / 1_048_576.0
        );
        if !dry_run {
            std::fs::remove_dir_all(&r.path)
                .map_err(|e| util::fmt_err(format!("deleting {}: {e}", r.path.display())))?;
        }
        freed += r.bytes;
    }
    println!(
        "planor-rec: {found} recording(s) found, {} kept, {} {} ({:.1} MB)",
        kept.len(),
        remove.len(),
        if dry_run {
            "would be deleted"
        } else {
            "deleted"
        },
        freed as f64 / 1_048_576.0
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(root: &Path, name: &str, started: i64, tool: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("meta.json"),
            format!("{{\"tool\":\"{tool}\",\"utc_start_unix_ms\":{started}}}"),
        )
        .unwrap();
        std::fs::write(d.join("video-0.mp4"), vec![0u8; 100]).unwrap();
    }

    #[test]
    fn prune_keeps_the_newest_and_never_touches_other_folders() {
        let root = std::env::temp_dir().join(format!("planor-rec-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        make(&root, "old", 1_000, "planor-rec");
        make(&root, "mid", 2_000, "planor-rec");
        make(&root, "new", 3_000, "planor-rec");
        // Not recordings: a folder without meta.json, one from another tool, and a plain file.
        std::fs::create_dir_all(root.join("photos")).unwrap();
        std::fs::write(root.join("photos/a.jpg"), b"x").unwrap();
        make(&root, "other-tool", 10, "something-else");
        std::fs::write(root.join("note.txt"), b"x").unwrap();

        run(&root, 2, true).unwrap();
        assert!(root.join("old").exists(), "dry run deletes nothing");

        run(&root, 2, false).unwrap();
        assert!(!root.join("old").exists());
        assert!(root.join("mid").exists());
        assert!(root.join("new").exists());
        assert!(root.join("photos/a.jpg").exists());
        assert!(root.join("other-tool/meta.json").exists());
        assert!(root.join("note.txt").exists());

        run(&root, 0, false).unwrap();
        assert!(!root.join("mid").exists() && !root.join("new").exists());
        assert!(root.join("photos/a.jpg").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_orders_by_start_time() {
        let r = |ms: i64| Recording {
            path: PathBuf::from(format!("r{ms}")),
            started_ms: ms,
            bytes: 0,
        };
        let (kept, remove) = plan(vec![r(5), r(1), r(9), r(3)], 2);
        assert_eq!(
            kept.iter().map(|r| r.started_ms).collect::<Vec<_>>(),
            vec![9, 5]
        );
        assert_eq!(
            remove.iter().map(|r| r.started_ms).collect::<Vec<_>>(),
            vec![3, 1]
        );
    }
}

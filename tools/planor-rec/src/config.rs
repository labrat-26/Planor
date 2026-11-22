//! Analyzer thresholds. Defaults live here; an optional `planor-rec.toml` overrides them.
//!
//! The file is looked for in the recording directory first, then the current directory.

use serde::{Deserialize, Serialize};
use std::path::Path;

fn d_tol() -> i32 {
    2
}
fn d_lag() -> i64 {
    50
}
fn d_black() -> u64 {
    300
}
fn d_pacing() -> u64 {
    100
}
fn d_search() -> i32 {
    24
}
fn d_edge() -> f32 {
    0.10
}
fn d_border() -> i32 {
    2
}
fn d_border_rgb() -> i32 {
    40
}
fn d_settle() -> u64 {
    80
}
fn d_gap() -> i32 {
    6
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Allowed difference between expected, Windows-reported and measured rectangle sides.
    pub position_tolerance_px: i32,
    /// Lag above this is a failure.
    pub lag_ms: i64,
    /// Interior black/flat or monitor black/frozen longer than this is a failure.
    pub black_flat_ms: u64,
    /// Present-time gap above this is a frame-pacing failure.
    pub frame_gap_ms: u64,
    /// How far the edge fit searches for the window outline.
    pub edge_search_px: i32,
    /// Minimum normalized edge contrast for a measurement to count (else `unmeasurable`).
    pub min_edge_score: f32,
    /// A Planor border must be this close to an expected tile outline.
    pub border_tolerance_px: i32,
    /// Per-channel tolerance when matching Planor border colours in lossy video.
    pub border_colour_tolerance: i32,
    /// Extra wait after the animation duration before a tile counts as settled.
    pub settle_margin_ms: u64,
    /// Planor's inter-tile gap (`layout.gap_px`), used to recognise a gap strip between tiles.
    pub gap_px: i32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            position_tolerance_px: d_tol(),
            lag_ms: d_lag(),
            black_flat_ms: d_black(),
            frame_gap_ms: d_pacing(),
            edge_search_px: d_search(),
            min_edge_score: d_edge(),
            border_tolerance_px: d_border(),
            border_colour_tolerance: d_border_rgb(),
            settle_margin_ms: d_settle(),
            gap_px: d_gap(),
        }
    }
}

impl Config {
    /// Load `<dir>/planor-rec.toml`, else `./planor-rec.toml`, else defaults.
    pub fn load(dir: &Path) -> (Self, Option<std::path::PathBuf>) {
        for candidate in [dir.join("planor-rec.toml"), Path::new("planor-rec.toml").to_path_buf()] {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                match toml::from_str::<Config>(&text) {
                    Ok(mut cfg) => {
                        cfg.validate();
                        return (cfg, Some(candidate));
                    }
                    Err(e) => {
                        eprintln!("planor-rec: ignoring {}: {e}", candidate.display());
                    }
                }
            }
        }
        (Config::default(), None)
    }

    fn validate(&mut self) {
        self.position_tolerance_px = self.position_tolerance_px.clamp(0, 200);
        self.lag_ms = self.lag_ms.clamp(0, 10_000);
        self.black_flat_ms = self.black_flat_ms.clamp(0, 60_000);
        self.frame_gap_ms = self.frame_gap_ms.clamp(1, 10_000);
        self.edge_search_px = self.edge_search_px.clamp(0, 200);
        self.min_edge_score = self.min_edge_score.clamp(0.0, 1.0);
        self.border_tolerance_px = self.border_tolerance_px.clamp(0, 50);
        self.border_colour_tolerance = self.border_colour_tolerance.clamp(0, 128);
        self.settle_margin_ms = self.settle_margin_ms.clamp(0, 5_000);
        self.gap_px = self.gap_px.clamp(0, 64);
    }
}

/// Planor's border palette (see `planor.example.toml`): focused, stacked, pinned, drop target, group hover.
pub const BORDER_COLOURS: [(&str, [u8; 3]); 5] = [
    ("focused", [0x3B, 0x82, 0xF6]),
    ("stacked", [0xA8, 0x55, 0xF7]),
    ("pinned", [0xF5, 0x9E, 0x0B]),
    ("drop_target", [0x10, 0xB9, 0x81]),
    ("group_hover", [0x93, 0xC5, 0xFD]),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_design() {
        let c = Config::default();
        assert_eq!(c.position_tolerance_px, 2);
        assert_eq!(c.lag_ms, 50);
        assert_eq!(c.black_flat_ms, 300);
        assert_eq!(c.frame_gap_ms, 100);
        assert_eq!(c.edge_search_px, 24);
        assert_eq!(c.gap_px, 6);
    }

    #[test]
    fn partial_file_keeps_defaults() {
        let c: Config = toml::from_str("lag_ms = 80\n").unwrap();
        assert_eq!(c.lag_ms, 80);
        assert_eq!(c.position_tolerance_px, 2);
    }
}

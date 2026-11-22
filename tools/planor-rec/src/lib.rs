//! Planor recorder and pixel-maths analyzer.
//!
//! The recorder captures every monitor with DXGI desktop duplication, encodes it through FFmpeg and
//! writes a single clocked event stream. The analyzer decodes the video and compares pixels with the
//! rectangles Planor claimed. It never calls an LLM: suspicious moments are exported as PNGs for a
//! separate subagent to read.

pub mod analyze;
pub mod capture;
pub mod config;
pub mod events;
pub mod image;
pub mod pipe;
pub mod prune;
pub mod record;
pub mod review;
pub mod timeline;
pub mod trace;
pub mod util;

pub use util::Rect;

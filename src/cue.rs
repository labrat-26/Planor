#![allow(dead_code)] // the gesture cue card was retired in favour of the mauve target border
//! Non-interactive, orientation-aware animated SVG cues for Milestone 4.8 (#112, #113, #115).
//!
//! One single source of truth (`SvgScene` + `SvgElem`) generates both valid SVG XML markup and
//! Direct2D vector primitives, so tests verify the exact geometry, orientation, animation phase,
//! modifier key label, theme/accent colors, DPI scaling, and PiP/target non-occlusion drawn on
//! screen. Cues never drive layout; gesture state is authoritative.

use planor::Rect;
use std::fmt::Write as _;
use std::time::Instant;

/// Animation cycle length in milliseconds for preview oscillation.
pub const CUE_CYCLE_MS: u64 = 1200;

/// Which input method owns the active gesture cue (#112, #113).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CueInput {
    /// Mouse drag: displays the setting-selected group modifier (`"Shift"`, `"Ctrl"`, `"Alt"`).
    /// Planor never synthesizes a keypress.
    Mouse { modifier: &'static str },
    /// Four-finger trackpad drag: displays directional swipe guidance; never instructs a modifier press.
    Trackpad,
}

/// Authoritative gesture cue state produced by the daemon (#112, #113, #115).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CueKind {
    /// Middle-range stationary dwell preview (1/4..3/4).
    /// `after` is the midpoint-selected side if released without activating group mode.
    StackEntryPreview { after: bool },
    /// Active / committed group mode (mouse modifier pressed or trackpad perpendicular swipe).
    /// `direction`: `-1` (up in landscape / left in portrait), `+1` (down / right), `0` (mouse modifier).
    StackEntryCommitted { direction: i8 },
    /// Quarter-span directional stack exit (#115).
    /// `trailing`: `false` (left in landscape / up in portrait), `true` (right / down).
    StackExit { trailing: bool },
}

/// Non-interactive animated SVG guidance cue placed on one monitor's overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GestureCue {
    pub monitor: String,
    pub monitor_area: Rect,
    pub held_rect: Rect,
    pub target_rect: Option<Rect>,
    pub horizontal: bool,
    pub input: CueInput,
    pub kind: CueKind,
    pub started_at: Instant,
}

/// Theme + accent palette used to build the SVG cue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CuePalette {
    pub dark: bool,
    pub card_bg: u32,
    pub card_border: u32,
    pub frame_stroke: u32,
    pub frame_fill: u32,
    pub ink: u32,
    pub accent: u32,
    pub key_bg: u32,
    pub key_border: u32,
}

impl CuePalette {
    pub fn from_system(dark: bool, accent_colorref: u32) -> Self {
        // `theme::accent_color()` returns Win32 COLORREF (0x00BBGGRR); convert to 0xRRGGBB for D2D/SVG.
        let r = accent_colorref & 0xFF;
        let g = (accent_colorref >> 8) & 0xFF;
        let b = (accent_colorref >> 16) & 0xFF;
        let rgb_accent = (r << 16) | (g << 8) | b;
        let accent = if rgb_accent == 0 {
            if dark { 0x60A5FA } else { 0x2563EB }
        } else {
            rgb_accent
        };
        if dark {
            Self {
                dark: true,
                card_bg: 0x1C2230,
                card_border: 0x3B475C,
                frame_stroke: 0xCBD5E1,
                frame_fill: 0x293347,
                ink: 0xF8FAFC,
                accent,
                key_bg: 0x273246,
                key_border: 0x64748B,
            }
        } else {
            Self {
                dark: false,
                card_bg: 0xF8FAFC,
                card_border: 0xCBD5E1,
                frame_stroke: 0x334155,
                frame_fill: 0xE2E8F0,
                ink: 0x0F172A,
                accent,
                key_bg: 0xFFFFFF,
                key_border: 0x94A3B8,
            }
        }
    }
}

/// A single SVG vector primitive shared between SVG XML generation and Direct2D rendering.
#[derive(Clone, Debug, PartialEq)]
pub enum SvgElem {
    RoundedRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        rx: f32,
        fill: Option<(u32, f32)>,
        stroke: Option<(u32, f32, f32)>,
    },
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        color: u32,
        alpha: f32,
        width: f32,
    },
    Polyline {
        points: Vec<(f32, f32)>,
        color: u32,
        alpha: f32,
        width: f32,
    },
}

/// Evaluated SVG cue frame in screen coordinates (local coordinates relative to `rect` top-left).
#[derive(Clone, Debug, PartialEq)]
pub struct SvgScene {
    pub rect: Rect,
    pub dpi: f32,
    pub horizontal: bool,
    pub input: CueInput,
    pub kind: CueKind,
    /// Animated sliding window displacement `(dx, dy)` in pixels for deterministic verification.
    pub slide_offset: (f32, f32),
    /// Primary arrow vector `(dx, dy)` indicating required or committed push/pull direction.
    pub arrow_vector: (f32, f32),
    /// Modifier press-key label shown on mouse cues (`None` for trackpad and stack exit).
    pub key_label: Option<&'static str>,
    /// True when the cue represents committed/active group mode or exit rather than preview.
    pub committed: bool,
    pub elements: Vec<SvgElem>,
}

fn overlap_area(a: Rect, b: Rect) -> i64 {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    if w > 0 && h > 0 {
        w as i64 * h as i64
    } else {
        0
    }
}

fn clamp_rect(r: Rect, bounds: Rect) -> Rect {
    // A transient zero-sized work area (topology change) must not make clamp() panic (min > max).
    let w = r.w.min(bounds.w.max(1)).max(1);
    let h = r.h.min(bounds.h.max(1)).max(1);
    let x = r.x.clamp(bounds.x, (bounds.x + bounds.w - w).max(bounds.x));
    let y = r.y.clamp(bounds.y, (bounds.y + bounds.h - h).max(bounds.y));
    Rect { x, y, w, h }
}

/// Compute the cue badge size in physical pixels scaled by `dpi`.
pub fn cue_size(cue: &GestureCue, dpi: f32) -> (i32, i32) {
    let dpi = dpi.clamp(0.5, 4.0);
    let has_keycap = matches!(
        (cue.input, cue.kind),
        (
            CueInput::Mouse { .. },
            CueKind::StackEntryPreview { .. } | CueKind::StackEntryCommitted { .. }
        )
    );
    let (lw, lh) = match (cue.horizontal, has_keycap) {
        (true, true) => (136.0, 112.0),
        (true, false) => (116.0, 96.0),
        (false, true) => (120.0, 132.0),
        (false, false) => (96.0, 116.0),
    };
    (
        (lw * dpi).round().max(32.0) as i32,
        (lh * dpi).round().max(32.0) as i32,
    )
}

/// Place the cue tracking `cue.held_rect` inside `cue.monitor_area` without obscuring
/// `cue.target_rect`'s centre or any unmanaged occluder / PiP rect in `holes` (#112).
pub fn place_cue_rect(cue: &GestureCue, holes: &[Rect], dpi: f32) -> Rect {
    let (w, h) = cue_size(cue, dpi);
    let area = cue.monitor_area;
    let margin = (14.0 * dpi.clamp(0.5, 4.0)).round() as i32;
    let held = cue.held_rect;
    let hcx = held.x + held.w / 2;
    let hcy = held.y + held.h / 2;

    let target_core = cue.target_rect.map(|t| {
        let inset_x = (t.w / 6).min(48);
        let inset_y = (t.h / 6).min(48);
        Rect {
            x: t.x + inset_x,
            y: t.y + inset_y,
            w: (t.w - 2 * inset_x).max(1),
            h: (t.h - 2 * inset_y).max(1),
        }
    });

    let raw_candidates = [
        Rect {
            x: hcx - w / 2,
            y: held.y - h - margin,
            w,
            h,
        },
        Rect {
            x: hcx - w / 2,
            y: held.y + margin,
            w,
            h,
        },
        Rect {
            x: hcx - w / 2,
            y: held.y + held.h + margin,
            w,
            h,
        },
        Rect {
            x: hcx - w / 2,
            y: held.y + held.h - h - margin,
            w,
            h,
        },
        Rect {
            x: held.x - w - margin,
            y: hcy - h / 2,
            w,
            h,
        },
        Rect {
            x: held.x + held.w + margin,
            y: hcy - h / 2,
            w,
            h,
        },
        Rect {
            x: held.x + margin,
            y: held.y + margin,
            w,
            h,
        },
        Rect {
            x: held.x + held.w - w - margin,
            y: held.y + margin,
            w,
            h,
        },
        Rect {
            x: held.x + margin,
            y: held.y + held.h - h - margin,
            w,
            h,
        },
        Rect {
            x: held.x + held.w - w - margin,
            y: held.y + held.h - h - margin,
            w,
            h,
        },
        Rect {
            x: area.x + margin,
            y: area.y + margin,
            w,
            h,
        },
        Rect {
            x: area.x + area.w - w - margin,
            y: area.y + margin,
            w,
            h,
        },
        Rect {
            x: area.x + margin,
            y: area.y + area.h - h - margin,
            w,
            h,
        },
        Rect {
            x: area.x + area.w - w - margin,
            y: area.y + area.h - h - margin,
            w,
            h,
        },
    ];

    let mut best = clamp_rect(raw_candidates[0], area);
    let mut best_score = i64::MAX;
    for raw in raw_candidates {
        let cand = clamp_rect(raw, area);
        let hole_ov: i64 = holes.iter().map(|&hl| overlap_area(cand, hl)).sum();
        let target_ov: i64 = target_core.map_or(0, |tc| overlap_area(cand, tc));
        let ccx = cand.x + cand.w / 2;
        let ccy = cand.y + cand.h / 2;
        let dist = ((ccx - hcx).abs() + (ccy - hcy).abs()) as i64;
        let score = hole_ov.saturating_mul(100_000) + target_ov.saturating_mul(10) + dist;
        if score < best_score {
            best_score = score;
            best = cand;
        }
    }
    best
}

/// Evaluate the animated SVG cue at `phase_ms` into concrete vector primitives.
pub fn evaluate_cue(
    cue: &GestureCue,
    phase_ms: u64,
    palette: CuePalette,
    holes: &[Rect],
    dpi: f32,
) -> SvgScene {
    let dpi = dpi.clamp(0.5, 4.0);
    let rect = place_cue_rect(cue, holes, dpi);
    let w = rect.w as f32;
    let h = rect.h as f32;
    let stroke = (2.0 * dpi).max(1.5);
    let thin = (1.5 * dpi).max(1.0);
    let card_r = 10.0 * dpi;

    let committed = matches!(
        cue.kind,
        CueKind::StackEntryCommitted { .. } | CueKind::StackExit { .. }
    );
    let key_label = match (cue.input, cue.kind) {
        (
            CueInput::Mouse { modifier },
            CueKind::StackEntryPreview { .. } | CueKind::StackEntryCommitted { .. },
        ) => Some(modifier),
        _ => None,
    };

    let t = (phase_ms % CUE_CYCLE_MS) as f32 / CUE_CYCLE_MS as f32;
    let wave = (t * std::f32::consts::TAU).sin();
    let pulse = 0.5 - 0.5 * (t * std::f32::consts::TAU).cos();

    let mut elements = Vec::with_capacity(28);
    let border_col = if committed {
        palette.accent
    } else {
        palette.card_border
    };
    let card_alpha = if palette.dark { 0.94 } else { 0.96 };
    elements.push(SvgElem::RoundedRect {
        x: 0.0,
        y: 0.0,
        w,
        h,
        rx: card_r,
        fill: Some((palette.card_bg, card_alpha)),
        stroke: Some((border_col, 0.95, if committed { stroke } else { thin })),
    });

    let keycap_reserve = if key_label.is_some() { 26.0 * dpi } else { 0.0 };
    let stage_h = (h - keycap_reserve).max(32.0 * dpi);
    let cx = w * 0.5;
    let cy = stage_h * 0.44;
    let amp = 8.0 * dpi;

    let (slide_offset, arrow_vector) = match cue.kind {
        CueKind::StackEntryPreview { .. } => {
            let win_w = (w * 0.42).max(20.0 * dpi);
            let win_h = (stage_h * 0.36).max(16.0 * dpi);
            let (dx, dy) = if cue.horizontal {
                (0.0, wave * amp)
            } else {
                (wave * amp, 0.0)
            };

            elements.push(SvgElem::RoundedRect {
                x: cx - win_w * 0.5,
                y: cy - win_h * 0.5,
                w: win_w,
                h: win_h,
                rx: 4.0 * dpi,
                fill: Some((palette.frame_fill, 0.85)),
                stroke: Some((palette.frame_stroke, 0.75, thin)),
            });

            let sx = cx - win_w * 0.5 + dx;
            let sy = cy - win_h * 0.5 + dy;
            elements.push(SvgElem::RoundedRect {
                x: sx,
                y: sy,
                w: win_w,
                h: win_h,
                rx: 4.0 * dpi,
                fill: Some((palette.accent, 0.24)),
                stroke: Some((palette.accent, 0.95, stroke)),
            });
            elements.push(SvgElem::Line {
                x1: sx + 3.0 * dpi,
                y1: sy + win_h * 0.28,
                x2: sx + win_w - 3.0 * dpi,
                y2: sy + win_h * 0.28,
                color: palette.accent,
                alpha: 0.85,
                width: thin,
            });

            let arrow_cy = stage_h * 0.82;
            let arrow_half = 11.0 * dpi;
            if cue.horizontal {
                push_double_arrow(
                    &mut elements,
                    cx,
                    arrow_cy,
                    0.0,
                    arrow_half,
                    palette.accent,
                    0.95,
                    stroke,
                );
                ((dx, dy), (0.0, 1.0))
            } else {
                push_double_arrow(
                    &mut elements,
                    cx,
                    arrow_cy,
                    arrow_half,
                    0.0,
                    palette.accent,
                    0.95,
                    stroke,
                );
                ((dx, dy), (1.0, 0.0))
            }
        }
        CueKind::StackEntryCommitted { direction } => {
            let win_w = (w * 0.44).max(20.0 * dpi);
            let win_h = (stage_h * 0.36).max(16.0 * dpi);
            let snap = 0.65 + 0.35 * pulse;
            let dir_f = if direction == 0 {
                1.0
            } else {
                direction as f32
            };
            let (dx, dy) = if cue.horizontal {
                (0.0, dir_f * amp * 0.65 * snap)
            } else {
                (dir_f * amp * 0.65 * snap, 0.0)
            };

            elements.push(SvgElem::RoundedRect {
                x: cx - win_w * 0.5 - dx * 0.35,
                y: cy - win_h * 0.5 - dy * 0.35,
                w: win_w,
                h: win_h,
                rx: 4.0 * dpi,
                fill: Some((palette.accent, 0.22)),
                stroke: Some((palette.accent, 0.80, thin)),
            });
            elements.push(SvgElem::RoundedRect {
                x: cx - win_w * 0.5 + dx * 0.45,
                y: cy - win_h * 0.5 + dy * 0.45,
                w: win_w,
                h: win_h,
                rx: 4.0 * dpi,
                fill: Some((palette.accent, 0.42)),
                stroke: Some((palette.accent, 1.0, stroke)),
            });

            let arrow_cy = stage_h * 0.82;
            let arrow_len = 13.0 * dpi;
            let av = if cue.horizontal {
                (0.0, dir_f)
            } else {
                (dir_f, 0.0)
            };
            push_arrow(
                &mut elements,
                cx - av.0 * arrow_len * 0.5,
                arrow_cy - av.1 * arrow_len * 0.5,
                cx + av.0 * arrow_len * 0.5,
                arrow_cy + av.1 * arrow_len * 0.5,
                palette.accent,
                1.0,
                stroke,
            );
            ((dx, dy), av)
        }
        CueKind::StackExit { trailing } => {
            let dir_f = if trailing { 1.0f32 } else { -1.0f32 };
            let slide = (0.35 + 0.65 * pulse) * 12.0 * dpi;
            let (dx, dy) = if cue.horizontal {
                (dir_f * slide, 0.0)
            } else {
                (0.0, dir_f * slide)
            };
            let win_w = (w * 0.36).max(18.0 * dpi);
            let win_h = (stage_h * 0.36).max(16.0 * dpi);

            elements.push(SvgElem::RoundedRect {
                x: cx - win_w * 0.5 - dx * 0.25,
                y: cy - win_h * 0.5 - dy * 0.25,
                w: win_w,
                h: win_h,
                rx: 4.0 * dpi,
                fill: Some((palette.frame_fill, 0.85)),
                stroke: Some((palette.frame_stroke, 0.85, thin)),
            });
            elements.push(SvgElem::RoundedRect {
                x: cx - win_w * 0.5 + dx,
                y: cy - win_h * 0.5 + dy,
                w: win_w,
                h: win_h,
                rx: 4.0 * dpi,
                fill: Some((palette.accent, 0.34)),
                stroke: Some((palette.accent, 1.0, stroke)),
            });

            let arrow_cy = stage_h * 0.82;
            let arrow_len = 16.0 * dpi;
            let av = if cue.horizontal {
                (dir_f, 0.0)
            } else {
                (0.0, dir_f)
            };
            push_arrow(
                &mut elements,
                cx - av.0 * arrow_len * 0.5,
                arrow_cy - av.1 * arrow_len * 0.5,
                cx + av.0 * arrow_len * 0.5,
                arrow_cy + av.1 * arrow_len * 0.5,
                palette.accent,
                1.0,
                stroke,
            );
            ((dx, dy), av)
        }
    };

    if let Some(label) = key_label {
        let pressed = matches!(cue.kind, CueKind::StackEntryCommitted { .. });
        let press_dy = if pressed {
            1.5 * dpi
        } else {
            pulse * 1.5 * dpi
        };
        let kw = (w * 0.68).max(48.0 * dpi);
        let kh = 20.0 * dpi;
        let kx = (w - kw) * 0.5;
        let ky = h - kh - 6.0 * dpi + press_dy;
        let (k_fill, k_stroke, k_ink) = if pressed {
            (palette.accent, palette.accent, 0xFFFFFF)
        } else {
            (palette.key_bg, palette.key_border, palette.ink)
        };
        elements.push(SvgElem::RoundedRect {
            x: kx,
            y: ky,
            w: kw,
            h: kh,
            rx: 5.0 * dpi,
            fill: Some((k_fill, 0.98)),
            stroke: Some((k_stroke, 0.95, thin)),
        });
        push_vector_label(
            &mut elements,
            label,
            kx + kw * 0.5,
            ky + kh * 0.5,
            8.5 * dpi,
            k_ink,
            (1.5 * dpi).max(1.2),
        );
    }

    SvgScene {
        rect,
        dpi,
        horizontal: cue.horizontal,
        input: cue.input,
        kind: cue.kind,
        slide_offset,
        arrow_vector,
        key_label,
        committed,
        elements,
    }
}

/// Serialize an evaluated `SvgScene` to valid standalone SVG 1.1 markup.
#[cfg_attr(not(test), allow(dead_code))]
pub fn to_svg_xml(scene: &SvgScene) -> String {
    let mut out = String::with_capacity(1024);
    let orient = if scene.horizontal {
        "landscape"
    } else {
        "portrait"
    };
    let mode = match scene.kind {
        CueKind::StackEntryPreview { .. } => "preview",
        CueKind::StackEntryCommitted { .. } => "committed",
        CueKind::StackExit { .. } => "exit",
    };
    let input = match scene.input {
        CueInput::Mouse { .. } => "mouse",
        CueInput::Trackpad => "trackpad",
    };
    let _ = write!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\" data-orientation=\"{orient}\" data-mode=\"{mode}\" data-input=\"{input}\"",
        scene.rect.w, scene.rect.h, scene.rect.w, scene.rect.h
    );
    if let Some(key) = scene.key_label {
        let _ = write!(out, " data-modifier=\"{key}\"");
    }
    out.push('>');
    for el in &scene.elements {
        match el {
            SvgElem::RoundedRect {
                x,
                y,
                w,
                h,
                rx,
                fill,
                stroke,
            } => {
                let _ = write!(
                    out,
                    "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"{rx:.1}\""
                );
                match fill {
                    Some((c, a)) => {
                        let _ = write!(out, " fill=\"#{c:06X}\" fill-opacity=\"{a:.2}\"");
                    }
                    None => out.push_str(" fill=\"none\""),
                }
                if let Some((c, a, sw)) = stroke {
                    let _ = write!(
                        out,
                        " stroke=\"#{c:06X}\" stroke-opacity=\"{a:.2}\" stroke-width=\"{sw:.1}\""
                    );
                }
                out.push_str("/>");
            }
            SvgElem::Line {
                x1,
                y1,
                x2,
                y2,
                color,
                alpha,
                width,
            } => {
                let _ = write!(
                    out,
                    "<line x1=\"{x1:.1}\" y1=\"{y1:.1}\" x2=\"{x2:.1}\" y2=\"{y2:.1}\" stroke=\"#{color:06X}\" stroke-opacity=\"{alpha:.2}\" stroke-width=\"{width:.1}\" stroke-linecap=\"round\"/>"
                );
            }
            SvgElem::Polyline {
                points,
                color,
                alpha,
                width,
            } => {
                out.push_str("<polyline fill=\"none\" points=\"");
                for (i, (px, py)) in points.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    let _ = write!(out, "{px:.1},{py:.1}");
                }
                let _ = write!(
                    out,
                    "\" stroke=\"#{color:06X}\" stroke-opacity=\"{alpha:.2}\" stroke-width=\"{width:.1}\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>"
                );
            }
        }
    }
    out.push_str("</svg>");
    out
}

#[allow(clippy::too_many_arguments)]
fn push_arrow(
    out: &mut Vec<SvgElem>,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    color: u32,
    alpha: f32,
    width: f32,
) {
    out.push(SvgElem::Line {
        x1,
        y1,
        x2,
        y2,
        color,
        alpha,
        width,
    });
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = dx.hypot(dy).max(1.0);
    let ux = dx / len;
    let uy = dy / len;
    let px = -uy;
    let py = ux;
    let head = (len * 0.42).clamp(4.0, 9.0);
    out.push(SvgElem::Polyline {
        points: vec![
            (
                x2 - ux * head + px * head * 0.7,
                y2 - uy * head + py * head * 0.7,
            ),
            (x2, y2),
            (
                x2 - ux * head - px * head * 0.7,
                y2 - uy * head - py * head * 0.7,
            ),
        ],
        color,
        alpha,
        width,
    });
}

#[allow(clippy::too_many_arguments)]
fn push_double_arrow(
    out: &mut Vec<SvgElem>,
    cx: f32,
    cy: f32,
    hx: f32,
    hy: f32,
    color: u32,
    alpha: f32,
    width: f32,
) {
    let x1 = cx - hx;
    let y1 = cy - hy;
    let x2 = cx + hx;
    let y2 = cy + hy;
    out.push(SvgElem::Line {
        x1,
        y1,
        x2,
        y2,
        color,
        alpha,
        width,
    });
    let len = (2.0 * hx).hypot(2.0 * hy).max(1.0);
    let ux = (2.0 * hx) / len;
    let uy = (2.0 * hy) / len;
    let px = -uy;
    let py = ux;
    let head = (len * 0.28).clamp(3.5, 7.5);
    out.push(SvgElem::Polyline {
        points: vec![
            (
                x2 - ux * head + px * head * 0.75,
                y2 - uy * head + py * head * 0.75,
            ),
            (x2, y2),
            (
                x2 - ux * head - px * head * 0.75,
                y2 - uy * head - py * head * 0.75,
            ),
        ],
        color,
        alpha,
        width,
    });
    out.push(SvgElem::Polyline {
        points: vec![
            (
                x1 + ux * head + px * head * 0.75,
                y1 + uy * head + py * head * 0.75,
            ),
            (x1, y1),
            (
                x1 + ux * head - px * head * 0.75,
                y1 + uy * head - py * head * 0.75,
            ),
        ],
        color,
        alpha,
        width,
    });
}

fn glyph_strokes(ch: char) -> &'static [&'static [(f32, f32)]] {
    match ch.to_ascii_uppercase() {
        'S' => &[&[
            (1.0, 0.15),
            (0.2, 0.0),
            (0.0, 0.22),
            (0.15, 0.48),
            (0.85, 0.52),
            (1.0, 0.78),
            (0.8, 1.0),
            (0.0, 0.85),
        ]],
        'H' => &[
            &[(0.0, 0.0), (0.0, 1.0)],
            &[(1.0, 0.0), (1.0, 1.0)],
            &[(0.0, 0.5), (1.0, 0.5)],
        ],
        'I' => &[
            &[(0.2, 0.0), (0.8, 0.0)],
            &[(0.5, 0.0), (0.5, 1.0)],
            &[(0.2, 1.0), (0.8, 1.0)],
        ],
        'F' => &[
            &[(1.0, 0.0), (0.0, 0.0), (0.0, 1.0)],
            &[(0.0, 0.5), (0.75, 0.5)],
        ],
        'T' => &[&[(0.0, 0.0), (1.0, 0.0)], &[(0.5, 0.0), (0.5, 1.0)]],
        'C' => &[&[
            (1.0, 0.15),
            (0.3, 0.0),
            (0.0, 0.25),
            (0.0, 0.75),
            (0.3, 1.0),
            (1.0, 0.85),
        ]],
        'R' => &[
            &[
                (0.0, 1.0),
                (0.0, 0.0),
                (0.75, 0.0),
                (1.0, 0.2),
                (0.75, 0.5),
                (0.0, 0.5),
            ],
            &[(0.45, 0.5), (1.0, 1.0)],
        ],
        'L' => &[&[(0.0, 0.0), (0.0, 1.0), (1.0, 1.0)]],
        'A' => &[
            &[(0.0, 1.0), (0.5, 0.0), (1.0, 1.0)],
            &[(0.2, 0.65), (0.8, 0.65)],
        ],
        _ => &[],
    }
}

fn push_vector_label(
    out: &mut Vec<SvgElem>,
    label: &str,
    cx: f32,
    cy: f32,
    char_h: f32,
    color: u32,
    width: f32,
) {
    let chars: Vec<char> = label.chars().collect();
    if chars.is_empty() {
        return;
    }
    let char_w = char_h * 0.62;
    let gap = char_h * 0.28;
    let total_w = chars.len() as f32 * char_w + (chars.len().saturating_sub(1)) as f32 * gap;
    let start_x = cx - total_w * 0.5;
    let top_y = cy - char_h * 0.5;
    for (idx, ch) in chars.into_iter().enumerate() {
        let ox = start_x + idx as f32 * (char_w + gap);
        for stroke_pts in glyph_strokes(ch) {
            let points = stroke_pts
                .iter()
                .map(|&(gx, gy)| (ox + gx * char_w, top_y + gy * char_h))
                .collect();
            out.push(SvgElem::Polyline {
                points,
                color,
                alpha: 1.0,
                width,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cue(horizontal: bool, input: CueInput, kind: CueKind) -> GestureCue {
        let monitor_area = if horizontal {
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1040,
            }
        } else {
            Rect {
                x: 0,
                y: 0,
                w: 1080,
                h: 1880,
            }
        };
        GestureCue {
            monitor: "MON0".into(),
            monitor_area,
            held_rect: Rect {
                x: 300,
                y: 300,
                w: 600,
                h: 500,
            },
            target_rect: Some(Rect {
                x: 400,
                y: 200,
                w: 700,
                h: 600,
            }),
            horizontal,
            input,
            kind,
            started_at: Instant::now(),
        }
    }

    #[test]
    fn landscape_and_portrait_rotate_animation_and_arrow_axis() {
        let pal = CuePalette::from_system(true, 0x00F6823B);
        let land = evaluate_cue(
            &sample_cue(
                true,
                CueInput::Trackpad,
                CueKind::StackEntryPreview { after: false },
            ),
            300,
            pal,
            &[],
            1.0,
        );
        let port = evaluate_cue(
            &sample_cue(
                false,
                CueInput::Trackpad,
                CueKind::StackEntryPreview { after: false },
            ),
            300,
            pal,
            &[],
            1.0,
        );
        assert!(land.slide_offset.0.abs() < 1e-4 && land.slide_offset.1.abs() > 1.0);
        assert_eq!(land.arrow_vector, (0.0, 1.0));
        assert!(port.slide_offset.1.abs() < 1e-4 && port.slide_offset.0.abs() > 1.0);
        assert_eq!(port.arrow_vector, (1.0, 0.0));
        assert!(land.key_label.is_none() && port.key_label.is_none());

        let land_svg = to_svg_xml(&land);
        let port_svg = to_svg_xml(&port);
        assert!(land_svg.contains("data-orientation=\"landscape\""));
        assert!(land_svg.contains("data-input=\"trackpad\""));
        assert!(!land_svg.contains("data-modifier="));
        assert!(port_svg.contains("data-orientation=\"portrait\""));
    }

    #[test]
    fn mouse_cue_displays_actual_configured_modifier_and_transitions_to_committed() {
        let pal = CuePalette::from_system(false, 0x00EB6325);
        for modifier in ["Shift", "Ctrl", "Alt"] {
            let prev = evaluate_cue(
                &sample_cue(
                    true,
                    CueInput::Mouse { modifier },
                    CueKind::StackEntryPreview { after: true },
                ),
                150,
                pal,
                &[],
                1.0,
            );
            let comm = evaluate_cue(
                &sample_cue(
                    true,
                    CueInput::Mouse { modifier },
                    CueKind::StackEntryCommitted { direction: 0 },
                ),
                150,
                pal,
                &[],
                1.0,
            );
            assert_eq!(prev.key_label, Some(modifier));
            assert!(!prev.committed);
            assert_eq!(comm.key_label, Some(modifier));
            assert!(comm.committed);
            assert_ne!(prev.elements, comm.elements);
            let xml = to_svg_xml(&prev);
            assert!(xml.contains(&format!("data-modifier=\"{modifier}\"")));
            assert!(xml.contains("data-mode=\"preview\""));
            assert!(to_svg_xml(&comm).contains("data-mode=\"committed\""));
        }
    }

    #[test]
    fn stack_exit_cue_rotates_with_orientation_and_direction() {
        let pal = CuePalette::from_system(true, 0x00F6823B);
        let land_left = evaluate_cue(
            &sample_cue(
                true,
                CueInput::Trackpad,
                CueKind::StackExit { trailing: false },
            ),
            400,
            pal,
            &[],
            1.0,
        );
        let land_right = evaluate_cue(
            &sample_cue(
                true,
                CueInput::Trackpad,
                CueKind::StackExit { trailing: true },
            ),
            400,
            pal,
            &[],
            1.0,
        );
        let port_up = evaluate_cue(
            &sample_cue(
                false,
                CueInput::Trackpad,
                CueKind::StackExit { trailing: false },
            ),
            400,
            pal,
            &[],
            1.0,
        );
        let port_down = evaluate_cue(
            &sample_cue(
                false,
                CueInput::Trackpad,
                CueKind::StackExit { trailing: true },
            ),
            400,
            pal,
            &[],
            1.0,
        );
        assert!(land_left.slide_offset.0 < 0.0 && land_left.arrow_vector == (-1.0, 0.0));
        assert!(land_right.slide_offset.0 > 0.0 && land_right.arrow_vector == (1.0, 0.0));
        assert!(port_up.slide_offset.1 < 0.0 && port_up.arrow_vector == (0.0, -1.0));
        assert!(port_down.slide_offset.1 > 0.0 && port_down.arrow_vector == (0.0, 1.0));
    }

    #[test]
    fn cue_avoids_pip_holes_scales_with_dpi_and_follows_theme_and_accent() {
        let cue = sample_cue(
            true,
            CueInput::Mouse { modifier: "Ctrl" },
            CueKind::StackEntryPreview { after: false },
        );
        let r100 = place_cue_rect(&cue, &[], 1.0);
        let r200 = place_cue_rect(&cue, &[], 2.0);
        assert!(r200.w >= r100.w * 2 - 2 && r200.h >= r100.h * 2 - 2);

        let pip = r100;
        let relocated = place_cue_rect(&cue, &[pip], 1.0);
        assert_eq!(
            overlap_area(relocated, pip),
            0,
            "cue must not obscure PiP hole"
        );
        assert!(relocated.x >= cue.monitor_area.x);
        assert!(relocated.y >= cue.monitor_area.y);
        assert!(relocated.x + relocated.w <= cue.monitor_area.x + cue.monitor_area.w);
        assert!(relocated.y + relocated.h <= cue.monitor_area.y + cue.monitor_area.h);

        let dark_pal = CuePalette::from_system(true, 0x00112233);
        let light_pal = CuePalette::from_system(false, 0x00445566);
        assert_ne!(dark_pal.card_bg, light_pal.card_bg);
        assert_ne!(dark_pal.ink, light_pal.ink);
        assert_eq!(dark_pal.accent, 0x332211);
        assert_eq!(light_pal.accent, 0x665544);
    }
}

//! Pure pixel maths: outline fitting, border colour boxes, interior statistics, PNG export.
//!
//! Everything here works on an in-memory BGRA frame (the format DXGI duplication and FFmpeg give
//! us) and has no Windows dependency beyond the PNG writer, so it is unit-testable on synthetic
//! frames.

use crate::util::Rect;

#[derive(Clone, Debug)]
pub struct Image {
    pub w: i32,
    pub h: i32,
    /// BGRA, rows tightly packed.
    pub bgra: Vec<u8>,
}

impl Image {
    pub fn new(w: i32, h: i32, bgra: Vec<u8>) -> Self {
        debug_assert_eq!(bgra.len(), (w.max(0) * h.max(0) * 4) as usize);
        Self { w, h, bgra }
    }

    /// Solid-colour frame, useful for tests (and as the "no frame yet" placeholder).
    pub fn solid(w: i32, h: i32, rgb: [u8; 3]) -> Self {
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for px in bgra.chunks_exact_mut(4) {
            px[0] = rgb[2];
            px[1] = rgb[1];
            px[2] = rgb[0];
            px[3] = 255;
        }
        Self { w, h, bgra }
    }

    pub fn black(w: i32, h: i32) -> Self {
        Self::solid(w, h, [0, 0, 0])
    }

    #[inline]
    pub fn in_bounds(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < self.w && y < self.h
    }

    /// Pixel at (x, y); coordinates outside the frame read the nearest edge pixel instead of
    /// running off the buffer (a tile that is partly off the monitor is measured near the border).
    #[inline]
    pub fn rgb(&self, x: i32, y: i32) -> [u8; 3] {
        let i = self.index(x, y);
        [self.bgra[i + 2], self.bgra[i + 1], self.bgra[i]]
    }

    #[inline]
    fn index(&self, x: i32, y: i32) -> usize {
        let x = x.max(0).min(self.w - 1);
        let y = y.max(0).min(self.h - 1);
        ((y * self.w + x) * 4) as usize
    }

    #[inline]
    pub fn lum(&self, x: i32, y: i32) -> f32 {
        let i = self.index(x, y);
        0.114 * self.bgra[i] as f32
            + 0.587 * self.bgra[i + 1] as f32
            + 0.299 * self.bgra[i + 2] as f32
    }

    pub fn fill(&mut self, rect: Rect, rgb: [u8; 3]) {
        self.fill_bgra(rect, [rgb[2], rgb[1], rgb[0], 255]);
    }

    pub fn fill_bgra(&mut self, rect: Rect, bgra: [u8; 4]) {
        let r = rect.clamp_to(Rect::new(0, 0, self.w, self.h));
        for y in r.y..r.bottom() {
            for x in r.x..r.right() {
                let i = ((y * self.w + x) * 4) as usize;
                self.bgra[i..i + 4].copy_from_slice(&bgra);
            }
        }
    }

    /// Draw a 2 px border just inside `rect` (test helper).
    pub fn outline(&mut self, rect: Rect, rgb: [u8; 3], width: i32) {
        self.fill(Rect::new(rect.x, rect.y, rect.w, width), rgb);
        self.fill(Rect::new(rect.x, rect.bottom() - width, rect.w, width), rgb);
        self.fill(Rect::new(rect.x, rect.y, width, rect.h), rgb);
        self.fill(Rect::new(rect.right() - width, rect.y, width, rect.h), rgb);
    }

    pub fn crop(&self, rect: Rect) -> Image {
        let r = rect.clamp_to(Rect::new(0, 0, self.w, self.h));
        let mut out = vec![0u8; (r.w.max(0) * r.h.max(0) * 4) as usize];
        for y in 0..r.h {
            let src = (((r.y + y) * self.w + r.x) * 4) as usize;
            let dst = (y * r.w * 4) as usize;
            out[dst..dst + (r.w * 4) as usize]
                .copy_from_slice(&self.bgra[src..src + (r.w * 4) as usize]);
        }
        Image::new(r.w, r.h, out)
    }

    /// Nearest-neighbour scale to `max_width`, preserving aspect. No-op when already small.
    pub fn scaled_to_width(&self, max_width: i32) -> Image {
        if self.w <= max_width || max_width <= 0 || self.w <= 0 || self.h <= 0 {
            return self.clone();
        }
        let nw = max_width;
        let nh = ((self.h as i64 * nw as i64) / self.w as i64).max(1) as i32;
        let mut out = vec![0u8; (nw * nh * 4) as usize];
        for y in 0..nh {
            let sy = (y as i64 * self.h as i64 / nh as i64) as i32;
            for x in 0..nw {
                let sx = (x as i64 * self.w as i64 / nw as i64) as i32;
                let si = ((sy * self.w + sx) * 4) as usize;
                let di = ((y * nw + x) * 4) as usize;
                out[di..di + 4].copy_from_slice(&self.bgra[si..si + 4]);
            }
        }
        Image::new(nw, nh, out)
    }
}

// ---- Outline fitting ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Fit {
    /// Measured rectangle, or `None` when nothing could be measured (`unmeasurable`).
    pub measured: Option<Rect>,
    pub offset: (i32, i32),
    /// Confidence of the measurement (classification sides are high, a luma fallback keeps its score).
    pub score: f32,
    pub vertical_score: f32,
    pub horizontal_score: f32,
    /// Which sides were really measured (left, right, top, bottom). A side that was not measured keeps
    /// its expected coordinate in `measured`, so it proves nothing about where the window is.
    pub sides: [bool; 4],
    /// How each measured side was found ("right gap 6 px at 717 (far window)").
    pub notes: Vec<String>,
    /// Why sides could not be measured (empty when every side was measured).
    pub reason: String,
}

/// Pixel meaning for the layout measurement: Planor knows the wallpaper is solid dark blue
/// RGB(0,0,90), taskbars come from `meta.json`, and everything else is window content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelClass {
    Wallpaper,
    Taskbar,
    Window,
}

/// What the analyzer knows about the expected layout besides the rectangle itself.
#[derive(Clone, Debug)]
pub struct LayoutContext {
    /// Monitor work area in image (monitor-local) coordinates.
    pub work: Rect,
    /// Taskbar rectangles in image (monitor-local) coordinates.
    pub taskbars: Vec<Rect>,
    /// Planor's inter-tile gap (`layout.gap_px`).
    pub gap_px: i32,
}

impl LayoutContext {
    pub fn new(work: Rect, taskbars: Vec<Rect>, gap_px: i32) -> Self {
        Self {
            work,
            taskbars,
            gap_px: gap_px.clamp(0, 64),
        }
    }

    /// Test/default context: the whole image is work area, there is no taskbar, gap 6 px.
    pub fn for_image(img: &Image) -> Self {
        Self::new(Rect::new(0, 0, img.w, img.h), Vec::new(), 6)
    }
}

/// Dark-blue wallpaper, tolerant of H.264 noise and of the shadow gradient that darkens it near
/// windows. Planor's border colours and window content are deliberately excluded: they are either
/// too bright in R/G or not blue-dominant enough.
pub fn is_wallpaper_rgb(rgb: [u8; 3]) -> bool {
    let r = rgb[0] as i32;
    let g = rgb[1] as i32;
    let b = rgb[2] as i32;
    // A focused window's shadow can darken the wallpaper in the inter-tile gap all the way to black
    // (seen on a real recording: the 6 px gap was RGB(0,0,0)). Near-pure black therefore counts as
    // shadowed wallpaper; genuinely dark window content (e.g. RGB(10,10,10)) stays a window.
    let shadowed_black = r <= 3 && g <= 3 && b <= 3;
    shadowed_black || (b >= 20 && b > 2 * r + 5 && b > 2 * g + 5 && r <= 30 && g <= 30)
}

fn class_at(img: &Image, x: i32, y: i32, taskbars: &[Rect]) -> PixelClass {
    if taskbars.iter().any(|t| t.contains(x, y)) {
        return PixelClass::Taskbar;
    }
    if is_wallpaper_rgb(img.rgb(x, y)) {
        PixelClass::Wallpaper
    } else {
        PixelClass::Window
    }
}

/// Run-length classification (wallpaper / taskbar / window) along row `y`, as `(class, start, end)`.
pub fn class_runs_row(img: &Image, y: i32, taskbars: &[Rect]) -> Vec<(PixelClass, i32, i32)> {
    let mut out: Vec<(PixelClass, i32, i32)> = Vec::new();
    for x in 0..img.w {
        let c = class_at(img, x, y, taskbars);
        match out.last_mut() {
            Some(last) if last.0 == c => last.2 = x + 1,
            _ => out.push((c, x, x + 1)),
        }
    }
    out
}

/// Share of grid samples (every `step` px) inside `area` and outside every rectangle of `excluded` that are
/// not wallpaper. That free area should show only the solid wallpaper; content there is another program's
/// window. `None` when too few samples fall in the free area to say anything.
pub fn foreign_content_fraction(
    img: &Image,
    area: Rect,
    excluded: &[Rect],
    step: i32,
) -> Option<f32> {
    let step = step.max(1);
    let area = area.intersection(Rect::new(0, 0, img.w, img.h));
    let (mut total, mut foreign) = (0usize, 0usize);
    let mut y = area.y;
    while y < area.bottom() {
        let mut x = area.x;
        while x < area.right() {
            if !excluded.iter().any(|r| r.contains(x, y)) {
                total += 1;
                if !is_wallpaper_rgb(img.rgb(x, y)) {
                    foreign += 1;
                }
            }
            x += step;
        }
        y += step;
    }
    (total >= 50).then(|| foreign as f32 / total as f32)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    fn label(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Top => "top",
            Side::Bottom => "bottom",
        }
    }

    fn vertical(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }

    /// -1 when the outside of the window is in the negative axis direction, +1 otherwise.
    fn outward(self) -> i32 {
        match self {
            Side::Left | Side::Top => -1,
            Side::Right | Side::Bottom => 1,
        }
    }

    fn side_coord(self, r: Rect) -> i32 {
        match self {
            Side::Left => r.x,
            Side::Right => r.right(),
            Side::Top => r.y,
            Side::Bottom => r.bottom(),
        }
    }

    /// The extent along the side (y for vertical sides, x for horizontal sides).
    fn perpendicular_span(self, r: Rect) -> (i32, i32) {
        if self.vertical() {
            (r.y, r.bottom())
        } else {
            (r.x, r.right())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Far {
    Edge,
    Taskbar,
    Window,
}

impl Far {
    fn label(self) -> &'static str {
        match self {
            Far::Edge => "edge",
            Far::Taskbar => "taskbar",
            Far::Window => "window",
        }
    }
}

#[derive(Clone, Debug)]
struct SideMeasurement {
    pos: i32,
    score: f32,
    note: String,
}

/// A monitor/work-area edge or a taskbar edge that the expected side is flush with.
fn known_edge(
    img: &Image,
    expected: Rect,
    side: Side,
    ctx: &LayoutContext,
) -> Option<(i32, &'static str)> {
    let target = side.side_coord(expected);
    let (span_lo, span_hi) = side.perpendicular_span(expected);
    let mut best: Option<(i32, &'static str, i32)> = None;
    let mut consider = |coord: i32, label: &'static str, priority: i32| {
        let d = (coord - target).abs();
        if d > 2 {
            return;
        }
        let better = match best {
            None => true,
            Some((b, _, bp)) => {
                d < (b - target).abs() || (d == (b - target).abs() && priority > bp)
            }
        };
        if better {
            best = Some((coord, label, priority));
        }
    };
    let (monitor, work) = match side {
        Side::Left => (0, ctx.work.x),
        Side::Right => (img.w, ctx.work.right()),
        Side::Top => (0, ctx.work.y),
        Side::Bottom => (img.h, ctx.work.bottom()),
    };
    consider(monitor, "monitor edge", 1);
    consider(work, "work-area edge", 2);
    for t in &ctx.taskbars {
        let (lo, hi) = side.perpendicular_span(*t);
        if lo >= span_hi || span_lo >= hi {
            continue;
        }
        let edge = match side {
            Side::Left => t.right(),
            Side::Right => t.x,
            Side::Top => t.bottom(),
            Side::Bottom => t.y,
        };
        consider(edge, "taskbar edge", 3);
    }
    best.map(|(coord, label, _)| (coord, label))
}

/// Fraction of rows/columns just inside `side` that contain window pixels. This is how a flush edge
/// is confirmed: the window must actually reach it. One window pixel in the four-pixel deep band is
/// enough per sample, so a single H.264 bleed pixel between a Planor border and its content does not
/// turn a real flush edge into an unknown.
fn window_band_fraction(img: &Image, expected: Rect, side: Side, taskbars: &[Rect]) -> f32 {
    let (span_lo, span_hi) = side.perpendicular_span(expected);
    let corner = ((span_hi - span_lo) / 8).clamp(0, 12);
    let lo = span_lo + corner;
    let hi = span_hi - corner;
    if hi <= lo {
        return 0.0;
    }
    let step = ((hi - lo) / 120).max(1);
    let cross: [i32; 4] = match side {
        Side::Left => [
            expected.x + 1,
            expected.x + 2,
            expected.x + 3,
            expected.x + 4,
        ],
        Side::Right => [
            expected.right() - 2,
            expected.right() - 3,
            expected.right() - 4,
            expected.right() - 5,
        ],
        Side::Top => [
            expected.y + 1,
            expected.y + 2,
            expected.y + 3,
            expected.y + 4,
        ],
        Side::Bottom => [
            expected.bottom() - 2,
            expected.bottom() - 3,
            expected.bottom() - 4,
            expected.bottom() - 5,
        ],
    };
    let mut total = 0u32;
    let mut window = 0u32;
    let mut along = lo;
    while along < hi {
        let mut any_window = false;
        for c in cross {
            let (x, y) = if side.vertical() {
                (c, along)
            } else {
                (along, c)
            };
            if img.in_bounds(x, y) && class_at(img, x, y, taskbars) == PixelClass::Window {
                any_window = true;
                break;
            }
        }
        total += 1;
        if any_window {
            window += 1;
        }
        along += step;
    }
    if total == 0 {
        0.0
    } else {
        window as f32 / total as f32
    }
}

fn majority_far(members: &[(i32, i32, Far)]) -> Far {
    let mut edge = 0;
    let mut taskbar = 0;
    let mut window = 0;
    for m in members {
        match m.2 {
            Far::Edge => edge += 1,
            Far::Taskbar => taskbar += 1,
            Far::Window => window += 1,
        }
    }
    if edge >= taskbar && edge >= window {
        Far::Edge
    } else if taskbar >= window {
        Far::Taskbar
    } else {
        Far::Window
    }
}

/// Find the window side by its wallpaper boundary: the wallpaper strip touching the window (a gap
/// between tiles, open desktop, or the monitor edge) is bounded by the window on the inside. The
/// far edge of the strip is deliberately never considered a candidate: that is the neighbour's
/// edge, the old 7 px error.
fn classify_side(
    img: &Image,
    expected: Rect,
    side: Side,
    search: i32,
    ctx: &LayoutContext,
) -> Option<SideMeasurement> {
    let target = side.side_coord(expected);
    let (span_lo, span_hi) = side.perpendicular_span(expected);
    let corner = ((span_hi - span_lo) / 12).clamp(2, 12);
    let lo_span = span_lo + corner;
    let hi_span = span_hi - corner;
    if hi_span <= lo_span {
        return None;
    }
    let limit = if side.vertical() { img.w } else { img.h };
    let extra = 48;
    let (scan_lo, scan_hi) = if side.outward() > 0 {
        (
            (target - search - 2).max(0),
            (target + search + extra + 2).min(limit),
        )
    } else {
        (
            (target - search - extra - 2).max(0),
            (target + search + 2).min(limit),
        )
    };
    if scan_hi - scan_lo < 3 {
        return None;
    }
    let step = ((hi_span - lo_span) / 80).max(1);
    let mut entries: Vec<(i32, i32, Far)> = Vec::new();
    let mut classes: Vec<PixelClass> = Vec::new();
    let mut rows = 0u32;
    let mut along = lo_span;
    while along < hi_span {
        classes.clear();
        for p in scan_lo..scan_hi {
            let (x, y) = if side.vertical() {
                (p, along)
            } else {
                (along, p)
            };
            classes.push(class_at(img, x, y, &ctx.taskbars));
        }
        let n = classes.len();
        let mut i = 0usize;
        while i < n {
            if classes[i] != PixelClass::Wallpaper {
                i += 1;
                continue;
            }
            let a = i;
            while i < n && classes[i] == PixelClass::Wallpaper {
                i += 1;
            }
            let b = i;
            if b - a < 2 {
                continue;
            }
            let entry = if side.outward() > 0 {
                // Window is before the strip; the near boundary is the strip's left/top side.
                if a == 0 || classes[a - 1] != PixelClass::Window {
                    None
                } else {
                    let far = if b >= n {
                        Far::Edge
                    } else {
                        match classes[b] {
                            PixelClass::Window => Far::Window,
                            PixelClass::Taskbar => Far::Taskbar,
                            PixelClass::Wallpaper => continue,
                        }
                    };
                    Some((scan_lo + a as i32, (b - a) as i32, far))
                }
            } else {
                // Window is after the strip; the near boundary is the strip's right/bottom side.
                if b >= n || classes[b] != PixelClass::Window {
                    None
                } else {
                    let far = if a == 0 {
                        Far::Edge
                    } else {
                        match classes[a - 1] {
                            PixelClass::Window => Far::Window,
                            PixelClass::Taskbar => Far::Taskbar,
                            PixelClass::Wallpaper => continue,
                        }
                    };
                    Some((scan_lo + b as i32, (b - a) as i32, far))
                }
            };
            if let Some(entry) = entry {
                if (entry.0 - target).abs() <= search {
                    entries.push(entry);
                }
            }
        }
        rows += 1;
        along += step;
    }
    if entries.is_empty() || rows == 0 {
        return None;
    }
    entries.sort_by_key(|e| e.0);
    let mut clusters: Vec<Vec<(i32, i32, Far)>> = Vec::new();
    for entry in entries {
        match clusters.last_mut() {
            Some(cluster) if entry.0 - cluster.last().unwrap().0 <= 1 => cluster.push(entry),
            _ => clusters.push(vec![entry]),
        }
    }
    let mut best: Option<(f32, Vec<(i32, i32, Far)>)> = None;
    for members in clusters {
        let support = members.len() as f32;
        if support < 2.0 {
            continue;
        }
        let mut widths: Vec<i32> = members.iter().map(|m| m.1).collect();
        widths.sort_unstable();
        let width = widths[widths.len() / 2];
        let far = majority_far(&members);
        let bonus = if far == Far::Edge {
            1.5
        } else if far == Far::Taskbar {
            1.3
        } else if (width - ctx.gap_px).abs() <= 2 {
            1.4
        } else {
            1.0
        };
        let rank = support * bonus;
        if best.as_ref().is_none_or(|(r, _)| rank > *r) {
            best = Some((rank, members));
        }
    }
    let (_, members) = best?;
    let support = members.len() as f32;
    if support < (rows as f32 * 0.35).max(2.0) {
        return None;
    }
    let mut positions: Vec<i32> = members.iter().map(|m| m.0).collect();
    positions.sort_unstable();
    let pos = positions[positions.len() / 2];
    let mut widths: Vec<i32> = members.iter().map(|m| m.1).collect();
    widths.sort_unstable();
    let width = widths[widths.len() / 2];
    let far = majority_far(&members);
    let kind = if far == Far::Window && (width - ctx.gap_px).abs() <= 2 {
        "gap"
    } else if far == Far::Edge {
        "open wallpaper"
    } else if far == Far::Taskbar {
        "taskbar-side wallpaper"
    } else {
        "wallpaper strip"
    };
    // The scan window limits how far the far side is looked for. Do not print a truncated width as
    // if it were the real strip width: for open wallpaper name the near boundary only.
    let note = if far == Far::Edge {
        format!("{} {kind} at {pos} (far edge)", side.label())
    } else {
        let far_pos = if side.outward() > 0 {
            pos + width
        } else {
            pos - width
        };
        let (lo, hi) = if far_pos < pos {
            (far_pos, pos)
        } else {
            (pos, far_pos)
        };
        format!(
            "{} {kind} {width} px {lo}..{hi} (far {})",
            side.label(),
            far.label()
        )
    };
    Some(SideMeasurement {
        pos,
        score: (support / rows as f32).clamp(0.0, 1.0),
        note,
    })
}

/// How strong and how much of a *step* the boundary between the pixel at (x,y) and its neighbour
/// in the search direction is. Returns `None` when either pixel is outside the image.
///
/// The boundary is between (x,y) and (x+u, y+v); callers turn the winning coordinate into a
/// rectangle side by adding one.
#[inline]
fn side_score(
    img: &Image,
    x: i32,
    y: i32,
    horizontal: bool,
    inside_sign: i32,
    band: i32,
) -> Option<(f32, f32)> {
    let (u, v) = if horizontal { (1, 0) } else { (0, 1) };
    let a = (x, y);
    let b = (x + u, y + v);
    if !img.in_bounds(a.0, a.1) || !img.in_bounds(b.0, b.1) {
        return None;
    }
    let edge = (img.lum(a.0, a.1) - img.lum(b.0, b.1)).abs() / 255.0;
    let mut inside = 0.0;
    let mut outside = 0.0;
    let mut outside_sq = 0.0;
    let mut n = 0;
    // A wide band (up to 24 px) averages out window texture, so an internal checkerboard or text
    // edge does not beat the real boundary.
    let mut k = 2;
    while k <= band {
        let ix = x + inside_sign * k * u;
        let iy = y + inside_sign * k * v;
        let ox = x - inside_sign * k * u;
        let oy = y - inside_sign * k * v;
        if img.in_bounds(ix, iy) && img.in_bounds(ox, oy) {
            let o = img.lum(ox, oy);
            inside += img.lum(ix, iy);
            outside += o;
            outside_sq += o * o;
            n += 1;
        }
        k += 2;
    }
    if n == 0 {
        return Some((edge, 1.0));
    }
    let step = ((inside - outside) / n as f32).abs() / 255.0;
    // A real window boundary has a uniform far side (wallpaper, taskbar, the next window). The inner
    // edge of a 2 px Planor border has the border itself in the "outside" band. Uniformity is used
    // to rank candidates, not to scale the reported contrast.
    let mean_out = outside / n as f32;
    let var_out = (outside_sq / n as f32 - mean_out * mean_out).max(0.0);
    let uniformity = 1.0 / (1.0 + var_out.sqrt() / 24.0);
    Some((edge.min(step), uniformity))
}

/// Average `side_score` along a vertical edge whose gradient is centred on column `x`.
fn vertical_line_score(
    img: &Image,
    rect: Rect,
    x: i32,
    inside_sign: i32,
    band: i32,
) -> Option<(f32, f32)> {
    if rect.h <= 4 {
        return None;
    }
    let step = (rect.h / 80).max(1);
    let mut sum = 0.0;
    let mut uniformity = 0.0;
    let mut n = 0;
    let mut y = rect.y + step;
    while y < rect.bottom() - step {
        if let Some((s, u)) = side_score(img, x, y, true, inside_sign, band) {
            sum += s;
            uniformity += u;
            n += 1;
        }
        y += step;
    }
    (n > 0).then_some((sum / n as f32, uniformity / n as f32))
}

/// Average `side_score` along a horizontal edge whose gradient is centred on row `y`.
fn horizontal_line_score(
    img: &Image,
    rect: Rect,
    y: i32,
    inside_sign: i32,
    band: i32,
) -> Option<(f32, f32)> {
    if rect.w <= 4 {
        return None;
    }
    let step = (rect.w / 80).max(1);
    let mut sum = 0.0;
    let mut uniformity = 0.0;
    let mut n = 0;
    let mut x = rect.x + step;
    while x < rect.right() - step {
        if let Some((s, u)) = side_score(img, x, y, false, inside_sign, band) {
            sum += s;
            uniformity += u;
            n += 1;
        }
        x += step;
    }
    (n > 0).then_some((sum / n as f32, uniformity / n as f32))
}

fn best_vertical(
    img: &Image,
    rect: Rect,
    base: i32,
    inside: i32,
    search: i32,
    band: i32,
) -> Option<(i32, f32)> {
    let mut best: Option<(i32, f32, f32)> = None; // pos, raw score, rank
    for dx in -search..=search {
        if let Some((score, uniformity)) = vertical_line_score(img, rect, base + dx, inside, band) {
            let rank = score * (0.3 + 0.7 * uniformity);
            if best.is_none_or(|(_, _, b)| rank > b) {
                best = Some((base + dx, score, rank));
            }
        }
    }
    best.map(|(pos, score, _)| (pos, score))
}

fn best_horizontal(
    img: &Image,
    rect: Rect,
    base: i32,
    inside: i32,
    search: i32,
    band: i32,
) -> Option<(i32, f32)> {
    let mut best: Option<(i32, f32, f32)> = None; // pos, raw score, rank
    for dy in -search..=search {
        if let Some((score, uniformity)) = horizontal_line_score(img, rect, base + dy, inside, band)
        {
            let rank = score * (0.3 + 0.7 * uniformity);
            if best.is_none_or(|(_, _, b)| rank > b) {
                best = Some((base + dy, score, rank));
            }
        }
    }
    best.map(|(pos, score, _)| (pos, score))
}

/// Measure one side in priority order: a known flush edge (monitor/work area/taskbar), then the
/// wallpaper boundary that touches the window, and only then the old luma step.
fn measure_side(
    img: &Image,
    expected: Rect,
    side: Side,
    search: i32,
    min_score: f32,
    ctx: &LayoutContext,
    failures: &mut Vec<String>,
) -> Option<SideMeasurement> {
    if let Some((pos, label)) = known_edge(img, expected, side, ctx) {
        let fraction = window_band_fraction(img, expected, side, &ctx.taskbars);
        if fraction >= 0.7 {
            return Some(SideMeasurement {
                pos,
                score: 1.0,
                note: format!("{} {label} at {pos}", side.label()),
            });
        }
        failures.push(format!(
            "{}: {label} at {pos} but only {:.0}% of the inside band is window",
            side.label(),
            fraction * 100.0
        ));
    }
    if let Some(m) = classify_side(img, expected, side, search, ctx) {
        return Some(m);
    }
    let vertical_band = (expected.w / 4).clamp(4, 24);
    let horizontal_band = (expected.h / 4).clamp(4, 24);
    let fallback = match side {
        Side::Left => best_vertical(img, expected, expected.x, 1, search, vertical_band),
        Side::Right => best_vertical(
            img,
            expected,
            expected.right() - 1,
            -1,
            search,
            vertical_band,
        ),
        Side::Top => best_horizontal(img, expected, expected.y, 1, search, horizontal_band),
        Side::Bottom => best_horizontal(
            img,
            expected,
            expected.bottom() - 1,
            -1,
            search,
            horizontal_band,
        ),
    };
    match fallback {
        Some((pos, score)) if score >= min_score => Some(SideMeasurement {
            pos: pos + 1,
            score,
            note: format!(
                "{} luma edge at {} (score {score:.2})",
                side.label(),
                pos + 1
            ),
        }),
        Some((_, score)) => {
            failures.push(format!(
                "{}: no wallpaper boundary within {search} px; luma edge score {score:.2} below {min_score:.2}",
                side.label()
            ));
            None
        }
        None => {
            failures.push(format!(
                "{}: no wallpaper boundary within {search} px and no luma edge",
                side.label()
            ));
            None
        }
    }
}

/// Fit a window outline inside `search` pixels of `expected`, on all four sides independently.
///
/// Each side is measured from what Planor and the wallpaper make known:
/// - a side flush with the monitor/work-area/taskbar edge is confirmed by the pixels just inside it
///   being window, not wallpaper;
/// - an inter-tile gap or open desktop is the wallpaper strip touching the window, and the window's
///   side is that strip's near boundary (the far boundary is the neighbour, never used here);
/// - the old luma/edge step is only a fallback.
///
/// If no side can be measured the rectangle is `unmeasurable`, never guessed. A side that could not
/// be measured keeps its expected coordinate; the other three still verify the rectangle.
pub fn fit_outline(
    img: &Image,
    expected: Rect,
    search: i32,
    min_score: f32,
    ctx: &LayoutContext,
) -> Fit {
    let search = search.max(0);
    let mut failures: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut accepted = 0usize;
    let mut sides = [false; 4];
    let mut score_sum = 0.0f32;
    let mut vertical_sum = 0.0f32;
    let mut vertical_n = 0usize;
    let mut horizontal_sum = 0.0f32;
    let mut horizontal_n = 0usize;

    let mut left = expected.x;
    let mut right = expected.right();
    let mut top = expected.y;
    let mut bottom = expected.bottom();

    if let Some(m) = measure_side(
        img,
        expected,
        Side::Left,
        search,
        min_score,
        ctx,
        &mut failures,
    ) {
        left = m.pos;
        sides[0] = true;
        accepted += 1;
        score_sum += m.score;
        vertical_sum += m.score;
        vertical_n += 1;
        notes.push(m.note);
    }
    if let Some(m) = measure_side(
        img,
        expected,
        Side::Right,
        search,
        min_score,
        ctx,
        &mut failures,
    ) {
        right = m.pos;
        sides[1] = true;
        accepted += 1;
        score_sum += m.score;
        vertical_sum += m.score;
        vertical_n += 1;
        notes.push(m.note);
    }
    if let Some(m) = measure_side(
        img,
        expected,
        Side::Top,
        search,
        min_score,
        ctx,
        &mut failures,
    ) {
        top = m.pos;
        sides[2] = true;
        accepted += 1;
        score_sum += m.score;
        horizontal_sum += m.score;
        horizontal_n += 1;
        notes.push(m.note);
    }
    if let Some(m) = measure_side(
        img,
        expected,
        Side::Bottom,
        search,
        min_score,
        ctx,
        &mut failures,
    ) {
        bottom = m.pos;
        sides[3] = true;
        accepted += 1;
        score_sum += m.score;
        horizontal_sum += m.score;
        horizontal_n += 1;
        notes.push(m.note);
    }

    // A nonsensical size (sides crossed or wildly different) falls back to the expected size on
    // that axis; the position part of the fit is still usable.
    if right - left < 16 {
        right = left + expected.w;
    } else if right - left - expected.w > 2 * search || expected.w - (right - left) > 2 * search {
        right = left + expected.w;
    }
    if bottom - top < 16 {
        bottom = top + expected.h;
    } else if bottom - top - expected.h > 2 * search || expected.h - (bottom - top) > 2 * search {
        bottom = top + expected.h;
    }

    let measured = (accepted > 0).then(|| Rect::new(left, top, right - left, bottom - top));
    let reason = failures.join("; ");
    Fit {
        measured,
        offset: (left - expected.x, top - expected.y),
        score: if accepted == 0 {
            0.0
        } else {
            score_sum / accepted as f32
        },
        vertical_score: if vertical_n == 0 {
            0.0
        } else {
            vertical_sum / vertical_n as f32
        },
        horizontal_score: if horizontal_n == 0 {
            0.0
        } else {
            horizontal_sum / horizontal_n as f32
        },
        sides,
        notes,
        reason,
    }
}

// ---- Interior statistics -----------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Stats {
    pub mean: f32,
    pub std_dev: f32,
    pub samples: u32,
}

/// Mean luminance and standard deviation inside `rect`, sampling every `step` pixels.
pub fn stats(img: &Image, rect: Rect, step: i32) -> Stats {
    let r = rect.clamp_to(Rect::new(0, 0, img.w, img.h));
    let step = step.max(1);
    if r.is_empty() {
        return Stats {
            mean: 0.0,
            std_dev: 0.0,
            samples: 0,
        };
    }
    let mut sum = 0.0f64;
    let mut sum_sq = 0.0f64;
    let mut n = 0u32;
    let mut y = r.y;
    while y < r.bottom() {
        let mut x = r.x;
        while x < r.right() {
            let l = img.lum(x, y) as f64;
            sum += l;
            sum_sq += l * l;
            n += 1;
            x += step;
        }
        y += step;
    }
    if n == 0 {
        return Stats {
            mean: 0.0,
            std_dev: 0.0,
            samples: 0,
        };
    }
    let mean = sum / n as f64;
    let var = (sum_sq / n as f64 - mean * mean).max(0.0);
    Stats {
        mean: mean as f32,
        std_dev: var.sqrt() as f32,
        samples: n,
    }
}

/// Classify an interior. `Unmeasurable` means it is too close to the background to judge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interior {
    Ok,
    Black,
    Flat,
    Unmeasurable,
}

pub fn classify_interior(img: &Image, rect: Rect, background: f32) -> (Interior, Stats) {
    let inner = rect.inset(6);
    let s = stats(img, inner, 2);
    if s.samples == 0 {
        return (Interior::Unmeasurable, s);
    }
    let contrast = (s.mean - background).abs();
    if s.mean < 8.0 && s.std_dev < 6.0 {
        return (Interior::Black, s);
    }
    if s.std_dev < 1.5 && contrast < 4.0 {
        return (Interior::Unmeasurable, s);
    }
    if s.std_dev < 1.5 {
        return (Interior::Flat, s);
    }
    (Interior::Ok, s)
}

/// Mean luminance of the ring just outside `rect`, i.e. the desktop background around a window.
pub fn surrounding_luminance(img: &Image, rect: Rect) -> f32 {
    let ring = rect.expand(20).intersection(rect.expand(-1));
    let outer = rect.expand(20).clamp_to(Rect::new(0, 0, img.w, img.h));
    if outer.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0f64;
    let mut n = 0u32;
    let mut y = outer.y;
    while y < outer.bottom() {
        let mut x = outer.x;
        while x < outer.right() {
            if !ring.contains(x, y) {
                sum += img.lum(x, y) as f64;
                n += 1;
            }
            x += 4;
        }
        y += 4;
    }
    if n == 0 { 0.0 } else { (sum / n as f64) as f32 }
}

// ---- Frame summaries ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct FrameSummary {
    pub mean: f32,
    /// 32 x 18 block means, used to decide whether the monitor changed between frames.
    pub signature: Vec<u8>,
}

pub fn summarize(img: &Image) -> FrameSummary {
    let grid_w = 64;
    let grid_h = 36;
    let mut signature = Vec::with_capacity((grid_w * grid_h) as usize);
    let mut sum = 0.0f64;
    let mut n = 0u32;
    for gy in 0..grid_h {
        for gx in 0..grid_w {
            let x0 = gx * img.w / grid_w;
            let x1 = ((gx + 1) * img.w / grid_w).max(x0 + 1);
            let y0 = gy * img.h / grid_h;
            let y1 = ((gy + 1) * img.h / grid_h).max(y0 + 1);
            let mut bsum = 0.0f64;
            let mut bn = 0u32;
            let mut y = y0;
            while y < y1 {
                let mut x = x0;
                while x < x1 {
                    bsum += img.lum(x, y) as f64;
                    bn += 1;
                    x += 4;
                }
                y += 4;
            }
            let v = if bn == 0 { 0.0 } else { bsum / bn as f64 };
            sum += v;
            n += 1;
            signature.push(v.clamp(0.0, 255.0) as u8);
        }
    }
    FrameSummary {
        mean: if n == 0 { 0.0 } else { (sum / n as f64) as f32 },
        signature,
    }
}

/// True when two frames are visually identical for the frozen-monitor check.
pub fn is_static(a: &FrameSummary, b: &FrameSummary) -> bool {
    if a.signature.len() != b.signature.len() {
        return false;
    }
    a.signature
        .iter()
        .zip(&b.signature)
        .all(|(x, y)| (*x as i32 - *y as i32).abs() <= 2)
}

// ---- Planor border colours ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColourBox {
    pub rect: Rect,
    pub pixels: u32,
    /// Index into `crate::config::BORDER_COLOURS`.
    pub colour: usize,
}

fn near_colour(px: [u8; 3], target: [u8; 3], tolerance: i32) -> bool {
    (0..3).all(|i| (px[i] as i32 - target[i] as i32).abs() <= tolerance)
}

/// Count bbox corners (out of four) that contain a few pixels of the colour. A rectangular outline
/// reaches its bbox corners; a circular badge does not.
fn corner_hits(img: &Image, rect: Rect, target: [u8; 3], tolerance: i32) -> u32 {
    // Small fixed corner patch: a rounded-rectangle outline passes within ~2-4 px of its bbox
    // corners, while a circular badge's ring stays ~7 px away from the exact corner.
    let cs = 4;
    let corners = [
        Rect::new(rect.x, rect.y, cs, cs),
        Rect::new(rect.right() - cs, rect.y, cs, cs),
        Rect::new(rect.x, rect.bottom() - cs, cs, cs),
        Rect::new(rect.right() - cs, rect.bottom() - cs, cs, cs),
    ];
    let mut hits = 0;
    for c in corners {
        let mut n = 0;
        let r = c.clamp_to(Rect::new(0, 0, img.w, img.h));
        for y in r.y..r.bottom() {
            for x in r.x..r.right() {
                if near_colour(img.rgb(x, y), target, tolerance) {
                    n += 1;
                }
            }
        }
        if n >= 1 {
            hits += 1;
        }
    }
    hits
}

/// Longest horizontal and vertical run of `target`-coloured pixels inside `rect`.
fn longest_runs(img: &Image, rect: Rect, target: [u8; 3], tolerance: i32) -> (i32, i32) {
    let r = rect.clamp_to(Rect::new(0, 0, img.w, img.h));
    let mut best_h = 0;
    let mut best_v = 0;
    for y in r.y..r.bottom() {
        let mut run = 0;
        for x in r.x..r.right() {
            if near_colour(img.rgb(x, y), target, tolerance) {
                run += 1;
                best_h = best_h.max(run);
            } else {
                run = 0;
            }
        }
    }
    for x in r.x..r.right() {
        let mut run = 0;
        for y in r.y..r.bottom() {
            if near_colour(img.rgb(x, y), target, tolerance) {
                run += 1;
                best_v = best_v.max(run);
            } else {
                run = 0;
            }
        }
    }
    (best_h, best_v)
}

/// Find connected regions matching Planor's border colours. Components are found with 8-connectivity
/// over a mask so anti-aliased gaps in lossy video do not split a border into dozens of slivers.
pub fn find_border_boxes(
    img: &Image,
    colours: &[(&str, [u8; 3])],
    tolerance: i32,
    min_pixels: u32,
    min_span: i32,
) -> Vec<ColourBox> {
    let (w, h) = (img.w, img.h);
    if w <= 0 || h <= 0 {
        return Vec::new();
    }
    let mut label = vec![0u8; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let px = img.rgb(x, y);
            for (k, (_, c)) in colours.iter().enumerate() {
                if near_colour(px, *c, tolerance) {
                    label[(y * w + x) as usize] = (k + 1) as u8;
                    break;
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut stack: Vec<(i32, i32, u8)> = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let idx = (y * w + x) as usize;
            let colour = label[idx];
            if colour == 0 {
                continue;
            }
            label[idx] = 0;
            stack.push((x, y, colour));
            let mut min_x = x;
            let mut max_x = x;
            let mut min_y = y;
            let mut max_y = y;
            let mut pixels = 0u32;
            while let Some((cx, cy, c)) = stack.pop() {
                pixels += 1;
                min_x = min_x.min(cx);
                max_x = max_x.max(cx);
                min_y = min_y.min(cy);
                max_y = max_y.max(cy);
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let nx = cx + dx;
                        let ny = cy + dy;
                        if nx < 0 || ny < 0 || nx >= w || ny >= h {
                            continue;
                        }
                        let ni = (ny * w + nx) as usize;
                        if label[ni] != c {
                            continue;
                        }
                        label[ni] = 0;
                        stack.push((nx, ny, c));
                    }
                }
            }
            let rect = Rect::new(min_x, min_y, max_x - min_x + 1, max_y - min_y + 1);
            // Border-shaped: a thin/sparse run of colour that contains at least one straight
            // section. A circular UI badge (an Edge profile avatar) or a filled icon has no long
            // horizontal/vertical run and is not a Planor border.
            let area = rect.area().max(1);
            let sparse = (pixels as i64) * 4 < area;
            let thin = rect.w.min(rect.h) <= 4;
            let runs = longest_runs(img, rect, colours[colour as usize - 1].1, tolerance);
            let corners = corner_hits(img, rect, colours[colour as usize - 1].1, tolerance);
            if pixels >= min_pixels
                && rect.w.max(rect.h) >= min_span
                && (thin || sparse)
                && runs.0.max(runs.1) >= min_span
                && (thin || corners >= 2)
            {
                out.push(ColourBox {
                    rect,
                    pixels,
                    colour: colour as usize - 1,
                });
            }
        }
    }
    out
}

/// A border box lies on an expected tile outline. A side segment (one short dimension) needs its
/// own side to match; a ring that spans the whole tile needs all four sides to match, so a ring
/// painted a few pixels off one edge is a failure even if its other edges line up.
pub fn on_any_outline(b: Rect, expected: &[Rect], tolerance: i32) -> bool {
    let is_ring = b.w.min(b.h) > 24;
    expected.iter().any(|e| {
        if is_ring {
            (b.x - e.x).abs() <= tolerance
                && (b.y - e.y).abs() <= tolerance
                && (b.right() - e.right()).abs() <= tolerance
                && (b.bottom() - e.bottom()).abs() <= tolerance
        } else {
            (b.x - e.x).abs() <= tolerance && b.y < e.bottom() && b.bottom() > e.y
                || (b.right() - e.right()).abs() <= tolerance
                    && b.y < e.bottom()
                    && b.bottom() > e.y
                || (b.y - e.y).abs() <= tolerance && b.x < e.right() && b.right() > e.x
                || (b.bottom() - e.bottom()).abs() <= tolerance
                    && b.x < e.right()
                    && b.right() > e.x
        }
    })
}

/// Fraction of the pixels of `colour` inside `bbox` that lie within `tolerance` of an expected
/// rectangle outline. This is the honest test for partial rings: a border fragment whose one side
/// is painted a few pixels off is not "on the outline" even when its other sides match.
pub fn outline_coverage(
    img: &Image,
    bbox: Rect,
    colour: [u8; 3],
    colour_tolerance: i32,
    expected: &[Rect],
    tolerance: i32,
) -> f32 {
    let r = bbox.clamp_to(Rect::new(0, 0, img.w, img.h));
    let mut matched = 0u32;
    let mut on = 0u32;
    for y in r.y..r.bottom() {
        for x in r.x..r.right() {
            if !near_colour(img.rgb(x, y), colour, colour_tolerance) {
                continue;
            }
            matched += 1;
            let mut nearest = i32::MAX;
            for e in expected {
                if y >= e.y - tolerance && y <= e.bottom() + tolerance {
                    nearest = nearest.min((x - e.x).abs()).min((x - e.right()).abs());
                }
                if x >= e.x - tolerance && x <= e.right() + tolerance {
                    nearest = nearest.min((y - e.y).abs()).min((y - e.bottom()).abs());
                }
            }
            if nearest <= tolerance {
                on += 1;
            }
        }
    }
    if matched == 0 {
        0.0
    } else {
        on as f32 / matched as f32
    }
}

/// Checks that a detected border box is painted where a Planor tile border may be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BorderVerdict {
    OnOutline,
    NotOnOutline,
    OverTaskbar,
    OutsideWorkArea,
}

pub fn check_border_box(
    b: Rect,
    expected: &[Rect],
    work: Rect,
    taskbars: &[Rect],
    tolerance: i32,
) -> BorderVerdict {
    if taskbars.iter().any(|t| b.overlap_area(*t) > 0) {
        return BorderVerdict::OverTaskbar;
    }
    if b.x < work.x || b.y < work.y || b.right() > work.right() || b.bottom() > work.bottom() {
        return BorderVerdict::OutsideWorkArea;
    }
    if on_any_outline(b, expected, tolerance) {
        BorderVerdict::OnOutline
    } else {
        BorderVerdict::NotOnOutline
    }
}

/// How many of the four sides of `rect` carry a straight run of `colour` along most of their length
/// (a band of up to 3 px just inside the side; the first and last 4 px are ignored so rounded corners
/// do not matter). A real Planor border is a closed rectangle: all four sides. App content in a border
/// colour (an Explorer selected row's two light-blue lines, an underline, a focus ring on one edge)
/// shows one or two sides at most.
pub fn sides_present(img: &Image, rect: Rect, colour: [u8; 3], tolerance: i32) -> u32 {
    let band = 3;
    let margin = 4;
    let coverage_needed = 0.70;
    let hit = |x: i32, y: i32| img.in_bounds(x, y) && near_colour(img.rgb(x, y), colour, tolerance);
    let horizontal = |y0: i32| -> bool {
        let span = rect.w - 2 * margin;
        if span <= 0 {
            return false;
        }
        let covered = (rect.x + margin..rect.right() - margin)
            .filter(|&x| (y0..y0 + band).any(|y| hit(x, y)))
            .count();
        covered as f32 / span as f32 >= coverage_needed
    };
    let vertical = |x0: i32| -> bool {
        let span = rect.h - 2 * margin;
        if span <= 0 {
            return false;
        }
        let covered = (rect.y + margin..rect.bottom() - margin)
            .filter(|&y| (x0..x0 + band).any(|x| hit(x, y)))
            .count();
        covered as f32 / span as f32 >= coverage_needed
    };
    [
        horizontal(rect.y),
        horizontal(rect.bottom() - band),
        vertical(rect.x),
        vertical(rect.right() - band),
    ]
    .into_iter()
    .filter(|&b| b)
    .count() as u32
}

/// Judge one detected border-coloured box. Returns the geometry verdict and whether it is a *stray
/// border* worth a failure:
/// - over the taskbar or outside the work area: always a stray border;
/// - on a tile outline (at least 90% of its colour pixels within tolerance): fine;
/// - anywhere else (inside a tile, off every outline): only when the box is not a thin line **and**
///   at least 3 of its 4 sides are present, because app content in these colours is common.
#[allow(clippy::too_many_arguments)]
pub fn judge_border_box(
    img: &Image,
    b: &ColourBox,
    colour: [u8; 3],
    expected: &[Rect],
    work: Rect,
    taskbars: &[Rect],
    border_tolerance_px: i32,
    colour_tolerance: i32,
) -> (BorderVerdict, bool) {
    let mut geometry = check_border_box(b.rect, expected, work, taskbars, border_tolerance_px);
    if matches!(
        geometry,
        BorderVerdict::OnOutline | BorderVerdict::NotOnOutline
    ) {
        let coverage = outline_coverage(
            img,
            b.rect,
            colour,
            colour_tolerance,
            expected,
            border_tolerance_px,
        );
        geometry = if coverage >= 0.9 {
            BorderVerdict::OnOutline
        } else {
            BorderVerdict::NotOnOutline
        };
    }
    let thin = b.rect.w.min(b.rect.h) < 12;
    let stray = match geometry {
        BorderVerdict::OnOutline => false,
        BorderVerdict::OverTaskbar | BorderVerdict::OutsideWorkArea => true,
        BorderVerdict::NotOnOutline => {
            !thin && sides_present(img, b.rect, colour, colour_tolerance) >= 3
        }
    };
    (geometry, stray)
}

// ---- PNG export --------------------------------------------------------------------------------

/// Read an 8-bit RGB or RGBA PNG (the files `write_png` produces).
pub fn read_png(path: &std::path::Path) -> std::io::Result<Image> {
    let file = std::fs::File::open(path)?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| std::io::Error::other("png too large"))?;
    let mut buf = vec![0u8; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err(std::io::Error::other("only 8-bit PNGs are supported"));
    }
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        other => {
            return Err(std::io::Error::other(format!(
                "unsupported PNG colour type {other:?}"
            )));
        }
    };
    let (w, h) = (info.width as i32, info.height as i32);
    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
    for px in buf[..info.buffer_size()].chunks_exact(channels) {
        bgra.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    Ok(Image::new(w, h, bgra))
}

/// Write an image as an 8-bit RGB PNG at `path`.
pub fn write_png(path: &std::path::Path, img: &Image) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::File::create(path)?;
    let mut rgb = Vec::with_capacity((img.w.max(0) * img.h.max(0) * 3) as usize);
    for px in img.bgra.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), img.w as u32, img.h as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    writer
        .write_image_data(&rgb)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BORDER_COLOURS;

    const WALL: [u8; 3] = [0, 0, 90];

    fn scene() -> (Image, Rect) {
        let mut img = Image::solid(240, 180, WALL);
        let win = Rect::new(60, 40, 90, 70);
        img.fill(win, [220, 220, 220]);
        img.outline(win, [59, 130, 246], 2);
        (img, win)
    }

    #[test]
    fn fits_a_window_outline_exactly() {
        let (img, win) = scene();
        let ctx = LayoutContext::for_image(&img);
        let expected = Rect::new(win.x + 7, win.y - 5, win.w, win.h);
        let fit = fit_outline(&img, expected, 24, 0.10, &ctx);
        assert_eq!(fit.measured, Some(win), "{fit:?}");
        assert_eq!(fit.offset, (-7, 5));
        assert!(fit.score > 0.4, "score {}", fit.score);
    }

    #[test]
    fn a_monitor_wide_window_is_measured_at_the_monitor_edges() {
        // Every side is flush; the pixels just inside the edge are window, so the position is the
        // monitor edge. This is a measurement, not an unknown.
        let mut img = Image::solid(320, 240, [0, 0, 90]);
        let win = Rect::new(0, 0, 320, 240);
        img.fill(win, [200, 200, 200]);
        img.outline(win, [59, 130, 246], 2);
        let ctx = LayoutContext::for_image(&img);
        let fit = fit_outline(&img, win, 24, 0.10, &ctx);
        assert_eq!(fit.measured, Some(win), "score {fit:?}");
    }

    #[test]
    fn fits_a_window_whose_size_differs_from_the_expectation() {
        let mut img = Image::solid(320, 240, [0, 0, 90]);
        let win = Rect::new(60, 50, 120, 80);
        img.fill(win, [200, 200, 200]);
        img.outline(win, [59, 130, 246], 2);
        let expected = Rect::new(60, 50, 100, 70);
        let ctx = LayoutContext::for_image(&img);
        let fit = fit_outline(&img, expected, 24, 0.10, &ctx);
        assert_eq!(fit.measured, Some(win), "score {fit:?}");
    }

    #[test]
    fn reports_unmeasurable_on_a_flat_frame() {
        let img = Image::solid(240, 180, WALL);
        let ctx = LayoutContext::for_image(&img);
        let fit = fit_outline(&img, Rect::new(60, 40, 90, 70), 24, 0.10, &ctx);
        assert_eq!(fit.measured, None);
        assert!(fit.score < 0.10);
    }

    #[test]
    fn measures_tiles_separated_by_a_six_px_wallpaper_gap() {
        // Two flush tiles with the 6 px Planor gap between them. The gap is the wallpaper strip;
        // each tile's side is the near boundary of that strip, never the neighbour's far edge.
        let mut img = Image::solid(320, 200, WALL);
        let left = Rect::new(0, 0, 150, 200);
        let right = Rect::new(156, 0, 164, 200);
        img.fill(left, [24, 24, 24]);
        img.fill(right, [30, 30, 30]);
        let ctx = LayoutContext::for_image(&img);
        let fl = fit_outline(&img, left, 24, 0.10, &ctx);
        assert_eq!(fl.measured, Some(left), "{fl:?}");
        assert!(
            fl.notes.iter().any(|n| n.contains("gap 6 px")),
            "{:?}",
            fl.notes
        );
        let fr = fit_outline(&img, right, 24, 0.10, &ctx);
        assert_eq!(fr.measured, Some(right), "{fr:?}");
        assert!(
            fr.notes.iter().any(|n| n.contains("gap 6 px")),
            "{:?}",
            fr.notes
        );
    }

    #[test]
    fn measures_tiles_next_to_a_darkened_shadow_gradient() {
        // The same layout, but the gap wallpaper is darkened towards both windows (shadow). Blue
        // dominance survives, so the strip and its two boundaries are still found.
        let mut img = Image::solid(320, 200, WALL);
        let left = Rect::new(0, 0, 150, 200);
        let right = Rect::new(156, 0, 164, 200);
        img.fill(left, [24, 24, 24]);
        img.fill(right, [30, 30, 30]);
        for x in 150..156 {
            let edge_distance = if x < 153 { x - 150 } else { 155 - x };
            let rg = (16 - edge_distance * 5).max(0) as u8;
            let b = (44 + edge_distance * 6) as u8;
            img.fill(Rect::new(x, 0, 1, 200), [rg, rg, b]);
        }
        let ctx = LayoutContext::for_image(&img);
        let fl = fit_outline(&img, left, 24, 0.10, &ctx);
        assert_eq!(fl.measured, Some(left), "{fl:?}");
        let fr = fit_outline(&img, right, 24, 0.10, &ctx);
        assert_eq!(fr.measured, Some(right), "{fr:?}");
    }

    #[test]
    fn a_window_shifted_by_five_px_is_measured_shifted() {
        // The expected left tile is at x=0, but the real window starts at x=5 (5 px off). The
        // monitor-edge flush check must fail and the wallpaper boundary must measure x=5, so the
        // position check sees a 5 px difference instead of a false pass.
        let mut img = Image::solid(320, 200, WALL);
        let actual = Rect::new(5, 0, 150, 200);
        let right = Rect::new(161, 0, 159, 200);
        img.fill(actual, [24, 24, 24]);
        img.fill(right, [30, 30, 30]);
        let expected = Rect::new(0, 0, 150, 200);
        let ctx = LayoutContext::for_image(&img);
        let fit = fit_outline(&img, expected, 24, 0.10, &ctx);
        let measured = fit.measured.expect("the shifted window is measurable");
        assert_eq!(measured, actual, "{fit:?}");
        assert_eq!(measured.max_side_diff(expected), 5, "{fit:?}");
    }

    #[test]
    fn measures_tiles_whose_gap_is_shadowed_to_pure_black() {
        // Real recording: tile edge at 957, a focused border just inside it, and a 6 px gap that is
        // RGB(0,0,0) (shadow), then the neighbour. The side is the near boundary of the gap, not the
        // neighbour's far edge 7 px away.
        let mut img = Image::solid(320, 200, [0, 0, 0]);
        let left = Rect::new(0, 0, 150, 200);
        let right = Rect::new(156, 0, 164, 200);
        img.fill(left, [254, 0, 253]);
        img.fill(right, [0, 224, 223]);
        img.outline(left, [59, 130, 246], 3);
        let ctx = LayoutContext::for_image(&img);
        let fl = fit_outline(&img, left, 24, 0.10, &ctx);
        assert_eq!(fl.measured, Some(left), "{fl:?}");
        let fr = fit_outline(&img, right, 24, 0.10, &ctx);
        assert_eq!(fr.measured, Some(right), "{fr:?}");
        // The classifier itself: black is shadowed wallpaper, near-black content is not.
        assert!(is_wallpaper_rgb([0, 0, 0]));
        assert!(is_wallpaper_rgb([1, 1, 2]));
        assert!(!is_wallpaper_rgb([10, 10, 10]));
    }

    #[test]
    fn measures_a_dark_window_on_equal_luma_wallpaper() {
        // The wallpaper is RGB(0,0,90), luma ~10.3. The window is RGB(10,10,10), luma 10. A luma
        // edge would see nothing; the blue-dominance classifier still sees window vs wallpaper.
        let mut img = Image::solid(320, 240, WALL);
        let win = Rect::new(40, 30, 100, 80);
        img.fill(win, [10, 10, 10]);
        let ctx = LayoutContext::for_image(&img);
        let fit = fit_outline(&img, win, 24, 0.10, &ctx);
        assert_eq!(fit.measured, Some(win), "{fit:?}");
    }

    #[test]
    fn a_window_flush_to_the_taskbar_uses_the_known_taskbar_edge() {
        let mut img = Image::solid(320, 240, WALL);
        let win = Rect::new(20, 20, 280, 180); // bottom at y=200
        img.fill(win, [24, 24, 24]);
        // Planor's border is drawn inside the last two pixels of the tile.
        img.fill(Rect::new(20, 198, 280, 2), [59, 130, 246]);
        let taskbar = Rect::new(0, 200, 320, 40);
        let ctx = LayoutContext::new(Rect::new(0, 0, 320, 200), vec![taskbar], 6);
        let fit = fit_outline(&img, win, 24, 0.10, &ctx);
        assert_eq!(fit.measured, Some(win), "{fit:?}");
        assert!(
            fit.notes.iter().any(|n| n.contains("taskbar edge")),
            "{:?}",
            fit.notes
        );
    }

    #[test]
    fn finds_border_colour_on_the_outline() {
        let (img, win) = scene();
        let boxes = find_border_boxes(&img, &BORDER_COLOURS, 12, 30, 8);
        assert!(!boxes.is_empty());
        let e = [win];
        assert!(boxes.iter().all(|b| check_border_box(
            b.rect,
            &e,
            Rect::new(0, 0, 240, 180),
            &[],
            2
        ) == BorderVerdict::OnOutline));
    }

    #[test]
    fn an_icon_ring_is_not_a_border() {
        // A light-blue circular badge (like an Edge profile avatar): sparse, but it has no long
        // straight run, so it must not be reported as a Planor border.
        let mut img = Image::solid(240, 180, WALL);
        let (cx, cy, r) = (120.0f32, 90.0f32, 22.0f32);
        for y in 0..180 {
            for x in 0..240 {
                let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
                if (d - r).abs() <= 2.0 {
                    img.fill(Rect::new(x, y, 1, 1), [0x93, 0xC5, 0xFD]);
                }
            }
        }
        let boxes = find_border_boxes(&img, &BORDER_COLOURS, 12, 20, 24);
        assert!(boxes.is_empty(), "{boxes:?}");
        // A straight border line of the same colour is still found.
        img.fill(Rect::new(20, 160, 120, 3), [0x93, 0xC5, 0xFD]);
        let boxes = find_border_boxes(&img, &BORDER_COLOURS, 12, 20, 24);
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].rect, Rect::new(20, 160, 120, 3));
    }

    #[test]
    fn border_over_taskbar_and_off_outline_are_flagged() {
        let mut img = Image::solid(240, 180, WALL);
        // A horizontal strip of focused-border blue inside the taskbar area.
        img.fill(Rect::new(20, 160, 120, 3), [59, 130, 246]);
        let boxes = find_border_boxes(&img, &BORDER_COLOURS, 12, 30, 8);
        assert!(!boxes.is_empty());
        let b = boxes[0].rect;
        let taskbar = [Rect::new(0, 158, 240, 22)];
        assert_eq!(
            check_border_box(b, &[], Rect::new(0, 0, 240, 180), &taskbar, 2),
            BorderVerdict::OverTaskbar
        );
        // Same box away from the taskbar is not on any expected outline.
        assert_eq!(
            check_border_box(
                b,
                &[Rect::new(0, 0, 100, 100)],
                Rect::new(0, 0, 240, 180),
                &[],
                2
            ),
            BorderVerdict::NotOnOutline
        );
    }

    #[test]
    fn a_ring_off_by_more_than_tolerance_on_one_side_fails() {
        // A stale border ring around a tile whose right edge is 6 px out: even though left, top and
        // bottom match, the ring as a whole is not on the expected outline.
        let mut img = Image::solid(240, 180, WALL);
        let expected = Rect::new(20, 20, 100, 80);
        img.outline(Rect::new(20, 20, 106, 80), [59, 130, 246], 2);
        let boxes = find_border_boxes(&img, &BORDER_COLOURS, 12, 30, 24);
        assert!(!boxes.is_empty());
        let coverage = outline_coverage(&img, boxes[0].rect, [59, 130, 246], 12, &[expected], 2);
        assert!(coverage < 0.9, "coverage {coverage}");
        // The same ring exactly on the outline is fully covered.
        let mut ok = Image::solid(240, 180, WALL);
        ok.outline(expected, [59, 130, 246], 2);
        let boxes = find_border_boxes(&ok, &BORDER_COLOURS, 12, 30, 24);
        let coverage = outline_coverage(&ok, boxes[0].rect, [59, 130, 246], 12, &[expected], 2);
        assert!(coverage >= 0.99, "coverage {coverage}");
    }
    #[test]
    fn classifies_black_flat_and_ok_interiors() {
        let bg = 8.0;
        let mut img = Image::solid(200, 150, WALL);
        let rect = Rect::new(40, 30, 100, 80);
        img.fill(rect, [0, 0, 0]);
        assert_eq!(classify_interior(&img, rect, bg).0, Interior::Black);
        img.fill(rect, [120, 120, 120]);
        assert_eq!(classify_interior(&img, rect, bg).0, Interior::Flat);
        // Textured interior.
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                let v = if (x / 7 + y / 5) % 2 == 0 { 40 } else { 200 };
                img.fill(Rect::new(x, y, 1, 1), [v, v, v]);
            }
        }
        assert_eq!(classify_interior(&img, rect, bg).0, Interior::Ok);
        // Same as background: unmeasurable, never a pass.
        img.fill(rect, WALL);
        let bg = surrounding_luminance(&img, rect);
        assert_eq!(classify_interior(&img, rect, bg).0, Interior::Unmeasurable);
    }

    #[test]
    fn detects_static_frames() {
        let a = summarize(&Image::solid(320, 180, WALL));
        let b = summarize(&Image::solid(320, 180, WALL));
        assert!(is_static(&a, &b));
        let mut c = Image::solid(320, 180, WALL);
        c.fill(Rect::new(20, 20, 100, 100), [200, 200, 200]);
        assert!(!is_static(&a, &summarize(&c)));
        // A small text-sized change must still count as a change (a frozen monitor check must not
        // miss a line of text appearing in an overlay).
        let mut d = Image::solid(320, 180, WALL);
        d.fill(Rect::new(40, 80, 40, 10), [230, 230, 230]);
        assert!(!is_static(&a, &summarize(&d)));
    }

    #[test]
    fn crop_and_scale_keep_size_relationships() {
        let (img, win) = scene();
        let c = img.crop(win.expand(10));
        assert_eq!((c.w, c.h), (win.w + 20, win.h + 20));
        let s = img.scaled_to_width(80);
        assert_eq!(s.w, 80);
        assert_eq!(s.h, 60);
    }
}

#[cfg(test)]
mod border_lookalike_tests {
    use super::*;
    use crate::config::BORDER_COLOURS;

    const WALL: [u8; 3] = [0, 0, 90];
    const FOCUSED: [u8; 3] = [0x3B, 0x82, 0xF6];
    const LIGHT_BLUE: [u8; 3] = [0x93, 0xC5, 0xFD];

    /// A grey tile on the wallpaper, as Planor draws a managed window.
    fn tile_scene() -> (Image, Rect) {
        let mut img = Image::solid(420, 240, WALL);
        let tile = Rect::new(20, 20, 380, 200);
        img.fill(tile, [60, 60, 60]);
        (img, tile)
    }

    /// Judge every border-coloured box in the frame the way the analyzer does; returns
    /// (number of boxes found, number reported as stray borders).
    fn judge_all(img: &Image, tile: Rect) -> (usize, usize) {
        let boxes = find_border_boxes(img, &BORDER_COLOURS, 40, 40, 24);
        let work = Rect::new(0, 0, img.w, img.h);
        let stray = boxes
            .iter()
            .filter(|b| {
                judge_border_box(
                    img,
                    b,
                    BORDER_COLOURS[b.colour].1,
                    &[tile],
                    work,
                    &[],
                    2,
                    40,
                )
                .1
            })
            .count();
        (boxes.len(), stray)
    }

    #[test]
    fn an_explorer_like_selected_row_is_not_a_stray_border() {
        // Two 2 px light-blue lines (the top and bottom of a selected row) inside the tile.
        let (mut img, tile) = tile_scene();
        img.fill(Rect::new(60, 80, 300, 2), LIGHT_BLUE);
        img.fill(Rect::new(60, 108, 300, 2), LIGHT_BLUE);
        let (found, stray) = judge_all(&img, tile);
        assert!(found >= 1, "the lines are detected as colour boxes");
        assert_eq!(stray, 0, "thin lines must not be reported");
    }

    #[test]
    fn a_two_sided_corner_shape_of_a_border_colour_is_not_a_stray_border() {
        // A left accent bar joined to a bottom line (an L): one connected, non-thin component with only
        // two sides. App content, not a Planor border.
        let (mut img, tile) = tile_scene();
        let row = Rect::new(60, 80, 300, 40);
        img.fill(Rect::new(row.x, row.y, 2, row.h), LIGHT_BLUE);
        img.fill(Rect::new(row.x, row.bottom() - 2, row.w, 2), LIGHT_BLUE);
        let boxes = find_border_boxes(&img, &BORDER_COLOURS, 40, 40, 24);
        assert_eq!(boxes.len(), 1, "one connected L shaped box: {boxes:?}");
        assert!(
            boxes[0].rect.w.min(boxes[0].rect.h) >= 12,
            "not thin, so the 3-side rule decides"
        );
        assert_eq!(sides_present(&img, boxes[0].rect, LIGHT_BLUE, 40), 2);
        let (_, stray) = judge_all(&img, tile);
        assert_eq!(stray, 0);
    }

    #[test]
    fn a_selection_row_with_a_light_fill_and_two_lines_is_not_a_stray_border() {
        // The Explorer look: pale fill (not a Planor colour) with a 2 px light-blue line top and bottom.
        let (mut img, tile) = tile_scene();
        let row = Rect::new(60, 80, 300, 30);
        img.fill(row, [229, 243, 255]);
        img.fill(Rect::new(row.x, row.y, row.w, 2), LIGHT_BLUE);
        img.fill(Rect::new(row.x, row.bottom() - 2, row.w, 2), LIGHT_BLUE);
        let (_, stray) = judge_all(&img, tile);
        assert_eq!(stray, 0);
    }

    #[test]
    fn a_real_stray_border_inside_a_tile_still_fails() {
        // A closed 2 px ring in Planor's focused colour that is not on the tile outline.
        let (mut img, tile) = tile_scene();
        let ring = Rect::new(100, 60, 160, 100);
        img.outline(ring, FOCUSED, 2);
        let boxes = find_border_boxes(&img, &BORDER_COLOURS, 40, 40, 24);
        assert_eq!(boxes.len(), 1);
        assert_eq!(sides_present(&img, ring, FOCUSED, 40), 4);
        let (found, stray) = judge_all(&img, tile);
        assert_eq!((found, stray), (1, 1));
    }

    #[test]
    fn a_ring_with_one_side_missing_still_fails_but_two_sides_do_not() {
        let (mut img, tile) = tile_scene();
        let ring = Rect::new(100, 60, 160, 100);
        img.outline(ring, FOCUSED, 2);
        // Erase the right side: three sides remain.
        img.fill(Rect::new(ring.right() - 2, ring.y, 2, ring.h), [60, 60, 60]);
        assert_eq!(sides_present(&img, ring, FOCUSED, 40), 3);
        // The remaining shape is still a U: reported.
        let (_, stray) = judge_all(&img, tile);
        assert_eq!(stray, 1);
    }

    #[test]
    fn the_real_tile_border_on_its_outline_is_not_stray() {
        let (mut img, tile) = tile_scene();
        img.outline(tile, FOCUSED, 2);
        let (found, stray) = judge_all(&img, tile);
        assert_eq!((found, stray), (1, 0));
    }

    #[test]
    fn a_thin_line_over_the_taskbar_still_fails() {
        let (mut img, tile) = tile_scene();
        img.fill(Rect::new(60, 228, 100, 2), FOCUSED);
        let boxes = find_border_boxes(&img, &BORDER_COLOURS, 40, 40, 24);
        assert_eq!(boxes.len(), 1);
        let work = Rect::new(0, 0, 420, 224);
        let taskbar = Rect::new(0, 224, 420, 16);
        let (verdict, stray) =
            judge_border_box(&img, &boxes[0], FOCUSED, &[tile], work, &[taskbar], 2, 40);
        assert_eq!(verdict, BorderVerdict::OverTaskbar);
        assert!(stray);
    }
}
#[cfg(test)]
mod out_of_frame_tests {
    use super::*;

    #[test]
    fn pixel_reads_outside_the_frame_clamp_instead_of_panicking() {
        let img = Image::solid(40, 30, [0, 0, 90]);
        assert_eq!(img.rgb(-10, 5), [0, 0, 90]);
        assert_eq!(img.rgb(5, 9999), [0, 0, 90]);
        assert_eq!(img.rgb(9999, 9999), [0, 0, 90]);
        assert!(img.lum(-1, -1) > 0.0);
    }

    #[test]
    fn fitting_a_tile_that_hangs_off_the_monitor_does_not_panic() {
        // A scrolled strip: the tile target is mostly past the right and bottom edge of the frame.
        let mut img = Image::solid(200, 120, [0, 0, 90]);
        img.fill(Rect::new(150, 70, 50, 50), [200, 200, 200]);
        let ctx = LayoutContext::for_image(&img);
        for target in [
            Rect::new(150, 70, 300, 200),
            Rect::new(-250, -100, 300, 200),
            Rect::new(190, 110, 400, 400),
        ] {
            let _ = fit_outline(&img, target, 24, 0.10, &ctx);
        }
    }
}

pub mod replay;

pub type WindowId = isize;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}
impl Rect {
    pub fn horizontal(self) -> bool {
        self.w >= self.h
    }
    pub fn length(self) -> i32 {
        if self.horizontal() { self.w } else { self.h }
    }
    pub fn project(self, start: i32, length: i32, cross: i32, depth: i32) -> Self {
        if self.horizontal() {
            Self {
                x: self.x + start,
                y: self.y + cross,
                w: length,
                h: depth,
            }
        } else {
            Self {
                x: self.x + cross,
                y: self.y + start,
                w: depth,
                h: length,
            }
        }
    }
}
impl Column {
    /// Position of member `j` relative to the deck position (`active + frac`), wrapped into (-n/2, n/2].
    pub fn relative(&self, j: usize) -> f64 {
        let n = self.members.len() as f64;
        let mut r = j as f64 - (self.active as f64 + self.frac);
        while r > n / 2. {
            r -= n
        }
        while r <= -n / 2. {
            r += n
        }
        r
    }
    /// True for the active member and its visible neighbours (next; also previous with 3+ members); the
    /// rest are hidden behind the next peek.
    pub fn shown(&self, j: usize) -> bool {
        let n = self.members.len();
        let r = self.relative(j);
        r.abs() < 0.5 || (0.5..=1.5).contains(&r) || (n >= 3 && (-1.5..=-0.5).contains(&r))
    }
}
#[derive(Clone, Debug)]
pub struct Column {
    pub members: Vec<WindowId>,
    pub active: usize,
    pub length: i32,
    minimum: i32,
    prior: Option<i32>,
    /// Fractional scroll through the members while a touchpad roll is in progress, in (-1, 1): the deck
    /// is drawn at position `active + frac`. Zero when settled.
    pub frac: f64,
}
impl Column {
    pub fn minimum(&self) -> i32 {
        self.minimum
    }
}
#[derive(Debug, Clone)]
pub struct Strip {
    pub area: Rect,
    pub gap: i32,
    pub share: f64,
    pub split: bool,
    pub columns: Vec<Column>,
    pub focus: usize,
    pub offset: i32,
    fullscreen: Option<(WindowId, usize, i32)>,
}
impl Strip {
    pub fn new(area: Rect) -> Self {
        Self {
            area,
            gap: 6,
            share: 0.5,
            split: true,
            columns: vec![],
            focus: 0,
            offset: 0,
            fullscreen: None,
        }
    }
    pub fn active(&self) -> Option<WindowId> {
        self.columns.get(self.focus).map(|c| c.members[c.active])
    }
    pub fn locate(&self, id: WindowId) -> Option<(usize, usize)> {
        self.columns
            .iter()
            .enumerate()
            .find_map(|(i, c)| c.members.iter().position(|&m| m == id).map(|j| (i, j)))
    }
    pub fn select(&mut self, id: WindowId) {
        if let Some((i, j)) = self.locate(id) {
            self.focus = i;
            self.columns[i].active = j
        }
    }
    pub fn insert(&mut self, id: WindowId, minimum: i32) {
        if self.locate(id).is_some() {
            return;
        }
        let span = self.area.length();
        let usable = (span - self.gap).max(1);
        let minimum = minimum.max(1);
        let length = match self.columns.as_mut_slice() {
            [] => span,
            [c] if self.split && c.length == span => {
                c.length = (usable / 2).min(usable - minimum).max(c.minimum);
                usable - c.length
            }
            _ => (usable as f64 * self.share).round() as i32,
        }
        .max(minimum);
        self.focus = if self.columns.is_empty() {
            0
        } else {
            self.focus + 1
        };
        self.columns.insert(
            self.focus,
            Column {
                members: vec![id],
                active: 0,
                length,
                minimum,
                prior: None,
                frac: 0.,
            },
        )
    }
    pub fn remove(&mut self, id: WindowId) {
        if let Some((i, j)) = self.locate(id) {
            let c = &mut self.columns[i];
            c.members.remove(j);
            if c.members.is_empty() {
                self.columns.remove(i);
                if i < self.focus {
                    self.focus -= 1
                }
                self.focus = self.focus.min(self.columns.len().saturating_sub(1))
            } else {
                if j < c.active {
                    c.active -= 1
                }
                c.active = c.active.min(c.members.len() - 1)
            }
            self.scroll(0)
        }
    }
    /// A window left alone on its monitor (its neighbour closed or moved away) fills it; it was
    /// only half of a pair. Not part of `remove`, which reorder and group reuse.
    pub fn fill(&mut self) {
        let span = self.area.length();
        if let [c] = self.columns.as_mut_slice() {
            c.length = span.max(c.minimum)
        }
    }
    /// The work area changed (a taskbar appeared or went, a display came back). The columns keep their
    /// share of the strip: they are rescaled to the new length, the last one absorbing the rounding so they
    /// fill it exactly. A column left longer than the area is clipped by the renderer and parked off-screen.
    pub fn set_area(&mut self, area: Rect) {
        let old = self.area.length();
        self.area = area;
        let n = self.columns.len();
        if n > 0 && old != area.length() && self.fullscreen.is_none() {
            let usable = area.length() - self.gap * (n as i32 - 1);
            let total: i32 = self.columns.iter().map(|c| c.length).sum();
            let ratio = usable as f64 / total.max(1) as f64;
            let mut used = 0;
            for c in &mut self.columns[..n - 1] {
                c.length = ((c.length as f64 * ratio).round() as i32).max(c.minimum);
                used += c.length;
            }
            let last = &mut self.columns[n - 1];
            last.length = (usable - used).max(last.minimum);
        }
        self.scroll(0)
    }
    pub fn group(&mut self, id: WindowId, target: WindowId) {
        if id == target || self.locate(id).is_none() || self.locate(target).is_none() {
            return;
        }
        self.remove(id);
        self.select(target);
        let single = self.columns.len() == 1;
        let c = &mut self.columns[self.focus];
        c.members.push(id);
        c.active = c.members.len() - 1;
        if single {
            c.length = self.area.length().max(c.minimum)
        }
    }
    pub fn ungroup(&mut self, id: WindowId) {
        if let Some((i, _)) = self.locate(id)
            && self.columns[i].members.len() > 1
        {
            self.focus = i;
            self.remove(id);
            self.insert(id, 1)
        }
    }
    pub fn navigate(&mut self, step: isize, cross: bool) {
        if self.columns.is_empty() {
            return;
        }
        if cross {
            let c = &mut self.columns[self.focus];
            c.active = (c.active as isize + step).rem_euclid(c.members.len() as isize) as usize
        } else {
            self.focus = self
                .focus
                .saturating_add_signed(step)
                .min(self.columns.len() - 1)
        }
    }
    /// Resize column `i` (`leading`: its leading edge was dragged). When the columns exactly fill the
    /// monitor, the neighbour on the dragged side takes the opposite change so a pair keeps filling it
    /// (a 50/50 pair stays a pair); otherwise only that column changes and the strip scrolls.
    pub fn resize(&mut self, i: usize, length: i32, leading: bool) {
        if (leading && i == 0) || i >= self.columns.len() {
            return;
        }
        let span = self.area.length();
        let fits = self.columns.len() > 1 && self.start(self.columns.len()) - self.gap == span;
        let old = self.columns[i].length;
        let mut new = length.max(self.columns[i].minimum);
        if fits {
            let j = if leading {
                i - 1
            } else if i + 1 < self.columns.len() {
                i + 1
            } else {
                i - 1
            };
            let other = self.columns[j].length - (new - old);
            let floor = self.columns[j].minimum;
            if other < floor {
                new -= floor - other;
                self.columns[j].length = floor;
            } else {
                self.columns[j].length = other;
            }
        }
        self.columns[i].length = new;
        if leading && !fits {
            // The strip flows from the left, so a longer column would push its RIGHT edge out. Dragging the
            // leading edge must move only that edge: scroll by the change so the trailing edge stays put.
            self.offset = self.offset.saturating_add(new - old);
        }
        self.scroll(0);
    }
    /// Touchpad scrolling: focus follows the column nearest the middle of the monitor, except at the
    /// ends of the strip, where the first and last column are always reachable.
    pub fn focus_nearest_center(&mut self) {
        if self.columns.is_empty() {
            return;
        }
        let max = (self.start(self.columns.len()) - self.gap - self.area.length()).max(0);
        if self.offset <= 0 {
            self.focus = 0;
        } else if self.offset >= max {
            self.focus = self.columns.len() - 1;
        } else {
            let center = self.area.length() / 2;
            let mut best = i32::MAX;
            for i in 0..self.columns.len() {
                let d = (self.start(i) - self.offset + self.columns[i].length / 2 - center).abs();
                if d < best {
                    best = d;
                    self.focus = i;
                }
            }
        }
    }
    pub fn maximize(&mut self) {
        if let Some(c) = self.columns.get_mut(self.focus) {
            c.length = c.prior.take().unwrap_or_else(|| {
                c.prior = Some(c.length);
                self.area.length().max(c.minimum)
            })
        }
        self.scroll(0);
    }
    pub fn fullscreen(&mut self, enter: bool, bounds: Rect) {
        if enter && self.fullscreen.is_none() {
            if let Some(id) = self.active() {
                let i = self.focus;
                let mut c = self.columns.remove(i);
                self.fullscreen = Some((id, i, c.length));
                c.length = if self.area.horizontal() {
                    bounds.w
                } else {
                    bounds.h
                };
                self.focus = self.columns.len();
                self.columns.push(c)
            }
        } else if !enter
            && let Some((id, i, length)) = self.fullscreen.take()
        {
            if let Some((j, _)) = self.locate(id) {
                let mut c = self.columns.remove(j);
                c.length = length;
                self.focus = i.min(self.columns.len());
                self.columns.insert(self.focus, c);
            }
            self.scroll(0);
        }
    }
    pub fn fullscreen_id(&self) -> Option<WindowId> {
        self.fullscreen.map(|f| f.0)
    }
    pub fn reorder(&mut self, id: WindowId, target: WindowId, after: bool) {
        let Some((from, _)) = self.locate(id) else {
            return;
        };
        if id == target || self.locate(target).is_none() {
            return;
        }
        let length = self.columns[from].length;
        let minimum = self.columns[from].minimum;
        self.remove(id);
        let (to, _) = self.locate(target).unwrap();
        let pos = to + usize::from(after);
        self.columns.insert(
            pos,
            Column {
                members: vec![id],
                active: 0,
                length,
                minimum,
                prior: None,
                frac: 0.,
            },
        );
        self.focus = pos
    }
    fn start(&self, i: usize) -> i32 {
        self.columns[..i].iter().map(|c| c.length + self.gap).sum()
    }
    pub fn scroll(&mut self, delta: i32) {
        self.offset = self.offset.saturating_add(delta).clamp(
            0,
            (self.start(self.columns.len()) - self.gap - self.area.length()).max(0),
        )
    }
    /// True when the column holding `id` lies entirely inside the current viewport.
    pub fn in_view(&self, id: WindowId) -> bool {
        let Some((i, _)) = self.locate(id) else {
            return false;
        };
        let start = self.start(i);
        start >= self.offset && start + self.columns[i].length <= self.offset + self.area.length()
    }
    pub fn reveal(&mut self, peek: i32) {
        self.scroll(0);
        if let Some(c) = self.columns.get(self.focus) {
            let start = self.start(self.focus);
            let end = start + c.length;
            // Already fully on screen: focusing it must not slide the strip sideways.
            if start >= self.offset && end <= self.offset + self.area.length() {
                return;
            }
            let extra = peek.min((self.area.length() - c.length).max(0));
            let left = start - if self.focus > 0 { extra } else { 0 };
            let right = end
                + if self.focus + 1 < self.columns.len() {
                    extra
                } else {
                    0
                };
            if right > self.offset + self.area.length() {
                self.offset = right - self.area.length()
            } else if left < self.offset {
                self.offset = left
            }
            self.scroll(0)
        }
    }
    pub fn rects(&self, stack_peek: i32) -> Vec<(WindowId, Rect)> {
        let mut start = -self.offset;
        let mut out = vec![];
        let depth = if self.area.horizontal() {
            self.area.h
        } else {
            self.area.w
        };
        for c in &self.columns {
            let n = c.members.len();
            // A group is a deck of equally sized windows. The active one sits in the column (minus the
            // preview strips), the next one starts right below it and, with three or more members, the
            // previous one ends right above it, so only a `peek` strip of each shows, the rest lying beyond
            // the column and clipped away. Exactly like the windows peeking in from the sides of a strip:
            // nothing is squashed, and no member is ever resized when the deck rolls, it only slides.
            // "Depth" is the column height in a landscape strip and its width in a portrait one.
            let both = n >= 3;
            let peek = if n > 1 { stack_peek.clamp(0, depth / if both { 4 } else { 2 }) } else { 0 };
            let before = if both { peek } else { 0 };
            let size = depth - before - peek;
            for (j, &id) in c.members.iter().enumerate() {
                let r = if n > 1 { c.relative(j) } else { 0. };
                let cross = before as f64 + r * size as f64;
                out.push((id, self.area.project(start, c.length, cross.round() as i32, size)));
            }
            start += c.length + self.gap
        }
        out
    }
}
pub fn motion(t: f64, spring: bool) -> f64 {
    let t = t.clamp(0., 1.);
    if t == 1. {
        1.
    } else if spring {
        1. - (1. + 8. * t) * (-8. * t).exp()
    } else {
        1. - (1. - t).powi(3)
    }
}
#[derive(Debug, PartialEq)]
pub enum Drag {
    None,
    Before,
    After,
    Stack,
}
pub fn drag(
    fraction: f64,
    speed: f64,
    threshold: f64,
    dwell: u64,
    hover: u64,
    freeze: bool,
) -> Drag {
    if (0.25..=0.75).contains(&fraction) && dwell >= hover {
        Drag::Stack
    } else if freeze || speed.abs() > threshold {
        Drag::None
    } else if fraction < 0.25 {
        Drag::Before
    } else if fraction > 0.75 {
        Drag::After
    } else {
        Drag::None
    }
}
/// Multi-finger swipe recogniser. Three or four fingers moving together lock to one axis (after the
/// dead zone) and then emit one `Step` every `step` units of travel along it, so a long swipe walks
/// several windows. `fingers` tells the caller which gesture it is.
#[derive(Default)]
pub struct Gesture {
    origin: Option<(f64, f64)>,
    last: (f64, f64),
    horizontal: Option<bool>,
    fingers: usize,
    dragging: bool,
    scrolling: bool,
}
#[derive(Debug, PartialEq)]
pub enum Swipe {
    None,
    /// Three fingers moving sideways: continuous scroll by this much finger travel.
    Scroll(f64),
    /// Three fingers moving up or down: continuous roll through the focused group (or, with no group,
    /// hopping to the monitor above or below).
    ScrollV(f64),
    /// A scroll ended (fingers lifted): settle on the focused window.
    ScrollEnd,
    /// Four fingers moved past the dead zone: pick the focused window up.
    DragStart,
    /// Finger travel since the previous sample (free on both axes).
    DragMove(f64, f64),
    /// Fingers lifted (or their number changed): drop the window.
    DragEnd,
    /// `direction` is +1 for rightwards/downwards finger travel and -1 for leftwards/upwards.
    Step { fingers: usize, horizontal: bool, direction: isize },
}
impl Gesture {
    pub fn sample(&mut self, points: &[(f64, f64)], deadzone: f64, step: f64) -> Swipe {
        let n = points.len();
        let ended = |g: &Self| {
            if g.dragging {
                Swipe::DragEnd
            } else if g.scrolling {
                Swipe::ScrollEnd
            } else {
                Swipe::None
            }
        };
        if n != 3 && n != 4 {
            let end = ended(self);
            *self = Self::default();
            return end;
        }
        if self.fingers != n {
            // Finger count changed mid-gesture: start over with the new count.
            let end = ended(self);
            *self = Self { fingers: n, ..Self::default() };
            if end != Swipe::None {
                return end;
            }
        }
        let pos = (
            points.iter().map(|p| p.0).sum::<f64>() / n as f64,
            points.iter().map(|p| p.1).sum::<f64>() / n as f64,
        );
        let Some(origin) = self.origin else {
            self.origin = Some(pos);
            self.last = pos;
            return Swipe::None;
        };
        let dx = pos.0 - origin.0;
        let dy = pos.1 - origin.1;
        if n == 4 {
            if !self.dragging {
                if dx.hypot(dy) < deadzone {
                    return Swipe::None;
                }
                self.dragging = true;
                self.last = origin; // the first move includes the dead-zone travel
                return Swipe::DragStart;
            }
            let (mx, my) = (pos.0 - self.last.0, pos.1 - self.last.1);
            self.last = pos;
            return if mx == 0. && my == 0. { Swipe::None } else { Swipe::DragMove(mx, my) };
        }
        // Three fingers start scrolling almost at once (a small dead zone only to pick the axis).
        let deadzone = deadzone.min(3.);
        if self.horizontal.is_none() {
            if dx.abs().max(dy.abs()) < deadzone {
                return Swipe::None;
            }
            self.horizontal = Some(dx.abs() >= dy.abs());
            self.last = origin;
        }
        let horizontal = self.horizontal.unwrap_or(true);
        let delta = if horizontal { pos.0 - self.last.0 } else { pos.1 - self.last.1 };
        self.last = pos;
        if horizontal {
            // Sideways: scroll continuously, with the focus following the window nearest the centre.
            self.scrolling = true;
            return if delta == 0. { Swipe::None } else { Swipe::Scroll(delta) };
        }
        let _ = step;
        self.scrolling = true;
        if delta == 0. { Swipe::None } else { Swipe::ScrollV(delta) }
    }
}
pub fn adjacent(source: Rect, others: &[Rect], dx: i32, dy: i32) -> Option<usize> {
    others
        .iter()
        .enumerate()
        .filter_map(|(i, b)| {
            let (gap, a0, a1, b0, b1) = match (dx, dy) {
                (1, 0) => (
                    b.x - source.x - source.w,
                    source.y,
                    source.y + source.h,
                    b.y,
                    b.y + b.h,
                ),
                (-1, 0) => (
                    source.x - b.x - b.w,
                    source.y,
                    source.y + source.h,
                    b.y,
                    b.y + b.h,
                ),
                (0, 1) => (
                    b.y - source.y - source.h,
                    source.x,
                    source.x + source.w,
                    b.x,
                    b.x + b.w,
                ),
                (0, -1) => (
                    source.y - b.y - b.h,
                    source.x,
                    source.x + source.w,
                    b.x,
                    b.x + b.w,
                ),
                _ => return None,
            };
            if gap < 0 {
                return None;
            }
            let separation = (b0 - a1).max(a0 - b1).max(0);
            Some((
                (
                    separation > 0,
                    gap as i64 * gap as i64 + separation as i64 * separation as i64,
                    (a0 + a1 - b0 - b1).abs(),
                ),
                i,
            ))
        })
        .min()
        .map(|(_, i)| i)
}
#[derive(Default)]
pub struct ContactFrame {
    waiting: bool,
    target: usize,
    points: std::collections::BTreeMap<u32, (f64, f64)>,
}
impl ContactFrame {
    pub fn packet(
        &mut self,
        count: u32,
        contacts: impl IntoIterator<Item = (u32, (f64, f64))>,
    ) -> Option<Vec<(f64, f64)>> {
        if count != 0 {
            self.points.clear();
            self.waiting = count == 3 || count == 4;
            self.target = count as usize;
        }
        if !self.waiting {
            return Some(vec![]);
        }
        self.points.extend(contacts);
        if self.points.len() < self.target {
            return None;
        }
        self.waiting = false;
        Some(std::mem::take(&mut self.points).into_values().collect())
    }
}

impl Strip {
    /// Roll the focused group by `delta` members (touchpad scroll through a stack). Wraps around.
    pub fn roll(&mut self, delta: f64) -> bool {
        let Some(c) = self.columns.get_mut(self.focus) else {
            return false;
        };
        let n = c.members.len() as isize;
        if n < 2 {
            return false;
        }
        c.frac += delta;
        while c.frac >= 1. {
            c.frac -= 1.;
            c.active = (c.active as isize + 1).rem_euclid(n) as usize;
        }
        while c.frac <= -1. {
            c.frac += 1.;
            c.active = (c.active as isize - 1).rem_euclid(n) as usize;
        }
        true
    }
    /// End of a roll: the deck settles on the nearest member.
    pub fn settle_roll(&mut self) {
        for c in &mut self.columns {
            if c.frac != 0. {
                let n = c.members.len() as isize;
                let step = c.frac.round() as isize;
                c.active = (c.active as isize + step).rem_euclid(n.max(1)) as usize;
                c.frac = 0.;
            }
        }
    }
    /// No empty space on the monitor: when the columns do not reach the end of the viewport, the last
    /// one takes the rest. Not used while a column is fullscreen.
    pub fn fill_gaps(&mut self) {
        if self.fullscreen.is_some() || self.columns.is_empty() {
            return;
        }
        let end = self.start(self.columns.len()) - self.gap;
        let span = self.area.length();
        if end < span {
            let last = self.columns.len() - 1;
            self.columns[last].length += span - end;
            self.offset = 0;
        } else {
            self.scroll(0);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvariantViolation {
    pub name: &'static str,
    pub details: String,
}

impl InvariantViolation {
    pub fn trace_line(&self) -> String {
        format!("INVARIANT {} {}", self.name, self.details)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SettledTile {
    pub id: WindowId,
    pub rect: Rect,
    pub bound: Rect,
    pub expect_on_screen: bool,
    pub focused: bool,
}

fn rect_overlap(a: Rect, b: Rect) -> i64 {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    (w.max(0) as i64) * (h.max(0) as i64)
}

/// Pure invariant check over a settled `Strip` (section 3.2):
/// 1. `one_active_per_group` — every column has >=1 member, a single valid `active` index, valid `focus`, no duplicates.
/// 2. `no_gap` — non-empty non-fullscreen strip fills to the viewport end, positive lengths, exact `gap`, valid `offset`.
/// 3. `no_overlap` — active tiles across columns have zero overlap.
/// 4. `inside_monitor` — cross-axis spans stay inside `strip.area`, and any column that fits in the viewport and is in view stays inside `strip.area`.
pub fn check_strip_invariants(strip: &Strip, stack_peek: i32) -> Vec<InvariantViolation> {
    let mut out = Vec::new();
    if strip.columns.is_empty() {
        return out;
    }

    // 1. one_active_per_group
    if strip.focus >= strip.columns.len() {
        out.push(InvariantViolation {
            name: "one_active_per_group",
            details: format!("focus {} out of range for {} columns", strip.focus, strip.columns.len()),
        });
    }
    let mut seen_ids = std::collections::HashSet::new();
    for (ci, col) in strip.columns.iter().enumerate() {
        if col.members.is_empty() {
            out.push(InvariantViolation {
                name: "one_active_per_group",
                details: format!("column {ci} has no members"),
            });
            continue;
        }
        if col.active >= col.members.len() {
            out.push(InvariantViolation {
                name: "one_active_per_group",
                details: format!(
                    "column {ci} active index {} >= {} members",
                    col.active,
                    col.members.len()
                ),
            });
        }
        if !col.frac.is_finite() || col.frac.abs() >= 1.0 {
            out.push(InvariantViolation {
                name: "one_active_per_group",
                details: format!("column {ci} has invalid roll frac {}", col.frac),
            });
        }
        for &id in &col.members {
            if !seen_ids.insert(id) {
                out.push(InvariantViolation {
                    name: "one_active_per_group",
                    details: format!("duplicate window id {id} across strip columns"),
                });
            }
        }
    }

    // 2. no_gap
    let span = strip.area.length();
    let total_end = strip.start(strip.columns.len()) - strip.gap;
    let max_off = (total_end - span).max(0);
    if strip.offset < 0 || strip.offset > max_off {
        out.push(InvariantViolation {
            name: "no_gap",
            details: format!("offset {} outside [0, {}]", strip.offset, max_off),
        });
    }
    for (ci, col) in strip.columns.iter().enumerate() {
        if col.length <= 0 {
            out.push(InvariantViolation {
                name: "no_gap",
                details: format!("column {ci} has non-positive length {}", col.length),
            });
        }
    }
    if strip.fullscreen.is_none() && total_end < span {
        out.push(InvariantViolation {
            name: "no_gap",
            details: format!(
                "strip ends at {total_end} leaving {} px empty before monitor end {span}",
                span - total_end
            ),
        });
    }

    // Active rects for overlap & monitor bounds checks.
    let all_rects = strip.rects(stack_peek);
    let mut active_rects: Vec<(usize, WindowId, Rect)> = Vec::new();
    for (ci, col) in strip.columns.iter().enumerate() {
        if let Some(&id) = col.members.get(col.active)
            && let Some((_, r)) = all_rects.iter().find(|(wid, _)| *wid == id)
        {
            active_rects.push((ci, id, *r));
        }
    }

    // Check exact gap between consecutive active columns when not fullscreen.
    if strip.fullscreen.is_none() {
        for pair in active_rects.windows(2) {
            let (ci0, _, r0) = pair[0];
            let (ci1, _, r1) = pair[1];
            let observed = if strip.area.horizontal() {
                r1.x - (r0.x + r0.w)
            } else {
                r1.y - (r0.y + r0.h)
            };
            if observed != strip.gap {
                out.push(InvariantViolation {
                    name: "no_gap",
                    details: format!(
                        "gap between column {ci0} and {ci1} is {observed} px, expected {}",
                        strip.gap
                    ),
                });
            }
        }
    }

    // 3. no_overlap
    for a in 0..active_rects.len() {
        for b in (a + 1)..active_rects.len() {
            let area = rect_overlap(active_rects[a].2, active_rects[b].2);
            if area > 0 {
                out.push(InvariantViolation {
                    name: "no_overlap",
                    details: format!(
                        "columns {} (id={}) and {} (id={}) overlap by {area} px^2 ({:?} vs {:?})",
                        active_rects[a].0,
                        active_rects[a].1,
                        active_rects[b].0,
                        active_rects[b].1,
                        active_rects[a].2,
                        active_rects[b].2
                    ),
                });
            }
        }
    }

    // 4. inside_monitor
    if strip.fullscreen.is_none() {
        let mon = strip.area;
        for (ci, id, r) in &active_rects {
            if r.w <= 0 || r.h <= 0 {
                out.push(InvariantViolation {
                    name: "inside_monitor",
                    details: format!("column {ci} (id={id}) has non-positive size {r:?}"),
                });
                continue;
            }
            if mon.horizontal() {
                if r.y < mon.y || r.y + r.h > mon.y + mon.h {
                    out.push(InvariantViolation {
                        name: "inside_monitor",
                        details: format!(
                            "column {ci} (id={id}) cross-axis y={}..{} exceeds monitor work y={}..{}",
                            r.y,
                            r.y + r.h,
                            mon.y,
                            mon.y + mon.h
                        ),
                    });
                }
            } else if r.x < mon.x || r.x + r.w > mon.x + mon.w {
                out.push(InvariantViolation {
                    name: "inside_monitor",
                    details: format!(
                        "column {ci} (id={id}) cross-axis x={}..{} exceeds portrait work x={}..{}",
                        r.x,
                        r.x + r.w,
                        mon.x,
                        mon.x + mon.w
                    ),
                });
            }
            if strip.in_view(*id)
                && (r.x < mon.x
                    || r.y < mon.y
                    || r.x + r.w > mon.x + mon.w
                    || r.y + r.h > mon.y + mon.h)
            {
                out.push(InvariantViolation {
                    name: "inside_monitor",
                    details: format!(
                        "in-view tile {id} rect {r:?} lies outside monitor work area {mon:?}"
                    ),
                });
            }
        }
    }

    out
}

/// Check that no window expected on screen (or holding focus) remains at the off-screen park position after settle.
pub fn check_settled_parked(
    tiles: &[SettledTile],
    actuals: &[(WindowId, Rect)],
    park_x: i32,
) -> Vec<InvariantViolation> {
    let mut out = Vec::new();
    for t in tiles {
        let Some((_, actual)) = actuals.iter().find(|(id, _)| *id == t.id) else {
            continue;
        };
        if (t.expect_on_screen || t.focused) && actual.x >= park_x - 2 {
            out.push(InvariantViolation {
                name: "no_window_left_parked",
                details: format!(
                    "window {} left parked at x={} (park_x={park_x}) expected={:?} focused={}",
                    t.id, actual.x, t.rect, t.focused
                ),
            });
        }
    }
    out
}

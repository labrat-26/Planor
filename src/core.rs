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
    pub fn intersect(self, o: Rect) -> Option<Rect> {
        let (x, y) = (self.x.max(o.x), self.y.max(o.y));
        let (r, b) = (
            (self.x + self.w).min(o.x + o.w),
            (self.y + self.h).min(o.y + o.h),
        );
        (r > x && b > y).then_some(Rect {
            x,
            y,
            w: r - x,
            h: b - y,
        })
    }
    pub fn union(self, o: Rect) -> Rect {
        let (x, y) = (self.x.min(o.x), self.y.min(o.y));
        Rect {
            x,
            y,
            w: (self.x + self.w).max(o.x + o.w) - x,
            h: (self.y + self.h).max(o.y + o.h) - y,
        }
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
        if !self.wraps() {
            return r;
        }
        while r > n / 2. {
            r -= n
        }
        while r <= -n / 2. {
            r += n
        }
        r
    }
    /// True for the active member and its visible neighbours (next; also previous with 3+ members); the
    /// rest are hidden behind the next peek. While the deck rolls, a member sliding out over the top stays
    /// drawn until it has fully left the column (it used to vanish at half way, a visible pop).
    pub fn shown(&self, j: usize) -> bool {
        let n = self.members.len();
        let r = self.relative(j);
        if n == 2 {
            // A pair is a two-item list: the other member always peeks from the side it is on.
            return true;
        }
        (r > -1. && r <= 1.5) || (n >= 3 && (-1.5..=-0.5).contains(&r))
    }
    /// The deck wraps around only when something is shown above the active member (3+ members show the
    /// previous one there). A pair shows nothing above, so rolling stops at either end instead of looping.
    pub fn wraps(&self) -> bool {
        self.members.len() >= 3
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
            // A destroyed fullscreen window must not leave its tuple behind: it would block
            // fill_gaps, set_area rescale and every future fullscreen on this strip for good.
            if self
                .fullscreen
                .as_ref()
                .is_some_and(|(fid, _, _)| *fid == id)
            {
                self.fullscreen = None;
            }
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
    /// Like `group`, but `leading` puts the joiner before the target's members (up on landscape, left on
    /// portrait) instead of after them; it becomes the active member either way (#113 swipe direction).
    pub fn group_at(&mut self, id: WindowId, target: WindowId, leading: bool) {
        self.group(id, target);
        if leading
            && let Some((i, _)) = self.locate(id)
            && self.columns[i].members.len() > 1
        {
            let c = &mut self.columns[i];
            let Some(at) = c.members.iter().position(|m| *m == id) else {
                return;
            };
            let m = c.members.remove(at);
            c.members.insert(0, m);
            c.active = 0;
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

    /// Remove `id` from its group and place it as its own column on the leading (`trailing == false`:
    /// left on landscape, top on portrait) or trailing (`trailing == true`: right on landscape, bottom
    /// on portrait) side of the group's own column (#79, #90).
    pub fn ungroup_to_side(&mut self, id: WindowId, trailing: bool) {
        let Some((from, _)) = self.locate(id) else {
            return;
        };
        if self.columns[from].members.len() <= 1 {
            return;
        }
        let span = self.area.length();
        // The new column lands right beside the group's own column (#90), on the side of the zone used.
        let pos = if trailing { from + 1 } else { from };
        self.remove(id);
        let usable = (span - self.gap).max(1);
        let length = match self.columns.as_mut_slice() {
            [] => span,
            [c] if self.split && c.length == span => {
                c.length = (usable / 2).min(usable - 1).max(c.minimum);
                usable - c.length
            }
            _ => (usable as f64 * self.share).round() as i32,
        }
        .max(1);
        let pos = pos.min(self.columns.len());
        self.columns.insert(
            pos,
            Column {
                members: vec![id],
                active: 0,
                length,
                minimum: 1,
                prior: None,
                frac: 0.,
            },
        );
        self.focus = pos;
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
            let delta = new as i64 - old as i64;
            let other = self.columns[j].length as i64 - delta;
            let floor = self.columns[j].minimum as i64;
            if other < floor {
                new = (new as i64 - (floor - other))
                    .clamp(self.columns[i].minimum as i64, i32::MAX as i64)
                    as i32;
                self.columns[j].length = floor as i32;
            } else {
                self.columns[j].length = other as i32;
            }
        }
        self.columns[i].length = new;
        if leading && !fits {
            // The strip flows from the left, so a longer column would push its RIGHT edge out. Dragging the
            // leading edge must move only that edge: scroll by the change so the trailing edge stays put.
            self.offset = self.offset.saturating_add(new.saturating_sub(old));
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
        if max == 0 {
            return;
        }
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
        } else if !enter && let Some((id, i, length)) = self.fullscreen.take() {
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
            // A pair does not wrap: its other member peeks below while the first is active and above
            // while the second is, sliding smoothly between the two as the deck rolls.
            let both = n >= 3;
            let peek = if n > 1 {
                stack_peek.clamp(0, depth / if both { 4 } else { 2 })
            } else {
                0
            };
            let size = depth - peek - if both { peek } else { 0 };
            let before = if both {
                peek as f64
            } else if n == 2 {
                (c.active as f64 + c.frac).clamp(0., 1.) * peek as f64
            } else {
                0.
            };
            for (j, &id) in c.members.iter().enumerate() {
                let r = if n > 1 { c.relative(j) } else { 0. };
                let cross = before + r * size as f64;
                out.push((
                    id,
                    self.area
                        .project(start, c.length, cross.round() as i32, size),
                ));
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
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Drag {
    None,
    /// Pull a grouped window out and put it before / after the target (by which half the cursor is in).
    Before,
    After,
    /// Join the target's group (the cursor is in the small zone in the middle of the target).
    Stack,
    /// Exchange places with the target.
    Swap,
}
/// Which edge of a neighbour overlay a dragged window entered from (#113).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum EntryEdge {
    /// Entered from the leading edge (left in landscape, top in portrait).
    Leading,
    /// Entered from the trailing edge (right in landscape, bottom in portrait).
    Trailing,
}

/// Threshold band across a neighbour tile measured from the entry edge (#113).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum NeighbourBand {
    /// Entry progress < 1/4: neighbours remain fixed; release returns held window to origin.
    BeforeQuarter,
    /// Entry progress in [1/4, 3/4): stationary dwell triggers preview; `after` is midpoint-selected side.
    Middle { after: bool },
    /// Entry progress >= 3/4: release animates into destination side without premature neighbour reflow.
    PastThreeQuarters { after: bool },
}

/// Determine which edge of a neighbour overlay `[target_lo, target_lo + target_span]` the held
/// window entered from (#113).
///
/// Uses the previous strip-axis pointer coordinate when it lay outside the neighbour, then falls
/// back to strip column order (`origin_col < target_col` => `Leading`), and finally the pointer's
/// half inside the neighbour (`pos < midpoint` => `Leading`).
pub fn infer_entry_edge(
    prev_pos: Option<i32>,
    target_lo: i32,
    target_span: i32,
    origin_col: Option<usize>,
    target_col: Option<usize>,
    pos: i32,
) -> EntryEdge {
    let span = target_span.max(1);
    let target_hi = target_lo + span;
    if let Some(prev) = prev_pos {
        if prev < target_lo {
            return EntryEdge::Leading;
        }
        if prev >= target_hi {
            return EntryEdge::Trailing;
        }
    }
    if let (Some(oc), Some(tc)) = (origin_col, target_col)
        && oc != tc
    {
        return if oc < tc {
            EntryEdge::Leading
        } else {
            EntryEdge::Trailing
        };
    }
    if (pos - target_lo) as i64 * 2 < span as i64 {
        EntryEdge::Leading
    } else {
        EntryEdge::Trailing
    }
}

/// Classify a pointer position along a neighbour overlay `[target_lo, target_lo + target_span]`
/// measured from `entry_edge` (#113).
///
/// Uses exact integer cross-multiplication so boundary values at 1/4, 1/2, and 3/4 never suffer
/// from floating-point rounding drift. Returns `(entry_progress, band)`.
pub fn classify_neighbour_entry(
    pos: i32,
    target_lo: i32,
    target_span: i32,
    entry_edge: EntryEdge,
) -> (f64, NeighbourBand) {
    let span = target_span.max(1) as i64;
    let raw_num = ((pos - target_lo) as i64).clamp(0, span);
    let progress_num = match entry_edge {
        EntryEdge::Leading => raw_num,
        EntryEdge::Trailing => span - raw_num,
    };
    let progress = progress_num as f64 / span as f64;
    let band = if progress_num * 4 < span {
        NeighbourBand::BeforeQuarter
    } else if progress_num * 4 >= span * 3 {
        NeighbourBand::PastThreeQuarters {
            after: entry_edge == EntryEdge::Leading,
        }
    } else {
        NeighbourBand::Middle {
            after: raw_num * 2 >= span,
        }
    };
    (progress, band)
}

/// Evaluate whether a drag of a stack member has reached the 1/4 directional stack-exit threshold
/// along the strip axis (#115).
///
/// - `delta_along`: signed gesture projection along the strip axis (left/up < 0, right/down > 0).
/// - `delta_cross`: signed gesture projection across the strip axis.
/// - `relevant_span`: usable overlay span of the neighbour in the pulled direction (or own column).
/// - `in_own_column`: true while the pointer is still inside the stack's own column (guards
///   cross-axis sibling reordering from triggering stack exit).
///
/// Returns `Some(trailing)` (`false` = leading/left/up, `true` = trailing/right/down) when at or
/// beyond 1/4 of `relevant_span`, or `None` when below threshold.
pub fn classify_stack_exit(
    delta_along: i32,
    delta_cross: i32,
    relevant_span: i32,
    in_own_column: bool,
) -> Option<bool> {
    if delta_along == 0 {
        return None;
    }
    let span = relevant_span.max(1) as i64;
    let along = (delta_along as i64).abs();
    let cross = (delta_cross as i64).abs();
    if along * 4 >= span && (along >= cross || !in_own_column) {
        Some(delta_along > 0)
    } else {
        None
    }
}

/// Compute the drag edge-scroll activation band in pixels from the monitor's usable strip-axis
/// span and the configured percentage (`0` = disabled, `1..=40` = rounded up to at least 1 px so
/// small monitors never truncate to an unusable zero-pixel band, #116).
pub fn edge_scroll_zone_px(usable_span: i32, percent: i32) -> i32 {
    if percent <= 0 || usable_span <= 0 {
        return 0;
    }
    let pct = percent.clamp(0, 40) as f64;
    ((usable_span as f64) * pct / 100.0).ceil().max(1.0) as i32
}

/// Return `-1` (leading edge), `+1` (trailing edge), or `0` (middle or disabled) for a cursor
/// strip-axis coordinate `pos` on a monitor whose usable strip-axis range is `[usable_lo, usable_hi]`
/// (#116).
pub fn edge_scroll_direction(pos: i32, usable_lo: i32, usable_hi: i32, percent: i32) -> i32 {
    let span = usable_hi - usable_lo;
    let zone = edge_scroll_zone_px(span, percent);
    if zone <= 0 {
        return 0;
    }
    if pos < usable_lo + zone {
        -1
    } else if pos >= usable_hi - zone {
        1
    } else {
        0
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
    /// Finger travel of the scroll so far (decides whether a fourth finger may still turn it into a drag).
    travel: f64,
    /// Five or more fingers were seen: nothing is a Planor gesture until the hand is lifted.
    vetoed: bool,
    /// Drag axis lock: `Some(horizontal)` makes only a four-finger start along that axis (the strip axis) pick
    /// the window up. Set by the caller before each `sample`; `None` is the free drag. The carry itself is
    /// restricted by the daemon (`pad_move`), which needs the raw finger deltas.
    pub axis_lock: Option<bool>,
    /// With the lock on: whether a monitor lies across the axis on the (negative, positive) side (left/up,
    /// right/down). A clearly cross-axis start towards such a side is a carry to that monitor (or a regroup from a
    /// perpendicular direction) and starts unlocked. Set by the caller before each `sample`.
    pub cross_ok: (bool, bool),
    /// The running drag started across the lock axis towards a monitor: the caller starts it unlocked.
    pub started_cross: bool,
    /// Unit rays toward neighbouring monitors (including diagonal ones) that lie off the strip axis (#102).
    pub monitor_rays: Vec<(f64, f64)>,
    /// The neighbouring-monitor ray that won when a four-finger drag started off the strip axis (#102).
    pub started_ray: Option<(f64, f64)>,
    /// A locked drag started across the axis: ignored until the fingers lift (the window is not picked up).
    rejected: bool,
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
    /// A fifth finger touched while carrying a window: put it back, drop nothing.
    DragCancel,
    /// `direction` is +1 for rightwards/downwards finger travel and -1 for leftwards/upwards.
    Step {
        fingers: usize,
        horizontal: bool,
        direction: isize,
    },
}
impl Gesture {
    pub fn sample(&mut self, points: &[(f64, f64)], deadzone: f64, step: f64) -> Swipe {
        let n = points.len();
        let lock = self.axis_lock;
        if self.rejected && (n == 3 || n == 4) {
            return Swipe::None;
        }
        if n >= 5 || (self.vetoed && n >= 3) {
            // The whole hand on the pad is not a Planor gesture. It lands and lifts in stages (3, 4, 5
            // fingers in the owner's recording), so whatever the first fingers started is cancelled and
            // everything is ignored until fewer than three fingers remain.
            let was = (self.dragging, self.scrolling);
            *self = Self {
                vetoed: true,
                ..Self::default()
            };
            return match was {
                (true, _) => Swipe::DragCancel,
                (_, true) => Swipe::ScrollEnd,
                _ => Swipe::None,
            };
        }
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
        let pos = (
            points.iter().map(|p| p.0).sum::<f64>() / n as f64,
            points.iter().map(|p| p.1).sum::<f64>() / n as f64,
        );
        if self.fingers != n {
            if self.scrolling && n == 4 && self.fingers == 3 && self.travel < 2. * deadzone {
                // Fingers also LAND one at a time: the owner's recorded four-finger drags often touched with
                // three first for a few frames. A scroll that has barely moved becomes the drag.
                *self = Self {
                    fingers: n,
                    ..Self::default()
                };
                return Swipe::ScrollEnd;
            }
            if self.dragging || self.scrolling {
                // The gesture is locked once it started: fingers rarely lift together, and the first one
                // up used to end a drag and start a three-finger scroll with the rest (a jerk at every
                // drop). Keep the gesture and re-base the centroid so it does not jump.
                self.fingers = n;
                self.last = pos;
                return Swipe::None;
            }
            // Still inside the dead zone: start over with the new count.
            *self = Self {
                fingers: n,
                ..Self::default()
            };
        }
        let Some(origin) = self.origin else {
            self.origin = Some(pos);
            self.last = pos;
            return Swipe::None;
        };
        let dx = pos.0 - origin.0;
        let dy = pos.1 - origin.1;
        if (n == 4 && !self.scrolling) || self.dragging {
            if !self.dragging {
                if dx.hypot(dy) < deadzone {
                    return Swipe::None;
                }
                if let Some(h) = lock {
                    // Pick the nearest offered ray (strip axis or a neighbouring monitor ray, #102)
                    // using the same 1.5x lead rule as the three-finger scroll.
                    let (along, cross) = if h {
                        (dx.abs(), dy.abs())
                    } else {
                        (dy.abs(), dx.abs())
                    };
                    let strip_ok = along >= 1.5 * cross;
                    let mon_ray = best_ray(dx, dy, &self.monitor_rays);
                    let mon_proj = mon_ray.map(|(rx, ry)| dx * rx + dy * ry);
                    if let (Some(ray), Some(proj)) = (mon_ray, mon_proj)
                        && (!strip_ok || proj >= RAY_LEAD_MARGIN * along)
                    {
                        self.started_cross = true;
                        self.started_ray = Some(ray);
                    } else if !strip_ok {
                        let across = if h { dy } else { dx };
                        let allowed = if across > 0. {
                            self.cross_ok.1
                        } else {
                            self.cross_ok.0
                        };
                        if cross >= 1.5 * along && allowed {
                            self.started_cross = true;
                            self.started_ray = None;
                        } else {
                            if cross >= 1.5 * along || dx.hypot(dy) >= 2. * deadzone {
                                self.rejected = true;
                            }
                            return Swipe::None;
                        }
                    }
                }
                self.dragging = true;
                self.last = origin; // the first move includes the dead-zone travel
                return Swipe::DragStart;
            }
            let (mx, my) = (pos.0 - self.last.0, pos.1 - self.last.1);
            self.last = pos;
            return if mx == 0. && my == 0. {
                Swipe::None
            } else {
                Swipe::DragMove(mx, my)
            };
        }
        // Three fingers start scrolling almost at once: the axis is picked after 3 units of travel when one
        // axis clearly leads (1.5x), else once the travel reaches the full dead zone. Fingers settle as they
        // land; the owner's recorded sideways swipe moved (3.1, -3.3) in its first 7 frames and was locked
        // vertical by the old "first to 3 units" rule, so it never scrolled.
        let full = deadzone;
        let deadzone = deadzone.min(3.);
        if self.horizontal.is_none() {
            let (ax, ay) = (dx.abs(), dy.abs());
            let big = ax.max(ay);
            let clear = big >= deadzone && big >= 1.5 * ax.min(ay);
            if !clear && big < full {
                return Swipe::None;
            }
            self.horizontal = Some(ax >= ay);
            self.last = origin;
        }
        let horizontal = self.horizontal.unwrap_or(true);
        let delta = if horizontal {
            pos.0 - self.last.0
        } else {
            pos.1 - self.last.1
        };
        self.last = pos;
        self.travel += delta.abs();
        if horizontal {
            // Sideways: scroll continuously, with the focus following the window nearest the centre.
            self.scrolling = true;
            return if delta == 0. {
                Swipe::None
            } else {
                Swipe::Scroll(delta)
            };
        }
        let _ = step;
        self.scrolling = true;
        if delta == 0. {
            Swipe::None
        } else {
            Swipe::ScrollV(delta)
        }
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
/// Reachable adjacent monitor in direction `(dx, dy)` sharing an edge with `source` (#111, #116):
/// requires a boundary gap `<= 2` px and positive perpendicular overlap (`overlap > 0`), so
/// disconnected display gaps and corner-only touches are excluded.
pub fn connected_adjacent(source: Rect, others: &[Rect], dx: i32, dy: i32) -> Option<usize> {
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
            if !(0..=2).contains(&gap) {
                return None;
            }
            let overlap = a1.min(b1) - a0.max(b0);
            if overlap <= 0 {
                return None;
            }
            Some(((gap, -overlap, (a0 + a1 - b0 - b1).abs()), i))
        })
        .min()
        .map(|(_, i)| i)
}
/// The projection lead a neighbouring-monitor ray needs over the strip axis to win when the
/// gesture already qualifies for the strip axis (`along >= 1.5 * cross`): a swipe that is clearly
/// along the strip must not drift onto a diagonal ray (#102 review).
pub const RAY_LEAD_MARGIN: f64 = 1.2;

/// Whether the ray `origin + t * dir` (t in 0..=`len`, `dir` a unit vector) passes more than 2 px
/// into the interior of any monitor in `others` except `target` and `home` (#102 review): a carry
/// along such a ray would enter that monitor first and the drop would land there, not on `target`.
fn ray_blocked(
    dir: (f64, f64),
    len: f64,
    origin: (f64, f64),
    target: Rect,
    home: Rect,
    others: &[Rect],
) -> bool {
    const PAD: f64 = 2.0;
    others.iter().any(|&m| {
        if m == target || m == home {
            return false;
        }
        let (x0, y0) = (m.x as f64 + PAD, m.y as f64 + PAD);
        let (x1, y1) = ((m.x + m.w) as f64 - PAD, (m.y + m.h) as f64 - PAD);
        if x0 >= x1 || y0 >= y1 {
            return false;
        }
        let (mut tmin, mut tmax) = (0.0f64, len);
        for (o, d, a, b) in [(origin.0, dir.0, x0, x1), (origin.1, dir.1, y0, y1)] {
            if d.abs() < 1e-9 {
                if o < a || o > b {
                    return false;
                }
            } else {
                let (mut t1, mut t2) = ((a - o) / d, (b - o) / d);
                if t1 > t2 {
                    std::mem::swap(&mut t1, &mut t2);
                }
                tmin = tmin.max(t1);
                tmax = tmax.min(t2);
                if tmin >= tmax {
                    return false;
                }
            }
        }
        true
    })
}

/// Unit direction vectors from `origin` (the drag's actual start point, usually the cursor over
/// the picked-up window) toward every neighbouring monitor (monitors whose bounds touch `home`
/// along an edge or corner, within 2 px), excluding rays nearly collinear with `home`'s strip axis
/// (`|cross| < 0.25`) since the strip axis already reaches those monitors (#102). The target point
/// is the foot of the perpendicular when the neighbour lies straight across an edge from `origin`,
/// else the neighbour's centre; when that ray would cross a third monitor on the way (the carry
/// would enter it first and the drop would land there, #102 review) or run along the strip axis,
/// the nearest point of the neighbour's rect is tried before giving up on that neighbour.
pub fn monitor_rays(
    home: Rect,
    others: &[Rect],
    horizontal: bool,
    origin: (f64, f64),
) -> Vec<(f64, f64)> {
    let (ox, oy) = origin;
    let mut rays = Vec::new();
    for &b in others {
        if b == home {
            continue;
        }
        let gap_x = (b.x - (home.x + home.w)).max(home.x - (b.x + b.w)).max(0);
        let gap_y = (b.y - (home.y + home.h)).max(home.y - (b.y + b.h)).max(0);
        if gap_x > 2 || gap_y > 2 {
            continue;
        }
        let covers_strip_edge = if horizontal {
            b.y <= home.y && b.y + b.h >= home.y + home.h
        } else {
            b.x <= home.x && b.x + b.w >= home.x + home.w
        };
        if covers_strip_edge {
            continue;
        }
        let centre = (b.x as f64 + b.w as f64 * 0.5, b.y as f64 + b.h as f64 * 0.5);
        let nearest = (
            ox.clamp(b.x as f64, (b.x + b.w) as f64),
            oy.clamp(b.y as f64, (b.y + b.h) as f64),
        );
        // When the neighbour lies straight across an edge from the origin (the nearest point differs
        // in one coordinate only), the perpendicular ray reaches it by the shortest path: straight up
        // from under a monitor above carries straight up instead of slanting toward its centre.
        let straight = (nearest.0 == ox || nearest.1 == oy).then_some(nearest);
        let dir = straight
            .into_iter()
            .chain([centre, nearest])
            .find_map(|(tx, ty)| {
                let (dx, dy) = (tx - ox, ty - oy);
                let len = dx.hypot(dy);
                let unit = (dx / len.max(1e-9), dy / len.max(1e-9));
                let cross = if horizontal {
                    unit.1.abs()
                } else {
                    unit.0.abs()
                };
                // A ray nearly along the strip axis is not offered: the strip axis already gets there.
                if len < 1.0 || cross < 0.25 || ray_blocked(unit, len, (ox, oy), b, home, others) {
                    return None;
                }
                Some(unit)
            });
        let Some((rx, ry)) = dir else {
            continue;
        };
        rays.push((rx, ry));
    }
    rays
}

/// Nearest unit ray in `rays` that `(dx, dy)` leads toward with the 1.5x lead rule (`proj > 0` and
/// `proj >= 1.5 * perp`), or `None` if no ray lies within that cone (#102).
pub fn best_ray(dx: f64, dy: f64, rays: &[(f64, f64)]) -> Option<(f64, f64)> {
    let mut best: Option<(f64, (f64, f64))> = None;
    for &(rx, ry) in rays {
        let proj = dx * rx + dy * ry;
        let perp = (dx * ry - dy * rx).abs();
        if proj > 0. && proj >= 1.5 * perp && best.is_none_or(|(bp, _)| proj > bp) {
            best = Some((proj, (rx, ry)));
        }
    }
    best.map(|(_, r)| r)
}
#[derive(Default)]
pub struct ContactFrame {
    /// Also assemble frames of 1, 2 or 5 fingers (only while `planor gestures record` runs).
    pub all: bool,
    waiting: bool,
    target: usize,
    seen_slots: usize,
    points: std::collections::BTreeMap<u32, (f64, f64)>,
}
impl ContactFrame {
    pub fn packet(
        &mut self,
        count: u32,
        contacts: impl IntoIterator<Item = (u32, (f64, f64))>,
    ) -> Option<Vec<(f64, f64)>> {
        let contacts: Vec<_> = contacts.into_iter().collect();
        let slots = if self.all && count != 0 && contacts.is_empty() {
            // Recording compatibility: an opening frame with no tip-down contacts is a lift.
            count as usize
        } else {
            contacts.len()
        };
        self.packet_with_slots(count, slots, contacts)
    }
    /// Assemble one HID contact frame from one or more reports. `slots` is the number of finger
    /// contact slots present in this report (tip-down AND tip-up combined), so hybrid multi-report
    /// digitizers wait until all `count` contacts have been reported before deciding whether a
    /// frame is a lift, while parallel digitizers (and the final report of a hybrid frame) emit an
    /// immediate lift when all reported contacts are tip-up (#43, #44).
    pub fn packet_with_slots(
        &mut self,
        count: u32,
        slots: usize,
        contacts: impl IntoIterator<Item = (u32, (f64, f64))>,
    ) -> Option<Vec<(f64, f64)>> {
        if count != 0 {
            self.points.clear();
            self.seen_slots = 0;
            self.waiting = count >= 3 || (self.all && count >= 1);
            self.target = count as usize;
        }
        if !self.waiting {
            return Some(vec![]);
        }
        let before = self.points.len();
        self.points.extend(contacts);
        let added = self.points.len() - before;
        self.seen_slots = self.seen_slots.saturating_add(slots.max(added));
        if self.seen_slots < self.target {
            return None;
        }
        self.waiting = false;
        self.seen_slots = 0;
        let pts: Vec<(f64, f64)> = std::mem::take(&mut self.points).into_values().collect();
        if !self.all && pts.len() < 3 {
            Some(vec![])
        } else {
            Some(pts)
        }
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
        if !delta.is_finite() {
            // A zero-depth divide (or a poisoned gain) can produce inf/NaN: refuse it instead of
            // spinning forever or leaving frac = NaN behind.
            return false;
        }
        if !c.wraps() {
            // No window above: clamp to the ends instead of looping round.
            let p = (c.active as f64 + c.frac + delta).clamp(0., (n - 1) as f64);
            c.active = p.floor() as usize;
            c.frac = p - c.active as f64;
            if c.active as isize == n - 1 {
                c.frac = 0.
            }
            return true;
        }
        // Whole steps in O(1), remainder keeps its sign in (-1, 1) exactly like the old per-step
        // wrap. A per-step loop hangs the daemon thread on a huge delta (swipe_scroll_gain has no
        // upper bound) and never terminates on inf.
        let total = c.frac + delta;
        let step = total.trunc();
        c.frac = total - step;
        // Reduce before adding: a huge delta saturates `step as isize` to isize::MAX, which would
        // overflow the add (debug builds panic). (a + s) mod n == (a + s mod n) mod n.
        let shift = (step as isize).rem_euclid(n);
        c.active = (c.active as isize + shift).rem_euclid(n) as usize;
        true
    }
    /// End of a roll: the deck settles on the nearest member.
    pub fn settle_roll(&mut self) {
        for c in &mut self.columns {
            if c.frac != 0. {
                let n = c.members.len() as isize;
                // Past half way the next member is the one mostly in view: it becomes active.
                let step = c.frac.round() as isize;
                c.active = if c.wraps() {
                    (c.active as isize + step).rem_euclid(n.max(1)) as usize
                } else {
                    (c.active as isize + step).clamp(0, n - 1) as usize
                };
                c.frac = 0.;
            }
        }
    }
    /// Exchange two windows' places (columns or group slots, the slots keep their sizes). Used when a
    /// window is dropped onto another one. The moved window becomes the focus.
    pub fn swap(&mut self, a: WindowId, b: WindowId) {
        let (Some((i, j)), Some((k, l))) = (self.locate(a), self.locate(b)) else {
            return;
        };
        if a == b {
            return;
        }
        self.columns[i].members[j] = b;
        self.columns[k].members[l] = a;
        self.focus = k;
        self.columns[k].active = l;
        self.scroll(0)
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
            details: format!(
                "focus {} out of range for {} columns",
                strip.focus,
                strip.columns.len()
            ),
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

pub mod cover {
    /// Timing parameters for covering newly created or cross-monitor windows behind the overlay
    /// while they resize at their destination slot on-screen.
    #[derive(Clone, Copy, Debug)]
    pub struct CoverConfig {
        pub min_hold_ms: u64,
        pub settle_ms: u64,
        pub spinner_delay_ms: u64,
        pub spinner_fade_ms: u64,
        pub max_ms: u64,
        pub rev_per_sec: f64,
    }

    impl Default for CoverConfig {
        fn default() -> Self {
            Self {
                min_hold_ms: 40,
                settle_ms: 48,
                spinner_delay_ms: 120,
                spinner_fade_ms: 100,
                max_ms: 320,
                rev_per_sec: 1.25,
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct CoverVisual {
        pub active: bool,
        pub timed_out: bool,
        pub spinner_alpha: f32,
        pub spinner_angle_rad: f32,
    }

    pub fn eval_cover(
        cfg: CoverConfig,
        elapsed_ms: u64,
        landed_for_ms: Option<u64>,
        source_size_matches: bool,
    ) -> CoverVisual {
        if elapsed_ms >= cfg.max_ms {
            return CoverVisual {
                active: false,
                timed_out: true,
                spinner_alpha: 0.0,
                spinner_angle_rad: 0.0,
            };
        }
        if elapsed_ms >= cfg.min_hold_ms
            && landed_for_ms.is_some_and(|l| source_size_matches || l >= cfg.settle_ms)
        {
            return CoverVisual {
                active: false,
                timed_out: false,
                spinner_alpha: 0.0,
                spinner_angle_rad: 0.0,
            };
        }
        let spinner_alpha = if elapsed_ms < cfg.spinner_delay_ms {
            0.0
        } else {
            ((elapsed_ms - cfg.spinner_delay_ms) as f32 / cfg.spinner_fade_ms.max(1) as f32)
                .clamp(0.0, 1.0)
        };
        let spinner_angle_rad =
            ((elapsed_ms as f64 / 1000.0) * std::f64::consts::TAU * cfg.rev_per_sec) as f32;
        CoverVisual {
            active: true,
            timed_out: false,
            spinner_alpha,
            spinner_angle_rad,
        }
    }
}

pub mod pacing {
    pub const MIN_REFRESH_HZ: f64 = 10.0;
    pub const MAX_REFRESH_HZ: f64 = 1000.0;
    pub const DEFAULT_REFRESH_HZ: f64 = 60.0;

    pub fn clamp_hz(hz: f64) -> Option<f64> {
        (hz.is_finite() && (MIN_REFRESH_HZ..=MAX_REFRESH_HZ).contains(&hz)).then_some(hz)
    }

    pub fn period_us(hz: f64) -> f64 {
        1_000_000.0 / hz.clamp(MIN_REFRESH_HZ, MAX_REFRESH_HZ)
    }

    pub fn effective_refresh_us(dwm_refresh_us: Option<f64>, monitor_hz: Option<f64>) -> f64 {
        if let Some(u) = dwm_refresh_us
            && u.is_finite()
            && (1_000.0..=100_000.0).contains(&u)
        {
            return u;
        }
        if let Some(h) = monitor_hz.and_then(clamp_hz) {
            return period_us(h);
        }
        1_000_000.0 / DEFAULT_REFRESH_HZ
    }

    pub fn fast_tick_ms(max_hz: f64) -> u64 {
        let hz = clamp_hz(max_hz).unwrap_or(DEFAULT_REFRESH_HZ).max(120.0);
        ((650.0 / hz).round() as u64).clamp(3, 6)
    }

    pub fn pad_move_min_interval_us(max_hz: f64) -> u64 {
        let hz = clamp_hz(max_hz).unwrap_or(DEFAULT_REFRESH_HZ).max(120.0);
        (period_us(hz) * 0.5).round().clamp(2_000.0, 4_000.0) as u64
    }

    pub fn settled_capture_delay_100ns(refresh_us: f64) -> i64 {
        ((refresh_us * 2.0).clamp(33_000.0, 120_000.0) * 10.0).round() as i64
    }

    pub fn live_gap_cutoff_ms(refresh_us: f64) -> u64 {
        ((refresh_us * 3.5) / 1000.0).ceil().clamp(120.0, 400.0) as u64
    }

    pub fn frame_budget_ms(refresh_us: f64) -> f64 {
        ((refresh_us / 1000.0).clamp(4.0, 16.0) * 10.0).round() / 10.0
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct FrameSample {
        pub interval_us: u64,
        pub refresh_us: f64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum FrameVerdict {
        Ok,
        Late,
        Dropped,
    }

    pub fn classify_frame(sample: FrameSample) -> FrameVerdict {
        let r = sample.refresh_us.clamp(1_000.0, 100_000.0);
        if sample.interval_us as f64 > r * 2.5 {
            FrameVerdict::Dropped
        } else if sample.interval_us as f64 > r * 1.5 {
            FrameVerdict::Late
        } else {
            FrameVerdict::Ok
        }
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct FrameSummary {
        pub frames: u64,
        pub late: u64,
        pub dropped: u64,
        pub p50_ms: f64,
        pub p95_ms: f64,
        pub worst_ms: f64,
        pub refresh_ms: f64,
        pub min_refresh_ms: f64,
        pub max_refresh_ms: f64,
        pub effective_hz: f64,
    }

    pub fn summarize_frames(samples: &[FrameSample]) -> FrameSummary {
        if samples.is_empty() {
            return FrameSummary::default();
        }
        let mut intervals = Vec::with_capacity(samples.len());
        let mut refreshes = Vec::with_capacity(samples.len());
        let mut late = 0u64;
        let mut dropped = 0u64;
        for &s in samples {
            intervals.push(s.interval_us);
            let r = s.refresh_us.clamp(1_000.0, 100_000.0);
            refreshes.push(r);
            match classify_frame(s) {
                FrameVerdict::Ok => {}
                FrameVerdict::Late => late += 1,
                FrameVerdict::Dropped => {
                    late += 1;
                    dropped += 1;
                }
            }
        }
        intervals.sort_unstable();
        refreshes.sort_by(f64::total_cmp);
        let pick_u64 = |v: &[u64], q: f64| v[((v.len() - 1) as f64 * q) as usize];
        let p50_us = pick_u64(&intervals, 0.5);
        let p95_us = pick_u64(&intervals, 0.95);
        let worst_us = *intervals.last().unwrap();
        let med_refresh = refreshes[(refreshes.len() - 1) / 2];
        let min_refresh = *refreshes.first().unwrap();
        let max_refresh = *refreshes.last().unwrap();
        FrameSummary {
            frames: samples.len() as u64,
            late,
            dropped,
            p50_ms: p50_us as f64 / 1000.0,
            p95_ms: p95_us as f64 / 1000.0,
            worst_ms: worst_us as f64 / 1000.0,
            refresh_ms: med_refresh / 1000.0,
            min_refresh_ms: min_refresh / 1000.0,
            max_refresh_ms: max_refresh / 1000.0,
            effective_hz: if p50_us > 0 { 1e6 / p50_us as f64 } else { 0.0 },
        }
    }
}

#[cfg(test)]
mod contact_frame_tests {
    use super::ContactFrame;

    #[test]
    fn contact_frame_parallel_and_hybrid_lifts_are_accurate() {
        // (a) Parallel digitizer (5 slots/report, normal mode all=false):
        //     3-, 4-, and 5-finger full tip-up lifts emit an immediate empty lift frame (#43, #44).
        let mut f = ContactFrame::default();
        assert!(!f.all);
        for n in [3u32, 4, 5] {
            let pts: Vec<_> = (1..=n).map(|i| (i, (i as f64 * 10.0, 10.0))).collect();
            let frame = f
                .packet_with_slots(n, 5, pts)
                .expect("parallel touch completes");
            assert_eq!(frame.len(), n as usize);
            assert_eq!(
                f.packet_with_slots(n, 5, []),
                Some(vec![]),
                "parallel {n}-finger tip-up lift must emit lift immediately"
            );
        }
        // Parallel partial tip-up: 4 counted, 2 down -> immediate lift; 4 counted, 3 down -> 3 pts.
        assert_eq!(
            f.packet_with_slots(4, 5, [(1, (1.0, 1.0)), (2, (2.0, 1.0))]),
            Some(vec![]),
            "parallel 4-counted with only 2 tip-down completes as lift"
        );
        let three = f
            .packet_with_slots(4, 5, [(1, (1.0, 1.0)), (2, (2.0, 1.0)), (3, (3.0, 1.0))])
            .expect("parallel 4-counted with 3 tip-down completes immediately");
        assert_eq!(three.len(), 3);

        // (b) Hybrid 1-per-report: first report tip-up, next 3 tip-down => NO false lift.
        let mut h1 = ContactFrame::default();
        assert_eq!(
            h1.packet_with_slots(4, 1, []),
            None,
            "hybrid 1/report must wait for continuations"
        );
        assert_eq!(h1.packet_with_slots(0, 1, [(2, (2.0, 1.0))]), None);
        assert_eq!(h1.packet_with_slots(0, 1, [(3, (3.0, 1.0))]), None);
        let h1_done = h1
            .packet_with_slots(0, 1, [(4, (4.0, 1.0))])
            .expect("hybrid 1/report completes on 4th slot");
        assert_eq!(
            h1_done.len(),
            3,
            "3 remaining tip-down contacts emitted without false lift"
        );

        // Hybrid 2-per-report: first report all tip-up, second report 2 tip-down => NO early lift on report 1.
        let mut h2 = ContactFrame::default();
        assert_eq!(
            h2.packet_with_slots(4, 2, []),
            None,
            "hybrid 2/report must wait for continuation"
        );
        assert_eq!(
            h2.packet_with_slots(0, 2, [(3, (3.0, 1.0)), (4, (4.0, 1.0))]),
            Some(vec![]),
            "hybrid 2/report completes with 2 tip-down (<3 in normal mode => lift)"
        );
        // Hybrid 2-per-report: first report 1 tip-up + 1 tip-down, second 2 tip-down => 3 pts.
        assert_eq!(h2.packet_with_slots(4, 2, [(2, (2.0, 1.0))]), None);
        let h2_three = h2
            .packet_with_slots(0, 2, [(3, (3.0, 1.0)), (4, (4.0, 1.0))])
            .expect("hybrid 2/report completes with 3 tip-down");
        assert_eq!(h2_three.len(), 3);

        // (c) Hybrid full lift across reports => None on opening report, Some([]) once complete.
        let mut hl = ContactFrame::default();
        assert_eq!(hl.packet_with_slots(4, 2, []), None);
        assert_eq!(
            hl.packet_with_slots(0, 2, []),
            Some(vec![]),
            "hybrid full lift emits once"
        );
        // Hybrid odd count=3 across 2-slot reports (padding slot in report 2) does not bleed into next frame.
        assert_eq!(hl.packet_with_slots(3, 2, []), None);
        assert_eq!(hl.packet_with_slots(0, 2, []), Some(vec![]));
        assert_eq!(
            hl.packet_with_slots(3, 2, [(1, (1.0, 1.0)), (2, (2.0, 1.0))]),
            None,
            "padding slot from previous frame must not bleed into next frame"
        );
        assert_eq!(
            hl.packet_with_slots(0, 2, [(3, (3.0, 1.0))]).unwrap().len(),
            3
        );

        // (d) Recording mode (`all = true`) behaviour unchanged.
        let mut rec = ContactFrame {
            all: true,
            ..ContactFrame::default()
        };
        assert_eq!(rec.packet(1, [(1, (5.0, 5.0))]), Some(vec![(5.0, 5.0)]));
        assert_eq!(rec.packet(2, [(1, (5.0, 5.0))]), None);
        assert_eq!(rec.packet(0, [(2, (9.0, 5.0))]).unwrap().len(), 2);
        assert_eq!(rec.packet(1, []), Some(vec![]));
    }
}

/// Monitor a fling in direction `(dx, dy)` (unit axis step) throws a window to: any other monitor lying on
/// that side of `source` (its rect starts past the source edge, within 2 px of overlap), even offset or across
/// a small gap. When several qualify, the one whose centre is nearest the source centre wins.
pub fn fling_target(source: Rect, others: &[Rect], dx: i32, dy: i32) -> Option<usize> {
    let sc = (
        source.x as f64 + source.w as f64 / 2.,
        source.y as f64 + source.h as f64 / 2.,
    );
    others
        .iter()
        .enumerate()
        .filter(|(_, b)| **b != source)
        .filter(|(_, b)| match (dx, dy) {
            (1, 0) => b.x >= source.x + source.w - 2,
            (-1, 0) => b.x + b.w <= source.x + 2,
            (0, 1) => b.y >= source.y + source.h - 2,
            (0, -1) => b.y + b.h <= source.y + 2,
            _ => false,
        })
        .map(|(i, b)| {
            let c = (b.x as f64 + b.w as f64 / 2., b.y as f64 + b.h as f64 / 2.);
            let d = (c.0 - sc.0).powi(2) + (c.1 - sc.1).powi(2);
            ((d * 1000.) as i64, i)
        })
        .min()
        .map(|(_, i)| i)
}

/// Lift-off velocity (units/s) of a flick from `(age_secs, dx, dy)` samples: the travel of the last 120 ms
/// divided by the time that travel actually took (at least 40 ms), so a short quick flick is not diluted by a
/// fixed 120 ms divisor.
pub fn fling_velocity(samples: &[(f64, f64, f64)]) -> (f64, f64) {
    let (mut vx, mut vy, mut oldest) = (0., 0., None::<f64>);
    for &(age, dx, dy) in samples {
        if age <= 0.12 {
            vx += dx;
            vy += dy;
            oldest = Some(oldest.map_or(age, |o: f64| o.max(age)));
        }
    }
    let span = oldest.unwrap_or(0.12).clamp(0.04, 0.12);
    (vx / span, vy / span)
}

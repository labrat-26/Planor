use planor::*;
fn strip() -> Strip {
    Strip::new(Rect {
        x: -100,
        y: 20,
        w: 1000,
        h: 600,
    })
}
#[test]
fn work_area_change_refits_columns() {
    // Portrait monitor: a lone column sized against the full monitor height must shrink when the taskbar
    // reserves its strip, or the renderer clips it and parks it off-screen.
    let mut s = Strip::new(Rect {
        x: 0,
        y: 0,
        w: 1440,
        h: 2560,
    });
    s.insert(1, 1);
    assert_eq!(s.columns[0].length, 2560);
    s.set_area(Rect {
        x: 0,
        y: 0,
        w: 1440,
        h: 2512,
    });
    assert_eq!(s.columns[0].length, 2512);
    // A pair keeps filling the new area exactly (gap 6 between the two columns).
    s.insert(2, 1);
    s.set_area(Rect {
        x: 0,
        y: 0,
        w: 1440,
        h: 2000,
    });
    let used: i32 = s.columns.iter().map(|c| c.length).sum::<i32>() + 6;
    assert_eq!(used, 2000);
}

#[test]
fn first_pair() {
    let mut s = strip();
    s.insert(1, 600);
    s.insert(2, 300);
    assert_eq!((s.columns[0].length, s.columns[1].length), (600, 394))
}
#[test]
fn stack_split() {
    let mut s = strip();
    s.insert(1, 1);
    s.insert(2, 1);
    s.group(2, 1);
    assert_eq!(s.columns[0].length, 1000);
    s.insert(3, 1);
    assert_eq!(s.columns[0].length, 497);
    s.ungroup(2);
    assert_eq!(s.active(), Some(2));
    assert_eq!(s.columns[0].members, [1])
}
#[test]
fn resize_focus() {
    let mut s = strip();
    for id in 1..=4 {
        s.insert(id, 1)
    }
    s.resize(0, 10, true);
    assert_eq!(s.columns[0].length, 497);
    s.resize(0, 600, false);
    assert_eq!(s.columns[1].length, 497);
    s.select(2);
    s.remove(2);
    assert_eq!(s.active(), Some(3))
}
#[test]
fn fullscreen_restore() {
    let mut s = strip();
    for id in 1..=3 {
        s.insert(id, 1)
    }
    s.select(1);
    s.maximize();
    s.maximize();
    s.fullscreen(true, s.area);
    assert_eq!(s.focus, 2);
    s.fullscreen(false, s.area);
    assert_eq!(s.focus, 0);
    assert_eq!(s.columns[0].length, 497)
}
#[test]
fn scroll_portrait() {
    let mut s = strip();
    s.area.h = 1200;
    for id in 1..=4 {
        s.insert(id, 1)
    }
    s.reveal(48);
    assert_eq!(s.rects(28).last().unwrap().1.y + 597, 1220);
    s.scroll(i32::MIN);
    assert_eq!(s.active(), Some(4));
    assert_eq!(s.offset, 0)
}
#[test]
fn motion_drag() {
    for spring in [true, false] {
        assert_eq!(motion(0., spring), 0.);
        assert_eq!(motion(1., spring), 1.)
    }
    assert_eq!(drag(0.1, 10., 300., 0, 350, false), Drag::Before);
    assert_eq!(drag(0.1, 10., 300., 0, 350, true), Drag::None);
    assert_eq!(drag(0.5, 0., 300., 350, 350, true), Drag::Stack)
}
#[test]
fn synthetic_contacts() {
    let mut g = Gesture::default();
    for n in 1..=5 {
        assert_eq!(g.sample(&vec![(0., 0.); n], 12., 100.), Swipe::None)
    }
    // Three fingers sideways: continuous scroll (dead-zone travel included), vertical drift ignored.
    g.sample(&[(0., 0.); 3], 12., 100.);
    assert_eq!(g.sample(&[(2., 1.); 3], 12., 100.), Swipe::None);
    assert_eq!(g.sample(&[(5., 1.); 3], 12., 100.), Swipe::Scroll(5.));
    assert_eq!(g.sample(&[(15., 1.); 3], 12., 100.), Swipe::Scroll(10.));
    assert_eq!(g.sample(&[(40., 90.); 3], 12., 100.), Swipe::Scroll(25.));
    assert_eq!(g.sample(&[(30., 90.); 3], 12., 100.), Swipe::Scroll(-10.));
    assert_eq!(g.sample(&[], 12., 100.), Swipe::ScrollEnd);
    // Three fingers vertically: continuous too (rolls through a group), and it ends like the sideways scroll.
    g.sample(&[(0., 0.); 3], 12., 100.);
    assert_eq!(g.sample(&[(1., 2.); 3], 12., 100.), Swipe::None);
    assert_eq!(g.sample(&[(1., 15.); 3], 12., 100.), Swipe::ScrollV(15.));
    assert_eq!(g.sample(&[(1., 120.); 3], 12., 100.), Swipe::ScrollV(105.));
    assert_eq!(g.sample(&[], 12., 100.), Swipe::ScrollEnd);
    // Four fingers drag the window: pick up past the dead zone, follow on both axes, drop on lift.
    assert_eq!(g.sample(&[(0., 0.); 4], 12., 100.), Swipe::None);
    assert_eq!(g.sample(&[(5., 5.); 4], 12., 100.), Swipe::None);
    assert_eq!(g.sample(&[(20., 5.); 4], 12., 100.), Swipe::DragStart);
    assert_eq!(g.sample(&[(30., 25.); 4], 12., 100.), Swipe::DragMove(30. - 0., 25. - 0.));
    assert_eq!(g.sample(&[(30., 25.); 4], 12., 100.), Swipe::None);
    assert_eq!(g.sample(&[(25., 40.); 4], 12., 100.), Swipe::DragMove(-5., 15.));
    assert_eq!(g.sample(&[], 12., 100.), Swipe::DragEnd);
    assert_eq!(g.sample(&[], 12., 100.), Swipe::None);
}
#[test]
fn physical_adjacency() {
    let center = strip().area;
    let left = Rect { x: -1100, ..center };
    let top = Rect {
        x: -1100,
        y: -580,
        ..center
    };
    assert_eq!(adjacent(center, &[top, left], -1, 0), Some(1));
    assert_eq!(adjacent(center, &[left], 1, 0), None)
}
#[test]
fn reorder_stack_member() {
    let mut s = strip();
    for id in 1..=4 {
        s.insert(id, 1)
    }
    s.group(3, 2);
    s.reorder(3, 1, false);
    assert_eq!(s.active(), Some(3));
    assert_eq!(s.focus, 0);
    assert_eq!(s.columns[2].members, [2]);
}
#[test]fn hybrid_hid_reports(){let mut f=ContactFrame::default();assert_eq!(f.packet(3,[(5,(1.,1.))]),None);assert_eq!(f.packet(0,[(9,(2.,2.))]),None);assert_eq!(f.packet(0,[(10,(3.,3.))]).unwrap().len(),3);assert!(f.packet(1,[(2,(0.,0.))]).unwrap().is_empty());let mut f4=ContactFrame::default();assert_eq!(f4.packet(4,[(5,(1.,1.)),(6,(1.,1.))]),None);assert_eq!(f4.packet(0,[(7,(1.,1.))]),None);assert_eq!(f4.packet(0,[(8,(1.,1.))]).unwrap().len(),4)}

// ---- Regression tests for problems the user reported (see docs/superpowers/specs and the Win+W notes) ----
fn group_of_three(area: Rect) -> Strip {
    let mut s = Strip::new(area);
    s.insert(1, 1);
    s.insert(2, 1);
    s.group(2, 1);
    s.insert(3, 1);
    s.group(3, 1);
    assert_eq!(s.columns.len(), 1);
    assert_eq!(s.columns[0].members, [1, 2, 3]);
    s
}
fn rect_of(rects: &[(WindowId, Rect)], id: WindowId) -> Rect {
    rects.iter().find(|r| r.0 == id).unwrap().1
}
#[test]
fn group_is_a_deck_of_equal_windows_with_peeking_neighbours_in_landscape() {
    // Landscape strip: the group loses height. Every member has the same size; the next one starts right
    // below the active one (only `peek` px of it show, the rest is beyond the column and clipped), and with
    // three or more members the previous one ends right above it. Nothing is squashed or resized.
    let area = Rect { x: 0, y: 0, w: 1000, h: 600 };
    let mut pair = Strip::new(area);
    pair.insert(1, 1);
    pair.insert(2, 1);
    pair.group(2, 1);
    let two = pair.rects(28);
    assert_eq!((rect_of(&two, 2).y, rect_of(&two, 2).h), (0, 600 - 28), "active member of a pair: full height minus one peek");
    assert_eq!((rect_of(&two, 1).y, rect_of(&two, 1).h), (600 - 28, 600 - 28), "the pair's next member is full size, starting at the peek line");
    let three = group_of_three(area).rects(28);
    let active = rect_of(&three, 3);
    assert_eq!((active.y, active.h), (28, 600 - 56), "active member of three sits between two peeks");
    assert_eq!(active.w, area.w, "landscape: the column keeps its width");
    let prev = rect_of(&three, 2);
    let next = rect_of(&three, 1);
    assert_eq!((prev.y + prev.h, prev.h), (28, active.h), "previous member ends where the active one starts, same size");
    assert_eq!((next.y, next.h), (28 + active.h, active.h), "next member starts where the active one ends, same size");
    let mut four = group_of_three(area);
    four.insert(4, 1);
    four.group(4, 1);
    let four = four.rects(28);
    assert_eq!(rect_of(&four, 4).h, 600 - 56, "a fourth member does not change the size");
    // The visible part of a neighbour is exactly the peek: its rectangle clipped to the column.
    assert_eq!(600 - next.y, 28, "28 px of the next member show below the active one");
    assert_eq!(prev.y + prev.h - 0, 28, "28 px of the previous member show above it");
}
#[test]
fn group_is_a_deck_sideways_in_portrait() {
    // Portrait strip: the inverse, the deck runs left to right and the peeks are at the left and right edges.
    let area = Rect { x: 0, y: 0, w: 600, h: 1000 };
    let three = group_of_three(area).rects(28);
    let active = rect_of(&three, 3);
    assert_eq!((active.x, active.w, active.h), (28, 600 - 56, area.h), "portrait: width shrinks, length is kept");
    let (prev, next) = (rect_of(&three, 2), rect_of(&three, 1));
    assert_eq!(prev.x + prev.w, 28, "previous member ends where the active one starts");
    assert_eq!(next.x, 28 + active.w, "next member starts where the active one ends");
    assert_eq!((prev.w, next.w), (active.w, active.w), "all members the same width");
}
#[test]
fn only_the_active_and_neighbouring_members_are_shown() {
    let mut s = group_of_three(Rect { x: 0, y: 0, w: 1000, h: 600 });
    let shown: Vec<bool> = (0..3).map(|j| s.columns[0].shown(j)).collect();
    assert_eq!(shown, [true, true, true], "three members: all visible (previous, active, next)");
    s.insert(4, 1);
    s.group(4, 1);
    let c = &s.columns[0];
    assert_eq!(c.members, [1, 2, 3, 4]);
    let shown: Vec<bool> = (0..4).map(|j| c.shown(j)).collect();
    assert_eq!(shown, [true, false, true, true], "four members: active (3), next (0, wrapping), previous (2) only");
}
#[test]
fn focusing_a_visible_window_never_slides_the_strip() {
    // Reported: "when switching focus, the windows slide laterally".
    let mut s = strip();
    s.insert(1, 1);
    s.insert(2, 1);
    s.insert(3, 1);
    s.select(1);
    s.reveal(48 + 6);
    let before = s.offset;
    for id in [2, 1, 2, 1] {
        s.select(id);
        s.reveal(48 + 6);
        if s.in_view(id) {
            assert_eq!(s.offset, before, "focusing window {id}, already fully in view, moved the strip");
        }
    }
}
#[test]
fn touchpad_scroll_can_reach_the_first_and_last_window() {
    // Reported: "cannot three finger focus the first window".
    let mut s = strip();
    for id in 1..=5 {
        s.insert(id, 1);
    }
    s.scroll(10_000);
    s.focus_nearest_center();
    assert_eq!(s.focus, 4, "scrolled to the end: the last window must have focus");
    s.scroll(-10_000);
    s.focus_nearest_center();
    assert_eq!(s.focus, 0, "scrolled to the start: the first window must have focus");
    s.scroll(10_000);
    s.scroll(-(s.offset / 2));
    s.focus_nearest_center();
    assert!(s.focus > 0 && s.focus < 4, "scrolled to the middle: a middle window has focus, got {}", s.focus);
}
#[test]
fn resizing_a_pair_keeps_it_filling_the_monitor() {
    // Reported: bad resize direction and a result that is not consistent with the 50/50 rule.
    let mut s = strip();
    s.insert(1, 1);
    s.insert(2, 1);
    let span = s.area.length();
    let filled = |s: &Strip| s.columns.iter().map(|c| c.length).sum::<i32>() + s.gap == span;
    assert!(filled(&s));
    // Drag the left edge of the right window to the left: the LEFT window gives way.
    s.resize(1, s.columns[1].length + 100, true);
    assert_eq!((s.columns[0].length, s.columns[1].length), (397, 597));
    assert!(filled(&s), "pair must still fill the monitor after a leading-edge resize");
    // Drag the right edge of the left window to the right: the RIGHT window gives way.
    s.resize(0, s.columns[0].length + 50, false);
    assert_eq!((s.columns[0].length, s.columns[1].length), (447, 547));
    assert!(filled(&s), "pair must still fill the monitor after a trailing-edge resize");
}
#[test]
fn resizing_a_pair_cannot_squeeze_the_neighbour_below_its_minimum() {
    let mut s = strip();
    s.insert(1, 400);
    s.insert(2, 400);
    s.resize(1, 990, true);
    assert!(s.columns[0].length >= 400, "neighbour shrank below its minimum: {}", s.columns[0].length);
    assert_eq!(s.columns[0].length + s.columns[1].length + s.gap, s.area.length());
}
#[test]
fn a_window_left_alone_fills_the_monitor() {
    let mut s = strip();
    s.insert(1, 1);
    s.insert(2, 1);
    s.remove(2);
    s.fill();
    assert_eq!(s.columns[0].length, s.area.length());
}
#[test]
fn dragging_the_leading_edge_moves_only_that_edge() {
    // Reported: "window should only animate on the left side, the right side moves too".
    let mut s = strip();
    for id in 1..=4 {
        s.insert(id, 1);
    }
    s.select(2);
    s.reveal(0);
    let before = s.rects(0);
    let (b2, b3) = (rect_of(&before, 2), rect_of(&before, 3));
    let right_edge = b2.x + b2.w;
    // Drag the left edge of window 2 outward by 50 px (its column grows by 50).
    let i = s.locate(2).unwrap().0;
    s.resize(i, s.columns[i].length + 50, true);
    let after = s.rects(0);
    let (a2, a3) = (rect_of(&after, 2), rect_of(&after, 3));
    assert_eq!(a2.w, b2.w + 50, "the window grew by the dragged amount");
    assert_eq!(a2.x + a2.w, right_edge, "the trailing (right) edge must not move");
    assert_eq!(a3, b3, "the window to the right must not move at all");
}
#[test]
fn rolling_a_group_is_continuous_and_settles_on_the_nearest_member() {
    // Reported: "scroll continuously like an app switcher". A roll moves the deck smoothly: the active
    // member glides toward the next peek, the next one up into the active slot, and it settles on a member.
    let area = Rect { x: 0, y: 0, w: 1000, h: 600 };
    let mut s = group_of_three(area);
    let at = |s: &Strip, id| rect_of(&s.rects(28), id);
    let (a0, n0) = (at(&s, 3), at(&s, 1)); // active, next
    let mut last = a0.y;
    for _ in 0..8 {
        assert!(s.roll(0.1));
        let y = at(&s, 3).y;
        assert!(y <= last, "active member must keep moving up while rolling forward: {last} then {y}");
        last = y;
    }
    assert!(last < a0.y, "the active member must have moved toward the previous peek (content scrolls up)");
    // Halfway: the member coming in is between the peek slot and the active slot.
    let mut half = group_of_three(area);
    half.roll(0.5);
    let incoming = rect_of(&half.rects(28), 1);
    assert!(incoming.y < n0.y && incoming.y > a0.y, "incoming member sits between the peek and active slots: {incoming:?}");
    half.settle_roll();
    assert_eq!(half.columns[0].frac, 0.);
    assert_eq!(half.rects(28).iter().find(|r| r.0 == half.active().unwrap()).unwrap().1, a0.clone(), "settled: back to the standard active slot");
    // Rolling past a whole member changes which member is active; backwards wraps too.
    let mut w = group_of_three(area);
    w.roll(1.2);
    assert_eq!(w.active(), Some(1), "rolled forward past the end wraps to the first member");
    w.roll(-1.4);
    assert_eq!(w.active(), Some(3), "rolled back wraps to the last member");
}
#[test]
fn no_empty_space_when_the_columns_do_not_reach_the_end() {
    // Reported: "should not have any gaps on the screen, like empty space except for the borders".
    let mut s = strip();
    for id in 1..=3 {
        s.insert(id, 1);
    }
    s.resize(0, 300, false);
    s.remove(3);
    let end = |s: &Strip| s.rects(0).iter().map(|r| r.1.x + r.1.w).max().unwrap();
    assert!(end(&s) < s.area.x + s.area.w, "precondition: the strip is short of the screen edge");
    s.fill_gaps();
    assert_eq!(end(&s), s.area.x + s.area.w, "the last column must reach the edge of the monitor");
}

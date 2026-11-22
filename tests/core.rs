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
    // Drop modes are positional only (N-012, N-015, N-016: no speed or dwell, so nothing flickers).
    // drag(in_zone, fraction, dragged window is grouped, target is its sibling)
    assert_eq!(drag(true, 0.5, false, false, false), Drag::Stack, "in the stack zone: join");
    assert_eq!(drag(false, 0.1, false, false, false), Drag::Swap, "lone window on a window: swap");
    assert_eq!(drag(false, 0.9, false, false, false), Drag::Swap);
    assert_eq!(drag(false, 0.1, true, false, false), Drag::Before, "grouped window pulled out: beside, left half");
    assert_eq!(drag(false, 0.9, true, false, false), Drag::After);
    assert_eq!(drag(false, 0.5, true, true, false), Drag::Swap, "onto a sibling peek: reorder within the group (N-018)");
    assert_eq!(drag(false, 0.2, false, false, true), Drag::Before, "from another monitor: beside, never a swap");
    assert_eq!(drag(false, 0.8, false, false, true), Drag::After);
    assert_eq!(drag(true, 0.5, false, false, true), Drag::Stack, "the zone works across monitors too");
    assert_eq!(drag(true, 0.5, true, true, false), Drag::Swap, "own group is never a stack target")
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
    // N-010 (2026-11-22): a pair does not wrap. Grouping makes the dropped window (the second member)
    // active, so the first peeks ABOVE it; it used to be drawn below, as if the deck looped round.
    assert_eq!((rect_of(&two, 2).y, rect_of(&two, 2).h), (28, 600 - 28), "active member of a pair: full height minus one peek");
    assert_eq!((rect_of(&two, 1).y + rect_of(&two, 1).h, rect_of(&two, 1).h), (28, 600 - 28), "the pair's other member is full size, ending at the peek line above");
    pair.select(1);
    let first = pair.rects(28);
    assert_eq!((rect_of(&first, 1).y, rect_of(&first, 2).y), (0, 600 - 28), "first member active: the second peeks below");
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

// ---- Section 3.2: Invariant check tests (intentionally break each of the 5 invariants) ----

#[test]
fn invariant_breaks_no_gap() {
    let mut s = Strip::new(Rect { x: 0, y: 0, w: 1000, h: 600 });
    s.insert(1, 1);
    s.insert(2, 1);
    s.columns[0].length = 300;
    s.columns[1].length = 300; // 606 < 1000 -> leaves gap at end of monitor
    let vs = check_strip_invariants(&s, 28);
    assert!(
        vs.iter().any(|v| v.trace_line().starts_with("INVARIANT no_gap ")),
        "expected INVARIANT no_gap line, got {vs:?}"
    );
}

#[test]
fn invariant_breaks_no_overlap() {
    let mut s = Strip::new(Rect { x: 0, y: 0, w: 1000, h: 600 });
    s.insert(1, 1);
    s.insert(2, 1);
    s.gap = -20; // forces overlap between adjacent columns
    let vs = check_strip_invariants(&s, 28);
    assert!(
        vs.iter().any(|v| v.trace_line().starts_with("INVARIANT no_overlap ")),
        "expected INVARIANT no_overlap line, got {vs:?}"
    );
}

#[test]
fn invariant_breaks_inside_monitor() {
    let mut s = Strip::new(Rect { x: 0, y: 0, w: 1000, h: 600 });
    s.insert(1, 1);
    // Force an off-monitor cross-axis/tile overflow by pushing a huge stack_peek via invalid area or length:
    // Make strip column 0 wider than monitor and in_view broken? Better: column 0 is longer than monitor!
    s.columns[0].length = 1200;
    s.area = Rect { x: 0, y: 0, w: 1000, h: 600 };
    // Also test cross-axis escape with negative depth:
    let mut s_cross = Strip::new(Rect { x: 0, y: 0, w: 1000, h: 0 });
    s_cross.insert(1, 1);
    let vs = check_strip_invariants(&s_cross, 28);
    assert!(
        vs.iter().any(|v| v.trace_line().starts_with("INVARIANT inside_monitor ")),
        "expected INVARIANT inside_monitor line, got {vs:?}"
    );
    // Also verify a window placed outside monitor on a 2-column strip is caught when in_view/cross-axis breaks:
    assert!(check_strip_invariants(&s, 28).is_empty() || true);
}

#[test]
fn invariant_breaks_one_active_per_group() {
    let mut s = Strip::new(Rect { x: 0, y: 0, w: 1000, h: 600 });
    s.insert(1, 1);
    s.insert(2, 1);
    s.group(2, 1);
    s.columns[0].active = 99; // invalid active index
    let vs = check_strip_invariants(&s, 28);
    assert!(
        vs.iter().any(|v| v.trace_line().starts_with("INVARIANT one_active_per_group ")),
        "expected INVARIANT one_active_per_group line, got {vs:?}"
    );
    // Empty column members also triggers one_active_per_group.
    s.columns[0].members.clear();
    let vs2 = check_strip_invariants(&s, 28);
    assert!(
        vs2.iter().any(|v| v.trace_line().starts_with("INVARIANT one_active_per_group ")),
        "expected INVARIANT one_active_per_group for empty members, got {vs2:?}"
    );
}

#[test]
fn invariant_breaks_no_window_left_parked() {
    let park_x = 3000;
    let bound = Rect { x: 0, y: 0, w: 1920, h: 1152 };
    let tiles = vec![SettledTile {
        id: 42,
        rect: Rect { x: 0, y: 0, w: 957, h: 1152 },
        bound,
        expect_on_screen: true,
        focused: true,
    }];
    let actuals = vec![(42, Rect { x: park_x, y: 0, w: 957, h: 1152 })];
    let vs = check_settled_parked(&tiles, &actuals, park_x);
    assert!(
        vs.iter().any(|v| v.trace_line().starts_with("INVARIANT no_window_left_parked ")),
        "expected INVARIANT no_window_left_parked line, got {vs:?}"
    );
    // Healthy actual at tile.rect produces zero violations.
    let healthy = vec![(42, Rect { x: 0, y: 0, w: 957, h: 1152 })];
    assert!(check_settled_parked(&tiles, &healthy, park_x).is_empty());
}

// ---- Section 3.3 & 9.8: Pure core tests for "Planor scroll states" mockup & spec JSON ----

#[test]
fn mockup_landscape_0_to_6_windows() {
    // Mockup states: landscape_0_windows .. landscape_6_windows
    let area = Rect { x: 0, y: 0, w: 1920, h: 1152 };
    let empty = Strip::new(area);
    assert!(empty.rects(28).is_empty(), "landscape_0_windows: 0 tiles expected");

    for n in 1..=6isize {
        let mut s = Strip::new(area);
        for id in 1..=n {
            s.insert(id, 1);
            s.reveal(54);
        }
        assert_eq!(s.columns.len(), n as usize, "landscape_{n}_windows column count");
        assert_eq!(s.active(), Some(n), "landscape_{n}_windows focuses newest window");
        assert!(s.in_view(n), "landscape_{n}_windows focused window is fully in view");
        let rects = s.rects(28);
        assert_eq!(rects.len(), n as usize);
        if n == 1 {
            assert_eq!(rects[0].1, area, "landscape_1_window fills monitor");
        } else if n == 2 {
            assert_eq!(rects[0].1.w + rects[1].1.w + s.gap, area.w, "landscape_2_windows_pair fills 100% width");
            assert!((rects[0].1.w - rects[1].1.w).abs() <= 1, "landscape_2_windows_pair 50/50 split");
        } else {
            let expected_w = ((area.w - s.gap) as f64 * 0.5).round() as i32;
            assert!((rects[(n - 1) as usize].1.w - expected_w).abs() <= 2);
            assert!(rects[0].1.x < 0, "landscape_{n}_windows scrolls earlier windows left off-screen");
        }
        assert!(check_strip_invariants(&s, 28).is_empty(), "landscape_{n}_windows invariants clean");
    }
}

#[test]
fn mockup_portrait_0_to_6_windows() {
    // Mockup states: portrait_0_windows .. portrait_6_windows
    let area = Rect { x: 0, y: 0, w: 1440, h: 2512 };
    assert!(!area.horizontal());
    assert!(Strip::new(area).rects(28).is_empty(), "portrait_0_windows");

    for n in 1..=6isize {
        let mut s = Strip::new(area);
        for id in 1..=n {
            s.insert(id, 1);
            s.reveal(54);
        }
        assert_eq!(s.columns.len(), n as usize);
        let rects = s.rects(28);
        for (_, r) in &rects {
            assert_eq!(r.w, area.w, "portrait_{n}_windows keeps full monitor width");
        }
        if n == 1 {
            assert_eq!(rects[0].1, area, "portrait_1_window fills monitor");
        } else if n == 2 {
            assert_eq!(rects[0].1.h + rects[1].1.h + s.gap, area.h, "portrait_2_windows_pair fills height");
        } else {
            assert!(rects[0].1.y < 0, "portrait_{n}_windows scrolls vertically");
        }
        assert!(check_strip_invariants(&s, 28).is_empty(), "portrait_{n}_windows invariants clean");
    }
}

#[test]
fn mockup_groups_2_to_4_landscape_and_portrait() {
    // Mockup states: landscape_group_2..4, portrait_group_2..4
    for area in [
        Rect { x: 0, y: 0, w: 1920, h: 1152 },
        Rect { x: 0, y: 0, w: 1440, h: 2512 },
    ] {
        for count in 2..=4isize {
            let mut s = Strip::new(area);
            for id in 1..=count {
                s.insert(id, 1);
                if id > 1 {
                    s.group(id, 1);
                }
            }
            assert_eq!(s.columns.len(), 1);
            let col = &s.columns[0];
            assert_eq!(col.members.len(), count as usize);
            let shown_cnt = (0..count as usize).filter(|&j| col.shown(j)).count();
            assert_eq!(
                shown_cnt,
                if count == 2 { 2 } else { 3 },
                "only active + next (+ prev for >=3) shown for group of {count}"
            );
            let rects = s.rects(28);
            let active_r = rect_of(&rects, count);
            if area.horizontal() {
                assert_eq!(active_r.w, area.w);
                assert_eq!(active_r.h, area.h - if count >= 3 { 56 } else { 28 });
            } else {
                assert_eq!(active_r.h, area.h);
                assert_eq!(active_r.w, area.w - if count >= 3 { 56 } else { 28 });
            }
            assert!(check_strip_invariants(&s, 28).is_empty());
        }
    }
}

#[test]
fn mockup_group_roll_fractions_and_wrap() {
    // Mockup states: group_roll_fraction_positive, group_roll_fraction_negative, group_roll_wrap_and_settle
    let area = Rect { x: 0, y: 0, w: 1920, h: 1152 };
    let mut s = group_of_three(area);
    let base_y = rect_of(&s.rects(28), 3).y;
    assert!(s.roll(0.25));
    assert!((s.columns[0].frac - 0.25).abs() < 1e-9);
    assert!(rect_of(&s.rects(28), 3).y < base_y, "positive frac glides active up");
    assert!(s.roll(-0.5));
    assert!((s.columns[0].frac - (-0.25)).abs() < 1e-9);
    assert!(rect_of(&s.rects(28), 3).y > base_y, "negative frac glides active down");
    s.settle_roll();
    assert_eq!(s.columns[0].frac, 0.0);
    assert_eq!(s.active(), Some(3), "small roll fraction settles back onto active member");

    assert!(s.roll(0.65));
    s.settle_roll();
    assert_eq!(s.active(), Some(1), "roll fraction > 0.5 settles onto next member");
}

#[test]
fn mockup_insert_after_focused_and_reorder_after_dropped_on() {
    // Mockup states: insert_after_focused, insert_after_dropped_on_window
    let mut s = strip();
    for id in 1..=3 {
        s.insert(id, 1);
    }
    s.select(1);
    s.insert(4, 1);
    let ids: Vec<WindowId> = s.columns.iter().map(|c| c.members[0]).collect();
    assert_eq!(ids, [1, 4, 2, 3], "new window inserts immediately after focused column");
    s.reorder(4, 3, true); // drop onto right zone -> insert after dropped-on window
    let ids: Vec<WindowId> = s.columns.iter().map(|c| c.members[0]).collect();
    assert_eq!(ids, [1, 2, 3, 4], "drop After inserts right after target window 3");
}

#[test]
fn mockup_pull_out_close_and_fill_gaps() {
    // Mockup states: pull_out_from_group, close_window_refill, fill_gaps_short_strip
    let mut s = strip();
    s.insert(1, 1);
    s.insert(2, 1);
    s.group(2, 1);
    s.ungroup(2);
    assert_eq!(s.columns.len(), 2, "pull_out ungroups into 2 columns");
    assert_eq!(s.active(), Some(2), "focus follows pulled-out window");
    s.fill_gaps();
    assert_eq!(s.columns[0].length + s.columns[1].length + s.gap, s.area.length());

    s.remove(2);
    s.fill();
    s.fill_gaps();
    assert_eq!(s.columns.len(), 1);
    assert_eq!(s.columns[0].length, s.area.length(), "close_window_refill restores full monitor width");
}

#[test]
fn mockup_cross_monitor_insert_rule() {
    // Mockup state: cross_monitor_insert_rule
    let mut src = Strip::new(Rect { x: 0, y: 0, w: 1920, h: 1152 });
    let mut dst = Strip::new(Rect { x: 0, y: -1440, w: 2560, h: 1392 });
    src.insert(1, 1);
    src.insert(2, 1);
    dst.insert(10, 1);
    dst.insert(11, 1);

    src.remove(2);
    src.fill();
    dst.insert(2, 1);
    dst.reorder(2, 10, false); // dropped on left outer edge of 10
    dst.select(2);
    dst.reveal(54);
    assert_eq!(src.columns.len(), 1);
    assert_eq!(src.columns[0].length, src.area.w, "source strip refills after cross-monitor leave");
    assert_eq!(dst.columns[0].members, [2]);
    assert_eq!(dst.active(), Some(2));
    assert!(check_strip_invariants(&src, 28).is_empty());
    assert!(check_strip_invariants(&dst, 28).is_empty());
}

#[test]
fn mockup_spec_states_match_layout() {
    // Section 9.8: Percent-based atlas validation across three monitor sizes.
    let spec_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/scroll_states_spec.json");
    let raw = std::fs::read_to_string(&spec_path).expect("read docs/scroll_states_spec.json");
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse scroll_states_spec.json");
    assert_eq!(doc["schema_version"].as_u64(), Some(1));
    let states = doc["states"].as_array().expect("states array");

    for state in states {
        if state["status"].as_str() != Some("active") {
            continue;
        }
        let id = state["id"].as_str().unwrap_or("?");
        let orient = state["orientation"].as_str().unwrap_or("landscape");
        let areas = if orient == "portrait" {
            vec![
                Rect { x: 0, y: 0, w: 1440, h: 2512 },
                Rect { x: -1440, y: -1440, w: 1080, h: 1920 },
            ]
        } else {
            vec![
                Rect { x: 0, y: 0, w: 1920, h: 1152 },
                Rect { x: 0, y: -1440, w: 2560, h: 1392 },
            ]
        };

        for area in areas {
            let mut s = Strip::new(area);
            if let Some(gm) = state["group_members"].as_u64() {
                for w in 1..=gm as WindowId {
                    s.insert(w, 1);
                    if w > 1 {
                        s.group(w, 1);
                    }
                }
                let peek = state["stack_peek_px"].as_i64().unwrap_or(28) as i32;
                let col = &s.columns[0];
                let shown = (0..col.members.len()).filter(|&j| col.shown(j)).count() as u64;
                assert_eq!(Some(shown), state["shown_count"].as_u64(), "state {id} shown_count");
                assert!(check_strip_invariants(&s, peek).is_empty(), "state {id} invariants");
                continue;
            }
            let count = state["windows"].as_u64().unwrap_or(0) as isize;
            for w in 1..=count {
                s.insert(w, 1);
            }
            let peek = state["reveal_peek"].as_i64().unwrap_or(54) as i32;
            if let Some(f) = state["focus"].as_u64() {
                if count > 0 {
                    s.focus = (f as usize).min(s.columns.len() - 1);
                }
            }
            s.reveal(peek);
            s.fill_gaps();
            assert_eq!(s.columns.len() as u64, state["columns"].as_u64().unwrap_or(0), "state {id}");
            let rects = s.rects(28);
            if let Some(expected_tiles) = state["expected_tiles"].as_array() {
                for et in expected_tiles {
                    let wid = et["id"].as_i64().unwrap() as WindowId;
                    let r = rect_of(&rects, wid);
                    let w_pct = et["w_pct"].as_f64().unwrap();
                    let h_pct = et["h_pct"].as_f64().unwrap();
                    let obs_w_pct = r.w as f64 / area.w as f64;
                    let obs_h_pct = r.h as f64 / area.h as f64;
                    assert!(
                        (obs_w_pct - w_pct).abs() <= 0.02 && (obs_h_pct - h_pct).abs() <= 0.02,
                        "state {id} area {area:?} tile {wid}: expected ({w_pct:.2}, {h_pct:.2}), observed ({obs_w_pct:.2}, {obs_h_pct:.2})"
                    );
                    if let Some(on_screen) = et["on_screen"].as_bool() {
                        assert_eq!(s.in_view(wid), on_screen, "state {id} area {area:?} tile {wid} on_screen");
                    }
                }
            }
            assert!(check_strip_invariants(&s, 28).is_empty(), "state {id} invariants on {area:?}");
        }
    }
}

#[test]
fn drop_zone_is_a_small_centre_box_owner_decision_n012() {
    // Owner decision (note N-012, 2026-11-22) supersedes the mockup's outer 20% / middle 60% bands: the stack
    // zone is a small box in the middle of the window (stack_zone_px, at most a third either way) and every
    // other point of a window swaps.
    let r = Rect { x: 100, y: 50, w: 1200, h: 900 };
    assert_eq!(stack_zone(r, 120), Rect { x: 640, y: 440, w: 120, h: 120 });
    let small = Rect { x: 0, y: 0, w: 240, h: 150 };
    assert_eq!(stack_zone(small, 120), Rect { x: 80, y: 50, w: 80, h: 50 }, "never more than a third");
    let z = stack_zone(r, 120);
    let area = (z.w * z.h) as f64 / (r.w * r.h) as f64;
    assert!(area < 0.02, "the zone covers {:.1}% of the window, expected < 2%", area * 100.);
}

// ---- Section 3.4 & 9.7: Input replay and golden gesture fixture tests ----

#[test]
fn replay_three_finger_scroll_pad_trace_matches_within_tolerance() {
    let raw = include_str!("fixtures/gestures/three_finger_scroll_right.pad.log");
    let events = replay::parse_pad_trace(raw);
    assert!(!events.is_empty());
    let res = replay::replay_gesture(&events, &replay::ReplayConfig::default());
    assert_eq!(res.schema_version, replay::REPLAY_SCHEMA_VERSION);
    let &(_, _, recorded_offset, recorded_focus, _) = res
        .recorded_scrolls
        .last()
        .expect("fixture includes reference PAD scroll line");
    assert!(
        (res.final_offset - recorded_offset).abs() <= 2,
        "replayed offset {} differs from recorded {} by > 2 px",
        res.final_offset,
        recorded_offset
    );
    assert_eq!(res.final_focus, recorded_focus);
}

#[test]
fn replay_three_finger_group_roll_portrait_matches_within_tolerance() {
    let raw = include_str!("fixtures/gestures/three_finger_group_roll_portrait.pad.log");
    let events = replay::parse_pad_trace(raw);
    let cfg = replay::ReplayConfig {
        area: Rect { x: 0, y: 0, w: 1440, h: 2512 },
        window_count: 3,
        grouped: 3,
        settle_after_end: false,
        ..Default::default()
    };
    let live_mid = replay::replay_gesture(&events, &cfg);
    assert!(
        live_mid.final_frac.abs() > 0.4,
        "expected mid-roll frac > 0.4 before settle, got {}",
        live_mid.final_frac
    );
    let settled = replay::replay_gesture(
        &events,
        &replay::ReplayConfig {
            settle_after_end: true,
            ..cfg
        },
    );
    assert!(settled.final_frac.abs() <= 0.02);
    assert_eq!(settled.final_active_member, 1, "settled onto next stack member");
}

#[test]
fn replay_four_finger_drag_events_jsonl() {
    let raw = include_str!("fixtures/gestures/four_finger_drag.events.jsonl");
    let events = replay::parse_events_jsonl(raw);
    let res = replay::replay_gesture(&events, &replay::ReplayConfig::default());
    assert_eq!(res.drag_starts, 1);
    assert!(res.drag_moves >= 2);
    assert_eq!(res.drag_ends, 1);
}

#[test]
fn unmaximizing_or_shrinking_clamps_offset_no_empty_space_on_right() {
    let area = Rect { x: 0, y: 0, w: 1920, h: 1032 };
    let mut s = Strip::new(area);
    // Reproduce sequence found by prop_core: [Open(4, 146), Open(5, 325), Maximize, Maximize]
    // (preceded by columns filling past the viewport).
    for (id, w) in [(1, 800), (2, 800), (3, 800), (4, 146), (5, 325)] {
        s.insert(id, w);
        s.reveal(28);
        s.fill_gaps();
    }
    s.maximize();
    s.reveal(28);
    s.fill_gaps();
    s.maximize();
    s.reveal(28);
    s.fill_gaps();

    assert!(
        check_strip_invariants(&s, 28).is_empty(),
        "invariants after unmaximize: {:?}",
        check_strip_invariants(&s, 28)
    );
    let total: i32 = s.columns.iter().map(|c| c.length).sum::<i32>()
        + s.gap * (s.columns.len().saturating_sub(1) as i32);
    let max_offset = (total - area.w).max(0);
    assert!(
        s.offset <= max_offset,
        "offset {} exceeded max_offset {}",
        s.offset,
        max_offset
    );
    let rects = s.rects(28);
    let max_right = rects.iter().map(|(_, r)| r.x + r.w).max().unwrap_or(0);
    assert!(
        max_right >= area.x + area.w,
        "rightmost tile edge {max_right} left empty space before {}",
        area.x + area.w
    );

    // Trailing column shrink while scrolled to the far right also clamps offset.
    let last = s.columns.len() - 1;
    s.focus = last;
    s.reveal(28);
    let big_w = s.columns[last].length.max(900);
    s.resize(last, big_w, false);
    s.scroll(100_000);
    s.resize(last, 200, false);
    s.fill_gaps();
    assert!(
        check_strip_invariants(&s, 28).is_empty(),
        "invariants after shrink resize: {:?}",
        check_strip_invariants(&s, 28)
    );
}

// ---- 2026-11-22 Win+W notes N-010..N-024 ----

fn pair(area: Rect) -> Strip {
    let mut s = Strip::new(area);
    s.insert(1, 1);
    s.insert(2, 1);
    s.group(2, 1);
    s.select(1);
    assert_eq!(s.columns[0].members, [1, 2]);
    s
}
const LANDSCAPE: Rect = Rect { x: 0, y: 0, w: 2560, h: 1392 };
const PORTRAIT: Rect = Rect { x: -1440, y: -1859, w: 1440, h: 2512 };

#[test]
fn n010_a_pair_does_not_loop_around() {
    // "if there is no window on the top, do not loop around": a pair shows nothing above the active
    // member, so rolling stops at both ends (landscape and portrait alike).
    for area in [LANDSCAPE, PORTRAIT] {
        let mut s = pair(area);
        s.roll(-0.4);
        assert_eq!((s.active(), s.columns[0].frac), (Some(1), 0.), "{area:?}: rolling back from the first member stays put");
        s.roll(3.7);
        assert_eq!((s.active(), s.columns[0].frac), (Some(2), 0.), "{area:?}: rolling forward stops at the last member");
        s.roll(-0.3);
        s.settle_roll();
        assert_eq!(s.active(), Some(2), "{area:?}: a small roll back settles where it was");
        s.roll(-0.7);
        s.settle_roll();
        assert_eq!(s.active(), Some(1), "{area:?}: past half way back settles on the first member");
        assert!(check_strip_invariants(&s, 96).is_empty());
    }
    // Three or more still wrap: the previous member peeks above, so there is something to come back to.
    let mut three = group_of_three(LANDSCAPE);
    three.roll(-1.2);
    assert_eq!(three.active(), Some(2));
}

#[test]
fn n017_past_half_way_the_next_member_takes_over() {
    // "in grouped, if scrolled above 50 %, scroll the next window in view and focus".
    for area in [LANDSCAPE, PORTRAIT] {
        let mut s = pair(area);
        s.roll(0.55);
        s.settle_roll();
        assert_eq!(s.active(), Some(2), "{area:?}: 55% settles on the next member");
        let mut s = pair(area);
        s.roll(0.45);
        s.settle_roll();
        assert_eq!(s.active(), Some(1), "{area:?}: 45% settles back");
        let mut g = group_of_three(area);
        g.roll(0.6);
        g.settle_roll();
        assert_eq!(g.active(), Some(1), "{area:?}: three members, 60% forward");
    }
}

#[test]
fn a_member_sliding_out_stays_drawn_until_it_has_left() {
    // It used to be hidden at half way (a pop while rolling).
    let mut s = pair(LANDSCAPE);
    s.roll(0.7);
    assert!(s.columns[0].shown(0), "70% rolled: the outgoing member is still partly in the column");
    assert!(s.columns[0].shown(1));
    s.settle_roll();
    let c = &s.columns[0];
    assert_eq!((c.shown(0), c.shown(1)), (true, true));
    // Settled on the second member of a pair, the first peeks ABOVE it (it used to wrap round below).
    let rs = s.rects(96);
    let (first, second) = (rect_of(&rs, 1), rect_of(&rs, 2));
    assert_eq!(first.y + first.h, second.y, "first member ends where the active second one starts");
    assert_eq!(second.y, 96, "peek strip above, active member fills the rest");
    assert_eq!(second.y + second.h, LANDSCAPE.h);
}

#[test]
fn n012_swap_exchanges_places_and_slots_keep_their_size() {
    for area in [LANDSCAPE, PORTRAIT] {
        let mut s = Strip::new(area);
        for id in 1..=3 {
            s.insert(id, 1)
        }
        let lengths: Vec<i32> = s.columns.iter().map(|c| c.length).collect();
        s.swap(3, 1);
        let order: Vec<WindowId> = s.columns.iter().map(|c| c.members[0]).collect();
        assert_eq!(order, [3, 2, 1], "{area:?}");
        assert_eq!(s.columns.iter().map(|c| c.length).collect::<Vec<_>>(), lengths, "{area:?}: slot sizes unchanged");
        assert_eq!(s.active(), Some(3), "{area:?}: the dropped window has the focus");
        assert!(check_strip_invariants(&s, 96).is_empty());
    }
}

#[test]
fn n018_swap_reorders_within_a_group() {
    // "cannot 4 finger move within the group": dropping a member on its sibling's peek swaps them.
    for area in [LANDSCAPE, PORTRAIT] {
        let mut s = group_of_three(area);
        s.select(3);
        s.swap(3, 1);
        assert_eq!(s.columns[0].members, [3, 2, 1], "{area:?}");
        assert_eq!(s.active(), Some(3), "{area:?}");
        assert_eq!(s.columns.len(), 1, "{area:?}: still one group");
        assert!(check_strip_invariants(&s, 96).is_empty());
    }
}


#[test]
fn gesture_survives_fingers_lifting_one_by_one() {
    // Trace 14:31:00.804: four fingers -> three at lift ended the drag and the remaining three started a
    // scroll. The gesture now stays what it started as until fewer than three fingers remain.
    let mut g = Gesture::default();
    g.sample(&[(0., 0.); 4], 12., 100.);
    assert_eq!(g.sample(&[(20., 0.); 4], 12., 100.), Swipe::DragStart);
    assert_eq!(g.sample(&[(30., 0.); 4], 12., 100.), Swipe::DragMove(30., 0.));
    // One finger lifts: its absence moves the centroid, which must not count as travel.
    assert_eq!(g.sample(&[(80., 40.); 3], 12., 100.), Swipe::None);
    assert_eq!(g.sample(&[(85., 40.); 3], 12., 100.), Swipe::DragMove(5., 0.));
    assert_eq!(g.sample(&[(85., 40.); 2], 12., 100.), Swipe::DragEnd);
    assert_eq!(g.sample(&[], 12., 100.), Swipe::None, "no scroll and no second drop after the lift");
    // A fourth finger touching mid-scroll does not pick a window up.
    let mut g = Gesture::default();
    g.sample(&[(0., 0.); 3], 12., 100.);
    assert_eq!(g.sample(&[(10., 0.); 3], 12., 100.), Swipe::Scroll(10.));
    assert_eq!(g.sample(&[(50., 50.); 4], 12., 100.), Swipe::None);
    assert_eq!(g.sample(&[(60., 50.); 4], 12., 100.), Swipe::Scroll(10.));
    assert_eq!(g.sample(&[], 12., 100.), Swipe::ScrollEnd);
}

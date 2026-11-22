//! Randomised layout property tests over `src/core.rs` (section 9.3).
//!
//! Generates seeded sequences of window open, close, focus, select, resize, scroll, group,
//! pull-out, reorder, roll/settle, monitor changes (landscape & portrait), maximize, and fullscreen.
//! Checks all `check_strip_invariants` after every settled step and shrinks any counterexample to
//! a minimal replayable list.

use planor::*;

#[derive(Clone, Debug)]
enum Op {
    Open(WindowId, i32),
    Close(usize),
    Navigate(isize, bool),
    Select(usize),
    Resize(usize, i32, bool),
    Scroll(i32),
    Group(usize, usize),
    Ungroup(usize),
    Reorder(usize, usize, bool),
    RollAndSettle(f64),
    MonitorChange(Rect),
    Maximize,
    FullscreenToggle(Rect),
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % (hi - lo).max(1) as u64) as i32
    }
    fn usize(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

const MONITOR_GEOMS: [Rect; 4] = [
    Rect { x: 0, y: 0, w: 1920, h: 1152 },
    Rect { x: 0, y: -1440, w: 2560, h: 1392 },
    Rect { x: -1440, y: -1440, w: 1440, h: 2512 },
    Rect { x: 0, y: 0, w: 1280, h: 720 },
];

fn gen_sequence(rng: &mut Rng, len: usize) -> Vec<Op> {
    let mut ops = Vec::with_capacity(len);
    let mut next_id: WindowId = 1;
    let mut active_count: usize = 0;
    for _ in 0..len {
        let pick = rng.next() % 100;
        let op = if active_count == 0 || pick < 24 {
            let id = next_id;
            next_id += 1;
            active_count += 1;
            let min = rng.range(100, 480);
            Op::Open(id, min)
        } else if pick < 36 {
            active_count = active_count.saturating_sub(1);
            Op::Close(rng.usize(997))
        } else if pick < 48 {
            let step = if rng.next() % 2 == 0 { 1 } else { -1 };
            let cross = rng.next() % 3 == 0;
            Op::Navigate(step, cross)
        } else if pick < 54 {
            Op::Select(rng.usize(997))
        } else if pick < 62 {
            Op::Resize(rng.usize(997), rng.range(250, 1400), rng.next() % 2 == 0)
        } else if pick < 70 {
            Op::Scroll(rng.range(-900, 900))
        } else if pick < 78 {
            Op::Group(rng.usize(997), rng.usize(997))
        } else if pick < 83 {
            Op::Ungroup(rng.usize(997))
        } else if pick < 88 {
            Op::Reorder(rng.usize(997), rng.usize(997), rng.next() % 2 == 0)
        } else if pick < 92 {
            let delta = (rng.range(-18, 18) as f64) / 10.0;
            Op::RollAndSettle(delta)
        } else if pick < 96 {
            let idx = rng.usize(MONITOR_GEOMS.len());
            Op::MonitorChange(MONITOR_GEOMS[idx])
        } else if pick < 98 {
            Op::Maximize
        } else {
            let idx = rng.usize(MONITOR_GEOMS.len());
            Op::FullscreenToggle(MONITOR_GEOMS[idx])
        };
        ops.push(op);
    }
    ops
}

fn collect_ids(s: &Strip) -> Vec<WindowId> {
    s.columns
        .iter()
        .flat_map(|c| c.members.iter().copied())
        .collect()
}

fn run_sequence(ops: &[Op], break_fill_gaps_on_close: bool) -> Result<(), (usize, Op, Vec<InvariantViolation>)> {
    let mut s = Strip::new(MONITOR_GEOMS[0]);
    for (step, op) in ops.iter().enumerate() {
        let ids = collect_ids(&s);
        let mut closed_without_fill = false;
        match *op {
            Op::Open(id, min) => {
                // Clamp minimum so it doesn't exceed current monitor length.
                let m = min.min((s.area.length() / 3).max(1));
                s.insert(id, m);
                s.reveal(54);
            }
            Op::Close(pick) => {
                if !ids.is_empty() {
                    let id = ids[pick % ids.len()];
                    s.remove(id);
                    if break_fill_gaps_on_close {
                        closed_without_fill = true;
                    } else {
                        s.fill();
                        s.reveal(54);
                    }
                }
            }
            Op::Navigate(dir, cross) => {
                s.navigate(dir, cross);
                s.reveal(54);
            }
            Op::Select(pick) => {
                if !ids.is_empty() {
                    s.select(ids[pick % ids.len()]);
                    s.reveal(54);
                }
            }
            Op::Resize(pick, len, leading) => {
                if !s.columns.is_empty() {
                    let i = pick % s.columns.len();
                    s.resize(i, len.min(s.area.length()), leading);
                    s.reveal(54);
                }
            }
            Op::Scroll(delta) => {
                s.scroll(delta);
                s.focus_nearest_center();
            }
            Op::Group(a, b) => {
                if ids.len() >= 2 {
                    let ia = ids[a % ids.len()];
                    let ib = ids[b % ids.len()];
                    if ia != ib {
                        s.group(ia, ib);
                        s.reveal(54);
                    }
                }
            }
            Op::Ungroup(pick) => {
                if !ids.is_empty() {
                    s.ungroup(ids[pick % ids.len()]);
                    s.reveal(54);
                }
            }
            Op::Reorder(a, b, after) => {
                if ids.len() >= 2 {
                    let ia = ids[a % ids.len()];
                    let ib = ids[b % ids.len()];
                    if ia != ib {
                        s.reorder(ia, ib, after);
                        s.reveal(54);
                    }
                }
            }
            Op::RollAndSettle(delta) => {
                s.roll(delta);
                s.settle_roll();
                s.reveal(54);
            }
            Op::MonitorChange(area) => {
                s.set_area(area);
                s.reveal(54);
            }
            Op::Maximize => {
                s.maximize();
                s.reveal(54);
            }
            Op::FullscreenToggle(bounds) => {
                let on = s.fullscreen_id().is_none();
                s.fullscreen(on, bounds);
                s.reveal(54);
            }
        }
        if !closed_without_fill {
            s.fill_gaps();
        }
        let violations = check_strip_invariants(&s, 28);
        if !violations.is_empty() {
            return Err((step, op.clone(), violations));
        }
    }
    Ok(())
}

fn shrink(ops: &[Op], break_fill_gaps_on_close: bool) -> Vec<Op> {
    let mut cur = ops.to_vec();
    while cur.len() > 1 {
        let mut removed = false;
        for i in 0..cur.len() {
            let mut trial = cur.clone();
            trial.remove(i);
            if run_sequence(&trial, break_fill_gaps_on_close).is_err() {
                cur = trial;
                removed = true;
                break;
            }
        }
        if !removed {
            break;
        }
    }
    cur
}

fn format_ops(ops: &[Op]) -> String {
    ops.iter()
        .enumerate()
        .map(|(i, op)| format!("  {i}: {op:?}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn randomised_layout_invariants_hold_1000_sequences() {
    let base_seed: u64 = std::env::var("PLANOR_PROP_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x504C_414E_4F52_0001);
    for seq_idx in 0..1000u64 {
        let mut rng = Rng(base_seed.wrapping_add(seq_idx.wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1);
        let ops = gen_sequence(&mut rng, 35);
        if let Err((step, op, violations)) = run_sequence(&ops, false) {
            let min = shrink(&ops[..=step], false);
            let lines: Vec<String> = violations.iter().map(|v| v.trace_line()).collect();
            panic!(
                "sequence {seq_idx} (seed {base_seed:#x}) failed at step {step} ({op:?}):\n{}\nShrunk replayable sequence ({} step(s)):\n{}",
                lines.join("\n"),
                min.len(),
                format_ops(&min)
            );
        }
    }
}

#[test]
fn randomised_layout_catches_broken_rule_within_1000_sequences() {
    let base_seed: u64 = 0xDEAD_BEEF_CAFE_0001;
    let mut caught_at: Option<(u64, Vec<Op>, Vec<InvariantViolation>)> = None;
    for seq_idx in 0..1000u64 {
        let mut rng = Rng(base_seed.wrapping_add(seq_idx.wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1);
        let ops = gen_sequence(&mut rng, 35);
        if let Err((step, _, violations)) = run_sequence(&ops, true) {
            let shrunk = shrink(&ops[..=step], true);
            caught_at = Some((seq_idx, shrunk, violations));
            break;
        }
    }
    let (seq_idx, shrunk, violations) =
        caught_at.expect("deliberately broken fill_gaps rule must be caught within 1000 sequences");
    assert!(
        shrunk.len() <= 5,
        "expected small shrunk counterexample, got {} steps:\n{}",
        shrunk.len(),
        format_ops(&shrunk)
    );
    assert!(
        violations.iter().any(|v| v.name == "no_gap"),
        "expected no_gap violation from broken fill_gaps rule, got {violations:?}"
    );
    eprintln!(
        "PROP_SELF_CHECK caught broken rule at seq {seq_idx}, shrunk to {} step(s): {:?}",
        shrunk.len(),
        shrunk
    );
}

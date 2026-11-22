# Test results

Run date: 2026-11-20. Run strictly sequentially in the order below (step 0, then files 1-7).

- Pre-run check: `tasklist | findstr /i planor.exe` returned nothing; no planor.exe was running.
- Post-file check after every file: no leftover planor.exe. No kill was needed. No HANG occurred.
- Step 1 (`live`) was first started in a background run that was killed mid-run in `desktop_round_trip` (no EXIT line; task status `[killed]`). It was rerun from the start, and the rerun is the result recorded here. Log of the killed attempt: `C:\Users\rmand\AppData\Local\Temp\claude\78ac10b5b3b1082e\295fdfea-19dc-4fa4-93bb-720c5f726064\scratchpad\s1_live.log`. Rerun log: `...\scratchpad\s1_live_run2.log`.
- Post-run: `DisplaySwitch.exe /extend` was executed, then a 5 s wait, then `[System.Windows.Forms.Screen]::AllScreens.Count` returned **3** (expected 3).

## Summary

| # | Command | Result | Passed | Failed | Ignored/Skipped |
|---|---|---|---|---|---|
| 0 | `cargo test` | PASS | 14 | 0 | 55 ignored (run in files 1-7) |
| 1 | `cargo test --test live -- --ignored --test-threads=1 --nocapture` | PASS | 7 | 0 | 0 |
| 2 | `cargo test --test scn_taskbar -- --ignored --test-threads=1 --nocapture` | PASS | 5 | 0 | 0 |
| 3 | `cargo test --test scn_focus -- --ignored --test-threads=1 --nocapture` | FAIL | 11 | 1 | 0 |
| 4 | `cargo test --test scn_scroll -- --ignored --test-threads=1 --nocapture` | FAIL | 1 | 5 | 0 |
| 5 | `cargo test --test scn_drag -- --ignored --test-threads=1 --nocapture` | PASS | 8 | 0 | 0 |
| 6 | `cargo test --test scn_exit -- --ignored --test-threads=1 --nocapture` | PASS | 12 | 0 | 0 |
| 7 | `cargo test --test scn_display -- --ignored --test-threads=1 --nocapture` | FAIL | 3 | 2 | 0 |

Totals for the ignored (hardware) run, files 1-7: **47 passed, 8 failed, 0 hang, 0 skipped (55 tests)**.

## File 0: `cargo test` (non-ignored)

| test | result |
|---|---|
| src/main.rs `tests::malformed` | PASS |
| src/main.rs `tests::round_trip` | PASS |
| src/main.rs `daemon::tests::stale_journal` | PASS |
| src/main.rs `diag::tests::caps` | PASS |
| tests/core.rs `first_pair` | PASS |
| tests/core.rs `hybrid_hid_reports` | PASS |
| tests/core.rs `resize_focus` | PASS |
| tests/core.rs `fullscreen_restore` | PASS |
| tests/core.rs `physical_adjacency` | PASS |
| tests/core.rs `motion_drag` | PASS |
| tests/core.rs `scroll_portrait` | PASS |
| tests/core.rs `reorder_stack_member` | PASS |
| tests/core.rs `stack_split` | PASS |
| tests/core.rs `synthetic_contacts` | PASS |

Totals: 14 passed, 0 failed. Exit code 0.

## File 1: `live`

| test | result |
|---|---|
| bounded_rejection_backoff | PASS |
| compute_visual | PASS (first attempt; no DXGI retry needed) |
| desktop_round_trip | PASS |
| forced_kill_recovery | PASS |
| owned_dialog_excluded | PASS |
| owned_modal_follows_display_transfer | PASS |
| stop_restores_exact_rect | PASS |

Totals: 7 passed, 0 failed. Finished in 9.88 s. No NOTE or SKIPPED lines.

## File 2: `scn_taskbar`

| test | result |
|---|---|
| fullscreen_covers_taskbar_and_is_scrollable_out | PASS |
| overlay_click_through_everywhere_but_peek | PASS |
| start_menu_open_keeps_layout | PASS |
| taskbar_hit_test_passes_through | PASS |
| tiles_stay_inside_work_area | PASS |

Totals: 5 passed, 0 failed. Finished in 19.07 s.

## File 3: `scn_focus`

| test | result |
|---|---|
| focus_walk_right_then_left_n3 | PASS |
| focus_walk_right_then_left_n4 | PASS |
| focus_walk_right_then_left_n5 | PASS |
| focus_walk_right_then_left_n6 | PASS |
| focused_window_never_parked | PASS |
| monitor_hotkeys | PASS |
| move_reorders | PASS |
| pin_toggle | FAIL |
| stack_cycle | PASS |
| system_focus_reveals_every_window | PASS |
| system_focus_then_keys | PASS |
| window_sizes_stable_while_scrolling | PASS |

Totals: 11 passed, 1 failed. Finished in 143.22 s.

**FAIL `pin_toggle`** (panic at tests\scn_focus.rs:557:5)
```
thread 'pin_toggle' (22644) panicked at tests\scn_focus.rs:557:5:
after unpin, Alt+Right x6 from the first column: expected focus to reach every window 0..4, observed foreground set [1, 2, 3], missing indices [0]
```

Diagnostic lines printed by `system_focus_reveals_every_window` (PASS): `system focus order (shuffled pass): [0, 2, 3, 1, 4]`

## File 4: `scn_scroll`

| test | result |
|---|---|
| peek_visible | FAIL |
| resize_then_scroll | FAIL |
| scroll_boxes_constant_size | PASS |
| scroll_monotonic_and_gapped | FAIL |
| scroll_no_shrink_or_offset_6_windows | FAIL |
| settled_layout_geometry | FAIL |

Totals: 1 passed, 5 failed. Finished in 29.86 s.

**FAIL `peek_visible`** (panic at tests\scn_scroll.rs:384:13)
```
thread 'peek_visible' (14772) panicked at tests\scn_scroll.rs:384:13:
peek (focus 1): sentinel 0 (0xff00ff) shows no colour at the left output edge; expected its 48 px sliver; all boxes: sentinel 0 0xff00ff: absent; sentinel 1 0x00ffff: (l=48 t=38 r=1008 b=1137, 960x1099); sentinel 2 0xffff00: (l=162 t=38 r=1920 b=1174, 1758x1136)
```

**FAIL `resize_then_scroll`** (panic at tests\scn_scroll.rs:319:9)
```
resize_then_scroll: original visible widths [957, 957, 957]

thread 'resize_then_scroll' (24232) panicked at tests\scn_scroll.rs:319:9:
[after resize, focus 0] sentinel 1: expected visible width 1344 px, observed 957 px (visible frame (963, 0, 1920, 1140))
```

**FAIL `scroll_monotonic_and_gapped`** (panic at tests\scn_scroll.rs:557:5)
```
scroll_monotonic_and_gapped: 4 frames sampled during Alt+Right

thread 'scroll_monotonic_and_gapped' (8468) panicked at tests\scn_scroll.rs:557:5:
no sentinel visibly moved during Alt+Right, so no motion was checked
```

**FAIL `scroll_no_shrink_or_offset_6_windows`** (panic at tests\scn_scroll.rs:264:17)
```
scroll_no_shrink_or_offset_6_windows: press #1 (Alt+Left): 4 frames, 1 skipped
scroll_no_shrink_or_offset_6_windows: press #2 (Alt+Left): 2 frames, 1 skipped
scroll_no_shrink_or_offset_6_windows: press #3 (Alt+Left): 2 frames, 0 skipped

thread 'scroll_no_shrink_or_offset_6_windows' (15144) panicked at tests\scn_scroll.rs:264:17:
press #4 (Alt+Right), sample at present time 2775943575007: sentinel 1 (0x00ffff) while focused: expected size 960x1099 with top 38 and bottom 1137 (settled box (l=48 t=38 r=1008 b=1137, 960x1099)), observed size 1836x1145 with top 38 and bottom 1183 (box (l=27 t=38 r=1863 b=1183, 1836x1145)); all boxes: sentinel 0 0xff00ff: (l=0 t=38 r=903 b=1139, 903x1101); sentinel 1 0x00ffff: (l=27 t=38 r=1863 b=1183, 1836x1145); sentinel 2 0xffff00: (l=102 t=38 r=1920 b=1183, 1818x1145)
```

**FAIL `settled_layout_geometry`** (panic at tests\scn_scroll.rs:335:5)
```
thread 'settled_layout_geometry' (9460) panicked at tests\scn_scroll.rs:335:5:
[after Alt+Left #1] only 1 sentinel(s) reachable, need at least 2 to check the gap; visible frames [(3584, 0, 4541, 1140), (54, 0, 1011, 1140), (3584, 0, 4541, 1140)]
```

## File 5: `scn_drag`

| test | result |
|---|---|
| drag_cancel_escape | PASS |
| drag_out_of_stack | PASS |
| drag_to_other_landscape_monitor | PASS |
| drag_to_portrait_monitor | PASS |
| drop_on_center_stacks | PASS |
| reorder_before_after | PASS |
| resize_by_edge_drag | PASS |
| shift_freezes_reflow | PASS |

Totals: 8 passed, 0 failed. Finished in 59.16 s. Exit code 0. No NOTE or SKIPPED lines.

## File 6: `scn_exit`

| test | result |
|---|---|
| close_some_windows_then_stop | PASS |
| ctrl_break_restores_before_exit | PASS |
| exit_restores_windows_on_many_monitors | PASS |
| kill_then_restart_recovers | PASS |
| kill_twice | PASS |
| maximized_before_start | PASS |
| minimized_window_stop | PASS |
| panic_hotkey | PASS |
| stale_journal_entry | PASS |
| stop_during_animation | PASS |
| stop_with_many_windows | PASS |
| stop_without_daemon_rescues_windows | PASS |

Totals: 12 passed, 0 failed. Finished in 41.44 s. Exit code 0. No NOTE or SKIPPED lines.

## File 7: `scn_display`

| test | result |
|---|---|
| offline_15s_round_trip | FAIL |
| offline_then_kill_recover | PASS |
| offline_then_stop | PASS |
| short_blip_3s | FAIL |
| topology_events_while_dragging | PASS |

Totals: 3 passed, 2 failed. Finished in 98.44 s.

**FAIL `offline_15s_round_trip`** (panic at tests\scn_display.rs:143:9)

Diagnostic lines printed before the failure:
```
settled rects: [[3576, 0, 4549, 1148], [-7, -1440, 2567, -41], [-1447, -1784, 7, 735], [-8, 0, 965, 1148], [955, 0, 1928, 1148]]
home monitors: [None, Some(1), Some(2), Some(0), Some(0)]
settled reachable: [false, true, true, true, true]
original monitors: [0, 1, 2, 0, 0]
```
Failure message:
```
thread 'offline_15s_round_trip' (8348) panicked at tests\scn_display.rs:143:9:
window 0: home monitor 0 remains attached during the 15s outage; expected revealable (not minimized, reachable within 2 s of Alt tap + focus); observed iconic=false reachable=false rect=[2936, 0, 3909, 1148]
```

**FAIL `short_blip_3s`** (panic at tests\scn_display.rs:143:9)

Diagnostic lines printed before the failure: identical to `offline_15s_round_trip` above (the same four `settled rects` / `home monitors` / `settled reachable` / `original monitors` lines).
Failure message:
```
thread 'short_blip_3s' (21484) panicked at tests\scn_display.rs:143:9:
window 0: home monitor 0 remains attached during the 3s outage; expected revealable (not minimized, reachable within 2 s of Alt tap + focus); observed iconic=false reachable=false rect=[2936, 0, 3909, 1148]
```

Diagnostic lines and NOTE lines from the passing tests in this file (copied verbatim):
```
display layout before: [[0, 0, 1920, 1200], [0, -1440, 2560, 0], [-1440, -1784, 0, 776]]
display layout after restore: [[0, 0, 1920, 1200], [0, -1440, 2560, 0], [-1440, -1784, 0, 776]]
NOTE exact rect differs for window 1: expected [100, -1340, 460, -1100] observed [1, -1439, 2559, -49]
NOTE exact rect differs for window 2: expected [-1340, -1684, -980, -1444] observed [-1439, -1783, -1, 727]
NOTE exact rect differs for window 3: expected [600, 100, 960, 340] observed [-8, 0, 965, 1148]
NOTE exact rect differs for window 4: expected [1000, 100, 1360, 340] observed [955, 0, 1928, 1148]
```
(The four NOTE lines are from `offline_then_stop`, which passed.)

## Post-run monitor restore

`DisplaySwitch.exe /extend` executed; after 5 s, `[System.Windows.Forms.Screen]::AllScreens.Count` = **3** (expected 3).

---
## Triage of the final run (maintainer notes, not runner output)

Product bugs found and FIXED (verified by tests): foreground change to an off-view window did not scroll it into view
(`system_focus_reveals_every_window` now passes); console close/logoff killed the process before restoring windows
(`ctrl_break_restores_before_exit`, `stop_without_daemon_rescues_windows` pass); Escape during a drag reordered windows
(`drag_cancel_escape` passes); clicking an already-visible tile must not scroll (`shift_freezes_reflow` passes).

Remaining failures and their classification:
| Test | Classification |
|---|---|
| scn_focus::pin_toggle (seen set misses window 0) | TEST FLAW: the starting column is never recorded in the 'seen' set |
| scn_scroll::resize_then_scroll | TEST FLAW: resizes with SetWindowPos; the daemon only honours user (edge-drag) resizes; covered by scn_drag::resize_by_edge_drag |
| scn_scroll::settled_layout_geometry | TEST FLAW: with focus on the middle window only one 957px tile fits, so >=2 reachable cannot be required |
| scn_scroll::scroll_monotonic_and_gapped | TEST TIMING: DXGI duplication delivers frames only on change; the 4 captured frames missed the 54px scroll |
| scn_scroll::peek_visible | OPEN PRODUCT ISSUE: thumbnail geometry verified correct (dest 48px, source = right-most 48px) but the left neighbour's content is WHITE. Windows resized while parked fully off-screen never repaint. An async RedrawWindow did not fix it (reverted). |
| scn_display::offline_15s_round_trip / short_blip_3s | OPEN: after an outage, SetForegroundWindow on a parked primary-monitor window does not reveal it. Not yet reproduced in isolation. |

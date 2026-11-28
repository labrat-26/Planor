# Milestone 4.8 verification matrix (#117)

Status key: **sim** = deterministic simulation or core test (`src/sim_tests.rs`, `tests/core.rs`); **live** = locked
live-desktop test (`tests/scn_drag.rs`); **GAP** = no test; **n/a** = removed or retired in 4.8 on the owner's request.
No row is covered by real trackpad hardware: every four-finger test replays simulated contacts.

Last full run: `scripts\verify.ps1 -Tier all -Perf -NoDisplaySwitch`, 68 pass / 0 flaky / 0 fail, budgets all pass
(`build/verify/20261128-012606`, run on 7cc627f plus the three new tests in this commit). The independent final-HEAD gate (Opus 5.5 review plus a run by someone other than the
coder) has NOT been done; coder runs do not substitute for it.

## Inputs
| Row | Coverage |
|---|---|
| mouse vs four-finger | sim: `mouse_*`, `pad_*`, `axis_lock_*` tests |
| landscape vs portrait | sim: `pad_perpendicular_swipe_groups_on_a_portrait_monitor`, `mouse_edge_scroll_on_a_portrait_monitor_follows_the_vertical_axis`, `stack_exit_needs_a_quarter_pull_both_orientations`, `dwell_in_the_middle_range_previews_then_modifier_groups_both_orientations`; live: `drag_to_portrait_monitor`, `drag_out_of_vertical_group_by_pulling_down` |
| single vs adjacent mixed monitors | sim: `three_monitor_*` rig (landscape, landscape, portrait; mixed sizes); live: `drag_to_other_landscape_monitor`. Mixed DPI: **GAP** (render park tests only) |
| ordinary movement vs fling | sim: `non_fling_four_finger_drag_stays_on_its_monitor_even_pushed_across_the_boundary`, `fling_release_onto_another_monitor_commits_immediately_even_before_async_move_lands`, `fling_does_not_hijack_a_committed_group_entry`; core: `fling_target_covers_every_direction_with_an_offset_layout`, `short_quick_flick_reads_its_true_speed` |

## Group entry
| Row | Coverage |
|---|---|
| before 1/4, exact threshold edges | core: `neighbour_bands_are_measured_from_the_entry_edge`, `entry_edge_prefers_previous_pointer_then_column_order_then_half`; sim `release_before_a_quarter_returns_home_and_mouse_still_crosses_monitors`; live `before_quarter_keeps_neighbours_fixed` |
| 250 ms dwell and configurable | sim: `dwell_restarts_on_movement_and_reversal_out_of_the_neighbour_cancels`, `zero_dwell_previews_on_the_first_middle_range_sample`, `new_drag_settings_are_validated_with_documented_bounds` |
| middle-range preview | sim: `dwell_in_the_middle_range_previews_then_modifier_groups_both_orientations` |
| midpoint-selected tile direction | sim: `n012_mouse_drop_tiles_beside_past_three_quarters_and_stacks_only_with_the_modifier`; live `reorder_before_after` |
| reversal / cancellation | sim: `pad_drag_cancel_during_preview_clears_session_cue_and_layout`, `closing_the_dragged_window_mid_preview_clears_the_gesture`; live `drag_cancel_escape` |
| crossing 3/4 | sim: `n012_mouse_drop_tiles_beside_past_three_quarters_and_stacks_only_with_the_modifier` |
| trackpad perpendicular swipe | sim: `pad_perpendicular_swipe_commits_group_mode_and_reverse_reverts`, `..._groups_on_a_portrait_monitor`, `..._with_along_axis_drift_still_groups` |
| mouse modifier not pressed / pressed / released | sim: `configured_group_modifier_ctrl_and_alt_activate_group_mode_and_shift_does_not`, `modifier_held_before_the_dwell_still_previews_first`, `mouse_group_across_monitors_transfers_and_stacks_only_with_the_modifier`; live `drop_on_center_stacks` |
| modifier setting changes | sim: `configured_group_modifier_ctrl_and_alt_...` |
| ordinary drag without modifier | sim: `drag_hover_never_draws_the_stack_border_before_group_mode` |

## Visuals / timing
| Row | Coverage |
|---|---|
| SVG cue per input/orientation | **n/a**: cue card retired at the owner's request (`src/cue.rs` is dead code) |
| modifier key after dwell | sim: see modifier rows |
| preview-to-group transition | sim: `dwell_in_the_middle_range_previews_then_modifier_groups_both_orientations` |
| immediate stack animation on trigger | sim: `stack_trigger_requests_the_drag_reflow_animation_at_once` (a `drag_reflow` scene with a non-zero duration is sent when group mode commits) |
| two-second vacancy timer bypass / stale timer | **n/a**: timer removed; `vacated_slot_stays_reserved_for_the_whole_drag` replaces it |
| theme / DPI, PiP occlusion | **GAP** |
| no extra scene/placement on lift | sim: `lifting_a_pad_drag_sends_one_final_scene` (at most two scenes, last one non-live) |

## Group exit
| Row | Coverage |
|---|---|
| under / at / over 1/4, both directions | core: `stack_exit_needs_a_quarter_of_the_span_along_the_axis`; sim: `stack_exit_needs_a_quarter_pull_both_orientations`, `stack_exit_toward_the_leading_side_places_the_member_before_the_stack`, `mouse_pull_along_the_stack_axis_exits_a_vertical_group`; live `drag_out_of_stack`, `drag_out_of_vertical_group_by_pulling_down` |
| return animation, focus, cancellation | sim: `sim_drag_grouped_window_beyond_group_border_leaves_group`; return-animation timing: **GAP** |

## Edge scroll
| Row | Coverage |
|---|---|
| 10 % default and configurable | core: `edge_scroll_zone_scales_with_percent_and_never_truncates_to_zero`; sim: `mouse_edge_scroll_uses_the_cursor_monitor_percent_and_zero_disables`, `edge_scroll_percent_changes_apply_to_the_running_drag` |
| both directions | live: `drag_to_edge_scrolls_landscape`, `edge_scroll_steps_are_even`; sim portrait test above |
| entry / exit / reversal | sim: `edge_scroll_stops_when_the_pointer_reverses_inside_the_zone`, `edge_scroll_speed_eases_in_with_hold_time_and_depth` |
| corners | sim: `edge_scroll_in_a_corner_follows_only_the_strip_axis` |
| mouse transfer among monitors | sim: `mouse_group_across_monitors_transfers_and_stacks_only_with_the_modifier`, `release_before_a_quarter_returns_home_and_mouse_still_crosses_monitors` |
| pad confinement except fling | sim: `non_fling_four_finger_drag_stays_on_its_monitor_even_pushed_across_the_boundary`; core `connected_adjacent_rejects_gaps_and_corner_touches` |
| no movement across disconnected gaps | core: `connected_adjacent_rejects_gaps_and_corner_touches`. Note: since the fling rework, `fling_target` accepts monitors across a small gap by design (owner request), so the "no gap" rule now applies only to `connected_adjacent` |
| topology changes | sim: `topology_change_during_a_pad_drag_clears_session_and_pad`, `scn_display_topology_while_dragging_cancels_cleanly` |

## Perf
Existing budgets (`startup <600 ms`, `render.scene <16 ms`, `gesture_to_place <16 ms`, `hook <5 ms`) pass in the last full run.
Chrome/video/fullscreen/PiP and group/ungroup scenario budgets: **GAP** (no dedicated scenario).

## Hardware
Not run. Simulated gesture input only; owner hardware trackpad check outstanding (#60, #63, #66, #68).

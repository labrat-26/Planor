# Open Win+W notes

Generated from [notes.json](notes.json) via `scripts\notes.ps1 -Generate`. Raw text lives in `%LOCALAPPDATA%\planor\trace.log` (timestamps UTC).

| ID | Time (UTC) | Note | State |
|---|---|---|---|
| N-001 | 06:20:20, 06:50:42, 07:11:52, 07:13:25 | freezes, "animation super broken", "super jumpy, can't pair because of the jumpiness" | open: needs a recording (`tools\planor-rec-bin\planor-rec.exe record` or `ring`); slow steps emit `SLOW`, stalls >= 300 ms emit `STALL` + stall bundle |
| N-003 | 06:46:05, 06:49:16, 07:11:36 | cannot group with the left or bottom window; "cannot pair correctly" while scrolling sideways | open: drop zones redesigned with N-012 (small filled centre box on every window of the monitor under the cursor, no dwell, no dodge); needs the owner to retry grouping with the left/bottom window |
| N-005 | 06:49:52, 06:52:26 | borders are on top | open: unclear what they sit on top of (Task Manager is handled; the note box and Start menu are not) |
| N-007 | 07:11:01 | pad movement laggy, not one to one | open: changed (live scrolling no longer eased), not confirmed on hardware touchpad (`tests/core.rs::replay_three_finger_scroll_pad_trace_matches_within_tolerance`) |
| N-013 | 14:35:37.902 (local) | scroll is really not smooth | open: changed, not verified live: momentum now stepped every 8 ms and placed directly (was a 70 ms ease restarted every 32 ms); stray scroll at drag lift removed. Needs a recording during a swipe (planor perf now shows DWM compose fps vs refresh Hz). |
| N-019 | 14:40:48.368 (local) | not sure how but something needs to be done with these animations | open: partly addressed, not verified live: border follows the stand-in, pairs glide instead of wrapping, rolling member no longer pops at half way, smoother momentum. Needs the owner to say which animation still looks wrong. |
| N-021 | 14:42:08.571 (local) | a green boarder appears ?? | open: explained: the green is drop_target_color (#10B981), which used to appear only after a 350 ms hover in the centre of a target. Now used during every drag only for the filled merge zones; the hovered swap target no longer gets a green border (#105, test src/sim_tests.rs::drag_hover_suppresses_green_border). Confirm with the owner. |
| N-022 | 14:42:29.297 (local) | boarder grows faster then the animation | open: changed in src/render.rs, not verified live: during motion the border is drawn around the gliding stand-in (rs), not the in-between rectangle that grew ahead of it. Render-only, no automated test. |
| N-024 | 14:43:50.043 (local) | the title bar of the other grouped apps got distored and elongated | open: changed in src/render.rs, not verified live: peek thumbnails map 1 source px to 1 screen px (was scaled by source size / tile size, which stretched title bars when the parked member was not exactly tile-sized). |
| N-025 | 16:39:35.892 (local) | boarder is off | open: inspected 163935891-boarder-is-off bundle: blue focus border (#3B82F6, 3 px at 150% DPI) aligned cleanly around T3 Code at 41..998 with 6 px gap to VLC during seq=129 180 ms transition (captured mid-flight while BitBlt stalled DwmFlush 77.58 ms; earlier 572 ms win32.place and 284 ms render.scene stalls during 4-finger drag desynced the overlay). Needs owner confirmation after feel smoothness fixes. |

## Fixed (guarded by regression tests)

| ID | Time (UTC) | Note | Test / Commit |
|---|---|---|---|
| N-002 | 06:22:09, 06:52:26, 06:52:51 | clicking the right window does not focus it; cannot keep focus on the rightmost window | `src/sim_tests.rs::sim_clicking_rightmost_window_during_scroll_settle_keeps_focus_n002` |
| N-004 | 06:48:09 | four-finger move should move the window within its stack | `src/sim_tests.rs::n018_dropping_a_group_member_on_its_sibling_reorders_the_group_on_both_orientations` |
| N-006 | 07:13:52 | no empty space on the right | `tests/core.rs::unmaximizing_or_shrinking_clamps_offset_no_empty_space_on_right` (6db2d71) |
| N-008 | 07:1x | group members should be equal-size windows that slide continuously without squashing | `tests/core.rs::group_is_a_deck_of_equal_windows_with_peeking_neighbours_in_landscape` (v0.1.0) |
| N-009 | 07:1x | live touchpad scrolling should have no easing | `tests/core.rs::touchpad_scroll_can_reach_the_first_and_last_window` (v0.1.0) |
| N-010 | 14:32:05.542 (local) | if there is no winow on the top . do not loop around. | `tests/core.rs::n010_a_pair_does_not_loop_around` |
| N-011 | 14:32:39.806 (local) | moving from right to left does not allow it to go back to right | `src/sim_tests.rs::n011_cross_axis_swipe_hops_monitors_both_ways_once_per_gesture` |
| N-012 | 14:35:06.286 (local) | if I drop the window not in a group and on top of the window , switch them aroud. make the drop zone way smaller and keep it permanently highlited  filled iin while moving window on the screen. if the app crosses the screen , remove the highlights from the fist and apply it to the other screen. | `src/sim_tests.rs::n012_mouse_drop_tiles_beside_past_three_quarters_and_stacks_only_with_the_modifier` |
| N-014 | 14:36:00.071 (local) | should scroll the same as landscape monitors but vertical | `src/sim_tests.rs::n014_portrait_scrolls_with_vertical_fingers_like_landscape_with_sideways` |
| N-015 | 14:36:53.869 (local) | there is a bug with the windows trying to doge | `src/sim_tests.rs::n012_mouse_drop_tiles_beside_past_three_quarters_and_stacks_only_with_the_modifier` |
| N-016 | 14:38:15.820 (local) | windows are sometime a bit to eager to get out of the way or nearly non at all | `src/sim_tests.rs::n012_mouse_drop_tiles_beside_past_three_quarters_and_stacks_only_with_the_modifier` |
| N-017 | 14:39:34.197 (local) | in grouped , if scropped above 50 % , scroll the next window in view and focus | `tests/core.rs::n017_past_half_way_the_next_member_takes_over` |
| N-018 | 14:40:17.180 (local) | cannot 4 finger move within the group | `src/sim_tests.rs::n018_dropping_a_group_member_on_its_sibling_reorders_the_group_on_both_orientations` |
| N-020 | 14:41:31.843 (local) | new window seems to be struggleing to keep the position initially | `src/sim_tests.rs::n020_a_new_window_that_moves_itself_is_put_back` |
| N-023 | 14:43:16.969 (local) | windows + scroll do not seem to work. check verical monitor too | `src/sim_tests.rs::n023_win_wheel_scrolls_the_strip_under_the_cursor_on_both_orientations` |

# Open Win+W notes

Generated from [notes.json](notes.json) via `scripts\notes.ps1 -Generate`. Raw text lives in `%LOCALAPPDATA%\planor\trace.log` (timestamps UTC).

| ID | Time (UTC) | Note | State |
|---|---|---|---|
| N-001 | 06:20:20, 06:50:42, 07:11:52, 07:13:25 | freezes, "animation super broken", "super jumpy, can't pair because of the jumpiness" | open: needs a recording (`tools\planor-rec-bin\planor-rec.exe record` or `ring`); slow steps emit `SLOW`, stalls >= 300 ms emit `STALL` + stall bundle |
| N-002 | 06:22:09, 06:52:26, 06:52:51 | clicking the right window does not focus it; cannot keep focus on the rightmost window | open: not reproduced |
| N-003 | 06:46:05, 06:49:16, 07:11:36 | cannot group with the left or bottom window; "cannot pair correctly" while scrolling sideways | open: drop zone is +-20 px around window centre; see MOCKUP_DISAGREEMENT in tests/core.rs::mockup_drop_zone_outer_20_middle_60_join |
| N-004 | 06:48:09 | four-finger move should move the window within its stack | open: not done |
| N-005 | 06:49:52, 06:52:26 | borders are on top | open: unclear what they sit on top of (Task Manager is handled; the note box and Start menu are not) |
| N-007 | 07:11:01 | pad movement laggy, not one to one | open: changed (live scrolling no longer eased), not confirmed on hardware touchpad (`tests/core.rs::replay_three_finger_scroll_pad_trace_matches_within_tolerance`) |

## Fixed (guarded by regression tests)

| ID | Time (UTC) | Note | Test / Commit |
|---|---|---|---|
| N-006 | 07:13:52 | no empty space on the right | `tests/core.rs::unmaximizing_or_shrinking_clamps_offset_no_empty_space_on_right` (6db2d71) |
| N-008 | 07:1x | group members should be equal-size windows that slide continuously without squashing | `tests/core.rs::group_is_a_deck_of_equal_windows_with_peeking_neighbours_in_landscape` (v0.1.0) |
| N-009 | 07:1x | live touchpad scrolling should have no easing | `tests/core.rs::touchpad_scroll_can_reach_the_first_and_last_window` (v0.1.0) |

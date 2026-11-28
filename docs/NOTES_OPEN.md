# Open Win+W notes

Generated from [notes.json](notes.json) via `scripts\notes.ps1 -Generate`. Raw text lives in `%LOCALAPPDATA%\planor\trace.log` (timestamps UTC).

| ID | Time (UTC) | Note | State |
|---|---|---|---|
| N-019 | 14:40:48.368 (local) | not sure how but something needs to be done with these animations | open: partly addressed, not verified live: border follows the stand-in, pairs glide instead of wrapping, rolling member no longer pops at half way, smoother momentum. Needs the owner to say which animation still looks wrong. |
| N-021 | 14:42:08.571 (local) | a green boarder appears ?? | open: fixed: the green centre (drop_target_color) and the cue card are removed in 4.8; the target now gets the mauve stacked border only while a drop would create a stack (test src/sim_tests.rs::dwell_in_the_middle_range_previews_then_modifier_groups_both_orientations). Confirm with the owner. |
| N-022 | 14:42:29.297 (local) | boarder grows faster then the animation | open: changed in src/render.rs, not verified live: during motion the border is drawn around the gliding stand-in (rs), not the in-between rectangle that grew ahead of it. Render-only, no automated test. |
| N-024 | 14:43:50.043 (local) | the title bar of the other grouped apps got distored and elongated | open: changed in src/render.rs, not verified live: peek thumbnails map 1 source px to 1 screen px (was scaled by source size / tile size, which stretched title bars when the parked member was not exactly tile-sized). |
| N-025 | 16:39:35.892 (local) | boarder is off | open: inspected 163935891-boarder-is-off bundle: blue focus border (#3B82F6, 3 px at 150% DPI) aligned cleanly around T3 Code at 41..998 with 6 px gap to VLC during seq=129 180 ms transition (captured mid-flight while BitBlt stalled DwmFlush 77.58 ms; earlier 572 ms win32.place and 284 ms render.scene stalls during 4-finger drag desynced the overlay). Needs owner confirmation after feel smoothness fixes. |

## Fixed (guarded by regression tests)

| ID | Time (UTC) | Note | Test / Commit |
|---|---|---|---|
| N-020 | 14:41:31.843 (local) | new window seems to be struggleing to keep the position initially | `src/sim_tests.rs::n020_a_new_window_that_moves_itself_is_put_back` |
| N-023 | 14:43:16.969 (local) | windows + scroll do not seem to work. check verical monitor too | `src/sim_tests.rs::n023_win_wheel_scrolls_the_strip_under_the_cursor_on_both_orientations` |

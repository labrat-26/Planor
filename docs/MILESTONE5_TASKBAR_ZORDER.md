# Milestone 5: taskbar, z-order, window placement (issues #36, #53, #56, #67)

Second pass (m5-opus) established the #36 root cause, added #56 diagnostics and fixed all three #67 items.
Commits: 4bfb5b8 (#67.1), 302c393 (#36), 4793533 (#56), f88da02 (#67.2), 28f802a (#67.3).

## #53 "unsure why the app resized": not a bug (documented)
Bundle trace (02:49:40): a second window (1969132) opened, so the tile was set to 957 wide. The app has a minimum
width of 1127 and answered `PLACE ... (2944,0,1127,1140)`; the layout then re-EXPECTed it at 1127 and gave the
neighbour 787. The second window then closed (`SCENE tiles=1`), the tile was expanded back to 1920 and the app
reported 1936x1148 mid-animation before settling at 1920x1140. So the resize came from a new window opening and
closing, plus the app's own minimum size (the verify pass reports real minimums). Nothing to fix.

## #36 "in groups, the taskbar should remain in the foreground": root cause found and fixed (302c393)
Bundle `000644700-in-groups-the-taskbar-should-remai` (2560x1440 monitor, work area h=1392, bottom taskbar 48 px).
`state.json` shows a 2-window group: the parked peek 918912 has `expected_rect (0,1364,1277,1364)` and
`os_rect (3584,1364)`. Pixel-sampling the screenshot confirms 76 rows of the terminal (y 1364..1440, x 0..1277)
painted over the taskbar strip — the first pass only looked at whether the taskbar buttons were visible and
concluded "not reproducible".

The build in that bundle is v0.6.0 (`os_rect.y == tile.y` is v0.6.0's park rule, and `parked_target_rect_for_monitor`
did not exist yet). In v0.6.0 `build_all_hosts` pushed a proxy's WGC picture at its whole stand-in rect with no
`bound` clip, so a group peek taller than the work area painted straight over the taskbar. 27335a7 already fixed
that by routing pictures and thumbnails through `proxy_blit_rects` (clip ∩ bound). The one drawn element still
unclipped today is the border stroke, which follows the whole stand-in rect and can reach the taskbar during a
group roll or glide, so borders now carry their tile `bound` and are drawn under `PushAxisAlignedClip`
(`render::BorderLine` / `border_line`).

Tests: `render::tests::group_peek_never_draws_over_the_taskbar` (bundle geometry, settled and moving: clip, WGC
backing and thumbnail destinations all stay inside the work area), `render::tests::border_lines_carry_the_work_area_clip`.
Also asserted by the live `scn_taskbar::tiles_stay_inside_work_area`.

## #56 "clicking on the bar made everything disappear": not established; diagnostics added (4793533)
Bundle `230436300` was taken at 23:04:36 but its 400-line trace tail starts at 23:04:07.9 with `SCENE tiles=0` —
everything was already gone ~4 s before the note, and the rest of the tail is PAD scroll lines. The trace of the
moment itself does not exist, so no cause can be established from it. (The `EVENT_OBJECT_HIDE` guard
`is_parked_by_planor` is not it: a parked window is invisible to `is_visible`, so the guard only *keeps* tiles.)

Next bundle will explain it:
- `FG <id> ... class=<cls>` for unmanaged foreground (`Shell_TrayWnd` = a taskbar click, `Progman`/`WorkerW` = show desktop)
- `REMOVE <id> reason=destroy|minimize|hide focused= fg= fg_class= columns_left=` for every tile that leaves the layout
- `HIDE <id> kept reason=visible|iconic|parked` for a managed HIDE that is ignored
- `CLEAR reason=topology|suspend|desktop` and `TOPOLOGY rescan` for the paths that wipe every visual
- `trace.tail.log` keeps 3000 lines instead of 400

## #67 edge cases: all three done
1. **Release-to-first-tick (4bfb5b8).** The low-level mouse hook records every `WM_LBUTTONUP`/`WM_RBUTTONUP` point
   with a growing sequence (`win32::last_release`, `try_lock` so the hook never blocks); `DragSession` snapshots the
   sequence at `MOVESIZESTART` and, once the button is up, drops at the hook point instead of the tick's cursor.
   No hook sample (test mode, missed `try_lock`) falls back to `GetCursorPos` exactly as before, so the two existing
   release tests still mean what they meant. Tests: `release_point_from_hook_wins_over_cursor_moved_before_tick`
   (fails without the fix; cursor drifts back onto the previous target between release and tick) and
   `release_point_from_before_drag_is_ignored` (a stale pre-drag release must not win).
2. **Holes latency (f88da02).** `EVENT_OBJECT_LOCATIONCHANGE` of an unmanaged overlay occluder sets `holes_stale`,
   so the next tick (<=32 ms) re-reads holes instead of waiting for the 200 ms poll. Managed windows do not trigger
   it, and the flag coalesces a move burst into one `holes()` per tick; the live-scroll hole cache is untouched.
   Test: `unmanaged_window_move_refreshes_holes_on_next_tick` (fails without the fix). The 2-tick NULL-foreground
   debounce stays: it is deliberate anti-blink, not latency.
3. **Post-GPU-reset pictures (28f802a).** `Host::invalidate_gpu` drops every capture source, and recovery redrew the
   last static frame with `pictures: &[]`, so settled peeks stayed blank until the daemon happened to publish a new
   scene — an idle desktop may never send one (`update_idle_sources` only refreshes pictures that still exist).
   Recovery now re-arms `last_settled_scene` as a static idle frame (`idle_scene_from_settled`, shared with the
   `OverlayUpdate` path) so `build_all_hosts` re-requests the sources, gated by `needs_scene_rearm`.
   Tests: `recovery_rearms_the_settled_scene_as_a_static_idle_frame`, `scene_rearm_after_recovery_is_gated`.
   Not verified: an actual TDR/device-loss round trip needs a real GPU reset, which no test here performs.

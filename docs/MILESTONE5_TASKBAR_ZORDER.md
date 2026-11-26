# Milestone 5: taskbar, z-order, window placement (issues #36, #53, #56, #67)

Readings and findings from the Win+W bundles. No daemon code changed in this milestone.

## #53 "unsure why the app resized": not a bug (documented)
Bundle trace (02:49:40): a second window (1969132) opened, so the tile was set to 957 wide. The app has a minimum
width of 1127 and answered `PLACE ... (2944,0,1127,1140)`; the layout then re-EXPECTed it at 1127 and gave the
neighbour 787. The second window then closed (`SCENE tiles=1`), the tile was expanded back to 1920 and the app
reported 1936x1148 mid-animation before settling at 1920x1140. So the resize came from a new window opening and
closing, plus the app's own minimum size (the verify pass reports real minimums). Nothing to fix.

## #36 / #56 taskbar and z-order: not reproducible from the bundles
Both screenshots show the taskbar fully visible and no tile or overlay over it. The overlay covers the whole monitor
rectangle but is WS_EX_TRANSPARENT and draws only inside each tile's `bound` (the strip's work area), so it cannot paint
over the taskbar. The only drawn element outside the work area is the drop zones during a drag, and those lie inside the
strip. The #56 "everything disappeared" matches the guarded EVENT_OBJECT_HIDE path (`is_parked_by_planor`, bf83a54), which
stays as is. Without a trace of the moment itself, no cause can be established; readings: #36 = the taskbar must not be
covered by Planor visuals (holds today, asserted by `scn_taskbar::taskbar_hit_test_passes_through` and
`tiles_stay_inside_work_area`).

## #67 edge cases: skipped, with reasons
1. Cursor movement between button release and first tick (<=32 ms) counts as the drop point: the sim tests
   `release_tick_retargets_from_previous_window_and_stacks_on_final_target` and
   `release_before_first_motion_tick_transfers_to_sampled_monitor` define "the first release tick samples the final
   drop position" as intended. A real fix needs the release position from the low-level mouse hook (WM_LBUTTONUP point)
   and a cross-thread event. Not a safe small change; left open.
2. NULL foreground / hole refresh trailing a fast unmanaged window: the 2-tick debounce is deliberate (avoids focus border
   blinking during activation) and the hole refresh cadence is a perf trade-off. Left as is.
3. WGC pictures only return on the next scene after a GPU reset: the surface does recover and the DWM thumbnail path
   covers the gap; a forced scene on recovery would add a redraw to every device loss. Left as is.

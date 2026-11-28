# Open Win+W notes

Generated from [notes.json](notes.json) via `scripts\notes.ps1 -Generate`. Raw text lives in `%LOCALAPPDATA%\planor\trace.log` (timestamps UTC).

| ID | Time (UTC) | Note | State |
|---|---|---|---|
| N-007 | 22:04:05.235 (local) | cannot fling up | open: open: by design, axis lock ignores a four-finger swipe that STARTS across the strip axis (test axis_lock_ignores_cross_axis_start_and_keeps_carry_on_axis), so a flick straight up from a landscape monitor never picks the window up. Needs an owner decision. |
| N-008 | 22:04:34.586 (local) | cannot change focus with the pad | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |
| N-009 | 22:05:03.682 (local) | oprning a minimisez window causes the laptop screen to flicker temp | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |

## Fixed (guarded by regression tests)

| ID | Time (UTC) | Note | Test / Commit |
|---|---|---|---|

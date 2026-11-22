# Open Win+W notes

Typed with Win+W while diagnostics are on; the raw text is in `%LOCALAPPDATA%\planor\trace.log` (timestamps UTC).
Fixed notes are removed from this list. Last cleaned 2026-11-22 after the 07:13 notes.

| Time (UTC) | Note | State |
|---|---|---|
| 06:20:20, 06:50:42, 07:11:52, 07:13:25 | freezes, "animation super broken", "super jumpy, can't pair because of the jumpiness" | open: needs a recording (`tools\planor-rec-bin\planor-rec.exe record`); slow steps are now written to the trace as `SLOW ...` lines with timestamps |
| 06:22:09, 06:52:26, 06:52:51 | clicking the right window does not focus it; cannot keep focus on the rightmost window | open, not reproduced |
| 06:46:05, 06:49:16, 07:11:36 | cannot group with the left or bottom window; "cannot pair correctly" while scrolling sideways | open: the drop zone is only +-20 px around the window centre (`stack_center_px`) |
| 06:48:09 | four-finger move should move the window within its stack | open, not done |
| 06:49:52, 06:52:26 | borders are on top | open: unclear what they sit on top of (Task Manager is handled; the note box and the Start menu are not) |
| 07:13:52 | no empty space on the right | open: strips shorter than the monitor are filled, so this must be another case |
| 07:11:01 | pad movement laggy, not one to one | changed (live scrolling no longer eased), not confirmed on the touchpad |

Fixed in the 07:1x round: group members are now equal-size windows that slide (the neighbours peek in and out clipped,
nothing is squashed or resized, so rolling a group is a pure translation); live scrolling has no easing.

# Open Win+W notes

Generated from [notes.json](notes.json) via `scripts\notes.ps1 -Generate`. Raw text lives in `%LOCALAPPDATA%\planor\trace.log` (timestamps UTC).

| ID | Time (UTC) | Note | State |
|---|---|---|---|
| N-001 | 21:56:02.769 (local) | broke the top window | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |
| N-002 | 22:01:35.842 (local) | no scroll vertical on the top bottom edge | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |
| N-003 | 22:02:25.039 (local) | windows still slide behind other windows | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |
| N-004 | 22:02:56.952 (local) | window do not want to pair virtically | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |
| N-005 | 22:03:21.079 (local) | I thought i asked for an option to not snap to the focused window | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |
| N-006 | 22:03:53.079 (local) | cannot fling reliably to the right | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |
| N-007 | 22:04:05.235 (local) | cannot fling up | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |
| N-008 | 22:04:34.586 (local) | cannot change focus with the pad | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |
| N-009 | 22:05:03.682 (local) | oprning a minimisez window causes the laptop screen to flicker temp | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |
| N-010 | 22:01:59.852 (local) | broke the top window temporaryl | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |

## Fixed (guarded by regression tests)

| ID | Time (UTC) | Note | Test / Commit |
|---|---|---|---|

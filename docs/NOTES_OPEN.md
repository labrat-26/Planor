# Open Win+W notes

Generated from [notes.json](notes.json) via `scripts\notes.ps1 -Generate`. Raw text lives in `%LOCALAPPDATA%\planor\trace.log` (timestamps UTC).

| ID | Time (UTC) | Note | State |
|---|---|---|---|
| N-009 | 22:05:03.682 (local) | oprning a minimisez window causes the laptop screen to flicker temp | open: open: trace shows the parked-window re-place loop (y=-37187 beyond the 16-bit range), fixed by commit Park y never beyond the 16-bit window range; not verified live, owner to retry |

## Fixed (guarded by regression tests)

| ID | Time (UTC) | Note | Test / Commit |
|---|---|---|---|

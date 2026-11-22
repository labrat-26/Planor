# Golden Touchpad Gesture Fixtures (3.4 / 9.7)

Versioned (`REPLAY_SCHEMA_VERSION = 1`) gesture input streams replayed by `tests/core.rs` via `planor::replay` without touchpad hardware.

- `three_finger_scroll_right.pad.log`: Three-finger horizontal swipe (`PAD contacts` + reference `PAD scroll` lines from a 1920x1152 strip with 5 windows).
- `three_finger_group_roll_portrait.pad.log`: Three-finger vertical swipe rolling a 3-member portrait deck (`1440x2512`).
- `four_finger_drag.events.jsonl`: `events.jsonl`-formatted 4-finger pickup, move, and drop sequence.

To capture a new hardware fixture, enable `[diagnostics] enabled = true` in `%APPDATA%\planor\planor.toml`, perform the gesture, and extract the `PAD ` lines from `%LOCALAPPDATA%\planor\trace.log`.

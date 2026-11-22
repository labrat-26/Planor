# Recorded touchpad gestures

Real gestures captured with `planor gestures record`, replayed by
`src/sim_tests.rs::recorded_gestures_replay_on_landscape_and_portrait` through the real engine on the
landscape monitor (MON0) **and** the portrait one (MON2) of the simulated three-monitor rig.

For every gesture (touch to last lift) the test works out what it means on that monitor and checks the
engine did it:

| Gesture | Expected |
|---|---|
| 3 fingers along the strip (sideways on landscape, up/down on portrait) | the strip scrolls |
| 3 fingers across the strip | a group of three rolls, the strip does not scroll |
| 4 fingers | the window is picked up and dropped, nothing is left scrolling or dragging |
| shorter than twice `axis_lock_deadzone` | only checked to leave no state behind |

After each gesture the layout invariants and `os_rect == expected_rect` are checked.

## Recording

```powershell
target\release\planor.exe gestures record scroll-right-slow --seconds 15
```

Do the gesture(s) on the touchpad during the countdown (lift all fingers between gestures). The file lands
in `%LOCALAPPDATA%\planor\gestures\<stamp>-<name>.jsonl` (or `--out <file>`); copy it here.

Format: first line `{"kind":"meta", ...}` (name, monitor under the cursor, touchpad settings), then one
`{"t":<us>,"kind":"contacts","pts":[[x,y],...]}` per touchpad frame; `"pts":[]` is a lift. Same format as
`planor-rec` `events.jsonl`, read by `planor::replay::parse_events_jsonl`.

`synthetic-sample.jsonl` is hand-made (meta `"synthetic": true`) so the harness runs before real
recordings exist.

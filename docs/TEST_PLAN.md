# Planor live test plan

Goal: catch the failures a user actually hits, which the original suite missed. The original suite used one
or two windows, never pressed a shortcut, never measured window sizes during motion, and never lost a monitor.

## Rules for every scenario file (hard requirements)
- File: `tests/scn_<name>.rs`. Header is exactly:
  ```rust
  #![allow(dead_code, unused_imports)]
  include!("common/helpers.rs");
  ```
  Every test is `#[test] #[ignore]` and is run with
  `cargo test --test scn_<name> -- --ignored --test-threads=1 --nocapture`.
- Use the helpers in `tests/common/helpers.rs` (`Sentinel::new/at`, `Daemon`, `press`, `fg_index`, `mons`,
  `reachable`, `monitor_of`, `DisplayGuard`, `Capture::boxes`, `eventually`, ...). Read that file first. Do NOT edit it
  and do NOT edit anything under `src/`. If a helper is missing, write a small private fn in your own file.
- Call `aware()` at the start of every test (physical pixels).
- Tests assert **correct product behaviour from the spec**. If the product is wrong the test must FAIL.
  Never loosen a tolerance, add a sleep, or skip an assertion to make a test pass. A red test with a precise
  message is the desired output for a real bug.
- Every assertion message must say what was expected, what was observed, and for which window index/step.
- Settle time: after `d.start()` sleep 1500 ms; `press()` already waits 700 ms.
- Sentinels are tiled by the test daemon only (config rules ignore every other window). Windows are created
  on the primary monitor at x = 50 + 60*i unless you use `Sentinel::at(i, 0, x, y)` with *physical* coordinates.
- Always clean up via RAII (`Daemon` and `Sentinel` already have `Drop`). A panic must never leave windows parked.
- Never start the daemon if a real `planor.exe` is running (the harness asserts this). Never touch other apps' windows.
- Report format when you run a test: test name, PASS/FAIL, and for failures the exact assertion message.

## Display facts for this machine (re-read at runtime via `mons()`, do not hard-code)
Primary landscape 1920x1200 at (0,0); landscape 2560x1440 above it; **portrait 1440x2560** up-left. Tests that
need a portrait/second monitor must find them with `mons()` and `is_portrait()`, and print `SKIPPED_<reason>`
(then return) if the topology does not provide them.

## Scenarios

### S1 `scn_focus.rs` : keyboard focus & shortcuts (reported bug: "with >2 windows one cannot be focused")
For N in {3,4,5,6} sentinels on the primary monitor (strip order = creation order):
1. `focus_walk_right_then_left`: start, then press Alt+Right N-1 times. After each press assert
   `fg_index == step+1` AND the focused window is `reachable` (>=90% on a monitor). At the strip end one more
   Alt+Right must keep focus on the last window. Then Alt+Left back to window 0 with the same two assertions.
   Focus must be able to reach EVERY window.
2. `focused_window_never_parked`: after every key in a random-but-seeded sequence of 40 presses from
   {Alt+Left, Alt+Right, Alt+Up, Alt+Down, Alt+S, Alt+Shift+Left, Alt+Shift+Right}, the foreground sentinel must be
   `reachable`. Print the key sequence on failure.
3. `window_sizes_stable_while_scrolling`: record each sentinel's visible width/height after settle; after every
   scroll press, every sentinel that is on screen must have the same width/height (+-2px) as at start.
4. `stack_cycle`: Alt+S groups the focused window with its left neighbour (or right if first). Alt+Up/Down cycles
   members; focus never leaves the stack; exactly one stack member is `reachable` at a time; Alt+S ungroups and
   all members are reachable columns again (scrolling allowed).
5. `move_reorders`: Alt+Shift+Right swaps the focused column with its right neighbour: window x-order changes
   accordingly; focus follows the moved window and stays reachable.
6. `click_focus_offscreen`: call `SetForegroundWindow` on a sentinel that is currently only a peek/off-screen;
   within 1.5 s it must become `reachable` (the strip must scroll to it).
7. `pin_toggle`: Alt+P pops the window out of the strip and it gets `WS_EX_TOPMOST`; Alt+P again re-inserts it
   right after the focused tile and clears TOPMOST. All windows reachable afterwards.
8. `monitor_hotkeys`: with >=2 monitors, Alt+] / Alt+[ move the active monitor and focus lands on a window of
   that monitor (place one sentinel per monitor with `Sentinel::at`).

### S2 `scn_scroll.rs` : size/position fidelity during scrolling (reported bug: "size issues when scrolling")
Use at most 3 sentinels (pixel colours collide at 4). Use `Capture::open(0)` and `Capture::boxes()`.
1. `scroll_boxes_constant_size`: start daemon, settle, take the settled boxes. Press Alt+Right and sample frames
   for 900 ms (record every `boxes()` result with its present time). For every sample and every sentinel whose box
   does not touch the left/right output edge: width and height must equal the settled width/height within 2 px,
   and top/bottom must equal the settled top/bottom within 2 px. Report the first violating sample with its
   index, colour, expected and actual box.
2. `scroll_monotonic_and_gapped`: during the scroll, the left edge of each sentinel moves monotonically toward
   its destination (no jumps backwards > 2px); after settle, the horizontal gap between adjacent sentinel boxes
   equals `gap_px` (6) scaled by DPI within 2px.
3. `settled_layout_geometry`: with 3 windows, after settle the visible frames (Sentinel::visible) are exactly
   gap-separated and each has height == work-area height. Repeat after 3 scroll presses left/right.
4. `resize_then_scroll`: resize the middle sentinel via `SetWindowPos` to 70% of work width, wait, then scroll
   through all windows: the other windows keep their original lengths (push semantics) within 2 px.
5. `peek_visible`: after settle the next off-screen window leaves a `peek_width_px`-wide (48) sliver on the
   screen edge (observed in pixels via `Capture::boxes`, colour present at the edge).

### S3 `scn_exit.rs` : exit & recovery (reported bug: "when exiting, apps remain off-screen")
Use 6 sentinels (plus `Sentinel::at` on other monitors where available). Record every window's
`GetWindowRect` + `GetWindowPlacement` BEFORE starting the daemon. Common assertion `restored(&ws, &before)`:
every window rect == before (+-1px) AND `reachable`. Scenarios:
1. `stop_with_many_windows`: tile, scroll to the middle, `stop` => restored; recovery.json removed; process exits 0.
2. `panic_hotkey`: Ctrl+Alt+Shift+Backspace => daemon exits and all restored.
3. `kill_then_restart_recovers`: tile, scroll so several are parked, kill the daemon, assert at least one window
   is NOT reachable (precondition), start a new daemon with `tile_existing_on_startup = false`, assert all restored
   within 5 s, then stop.
4. `kill_twice`: kill, restart, kill again (before any stop), restart: still all restored.
5. `stop_during_animation`: press Alt+Right and call `stop` 100 ms later: all restored.
6. `close_some_windows_then_stop`: drop 2 of 6 sentinels while tiled; remaining 4 restored; no daemon panic.
7. `minimized_window_stop`: minimize one sentinel while tiled, stop: it is restored to its pre-Planor placement
   (still minimized or normal exactly as the journal says) and is reachable via `ShowWindow(SW_RESTORE)`.
8. `maximized_before_start`: maximize a sentinel before start; after stop it is maximized again.
9. `stale_journal_entry`: hand-write recovery.json containing a dead hwnd plus a live one; daemon start must not
   fail and must restore the live one.

### S4 `scn_drag.rs` : drag & drop incl. portrait monitor (use REAL mouse input)
Drag with `SetCursorPos` + `mouse_event(LEFTDOWN)` on the caption (hwnd rect top + 12px, x centre), move in 20
steps of 15 ms (slow, < 300 px/s over the final 400 ms), `LEFTUP`. Always restore the cursor in a `Drop` guard.
1. `reorder_before_after`: drag window 2 over the outer 25% left edge of window 0 and drop: order becomes 2,0,1.
2. `drop_on_center_stacks`: hover the center 50% of another window >= 450 ms, drop: they are one stack
   (one of them `reachable`, the other a stack peek); border colour stacked.
3. `shift_freezes_reflow`: hold Shift while dragging across tiles: no neighbour moves (>2px) until drop.
4. `drag_out_of_stack`: drag a stack member out: auto-unwrap, both become normal tiles, focus follows dragged window.
5. `drag_to_portrait_monitor`: needs a portrait monitor. Drag a sentinel onto it. Expected: the window ends on the
   portrait monitor, strip scrolls **vertically** (two windows there share the width, differ in y, each width ==
   work width); then drag it back to the primary: layout there is horizontal again; all windows `reachable`.
6. `drag_to_other_landscape_monitor`: same for the second landscape monitor.
7. `drag_cancel_escape`: press Escape mid-drag: window returns to its original column.
8. `resize_by_edge_drag`: drag the right edge of window 1 +200px: later windows shift (push), earlier unchanged;
   dragging the leading (left) edge of window 0 is ignored.

### S5 `scn_display.rs` : monitor offline (reported: "screen offline for like 15 seconds")
Uses `DisplayGuard::internal_only()` (runs `DisplaySwitch.exe /internal`) and its Drop (`/extend`). Needs >=2
monitors else print `SKIPPED_SINGLE_MONITOR`. Record `mons()` before and assert the same rects after restore.
1. `offline_15s_round_trip`: one sentinel per monitor via `Sentinel::at`, plus 2 extra on the primary. Start daemon,
   settle, record rects. Go internal-only; sleep 15 s. Assert: no sentinel is lost: every window is either
   minimized (iconic) or `reachable` on the remaining monitor; windows of the primary stay tiled, focusable with
   Alt+Left/Right and `reachable`; daemon still responds to `reload`. `planor pull-orphans` => every sentinel is
   `reachable` on the primary and not iconic. Restore (`/extend`), sleep 6 s: every window back on its ORIGINAL
   monitor (`monitor_of`) and `reachable`, rects == recorded within 3 px.
2. `short_blip_3s`: same with 3 s (below/around the 1.5 s debounce): same final assertions.
3. `offline_then_stop`: while internal-only, call `stop`: all sentinels restored (rect == original, `reachable`
   after `/extend`).
4. `offline_then_kill_recover`: while internal-only kill the daemon, `/extend`, start daemon again: all restored.
5. `topology_events_while_dragging`: begin a drag (mouse down + move), trigger internal-only: no panic, daemon
   alive, no sentinel lost.

### S6 `scn_taskbar.rs` : taskbar & shell interaction (reported: "the start bar is clickable")
Interpretation to verify: tiles never cover the taskbar, and the overlay never blocks it.
1. `tiles_stay_inside_work_area`: 3 sentinels: every visible frame lies inside its monitor's work area
   (taskbar excluded) after settle and after scrolls.
2. `taskbar_hit_test_passes_through`: for each monitor with a taskbar strip (monitor rect minus work rect),
   `WindowFromPoint` at the centre of that strip must resolve (via `GetAncestor(GA_ROOT)`) to a window whose class
   is `Shell_TrayWnd` or `Shell_SecondaryTrayWnd`, not a Planor overlay. Also check the overlay never has
   `WS_EX_NOACTIVATE` missing / never takes foreground when the Start button strip is hit-tested.
3. `overlay_click_through_everywhere_but_peek`: sample a 9x9 grid of points over the primary monitor (excluding the
   peek rects): `WindowFromPoint` must never return an overlay window; at a point inside a peek sliver it may.
4. `fullscreen_covers_taskbar_and_is_scrollable_out`: make a sentinel fullscreen (borderless, monitor bounds) =>
   it moves to the strip end, covers the taskbar; Alt+Left brings the previous tile back and the taskbar is visible.
5. `start_menu_open_keeps_layout`: send the Win key (open/close Start) and assert no sentinel moves (+-2 px).

## Runner protocol (Haiku runner)
Run scenarios one at a time on the real desktop (never in parallel), in order S6, S1, S2, S4, S3, S5.
For each file: `cargo test --test <file> -- --ignored --test-threads=1 --nocapture`. Write results to
`docs/TEST_RESULTS.md`: per test PASS/FAIL, assertion messages verbatim, plus printed diagnostics. Do not fix
product code; do not edit assertions. If the machine is left with parked windows, run `planor.exe stop` from
`target/release` with the same `PLANOR_INSTANCE` is not needed; just report it.

## Regression tests added from the Win+W notes (2026-11-22)
Unit tests in `tests/core.rs` (run with plain `cargo test`):
- `group_shrinks_the_column_by_one_fixed_peek_in_landscape`, `..._sideways_in_portrait`,
  `only_the_active_and_next_member_are_shown`: a group is the active window minus ONE peek strip, whatever the
  member count; portrait is the inverse; members past the next one are hidden.
- `focusing_a_visible_window_never_slides_the_strip` ("when switching focus, the windows slide laterally").
- `touchpad_scroll_can_reach_the_first_and_last_window` ("cannot three finger focus the first window").
- `resizing_a_pair_keeps_it_filling_the_monitor`, `resizing_a_pair_cannot_squeeze_the_neighbour_below_its_minimum`
  (bad resize direction, state not consistent with the 50/50 rule).
- `a_window_left_alone_fills_the_monitor`.

Live test `tests/scn_overlay.rs` (`cargo test --test scn_overlay -- --ignored`):
- `border_never_paints_over_an_unmanaged_window` ("border was above the task manager"). A non-topmost,
  non-activating, untiled window covers the monitor; the focus border (#3B82F6) must not appear over it and must
  return when it closes. Verified discriminating: with carving disabled (`PLANOR_HOLES=NoSuchClass`) it fails.
  `PLANOR_HOLES=<window class>` makes an isolated test daemon cut out only that class (real daemons cut out every
  unmanaged window).

Not yet covered by an automatic test (needs touchpad input or a screen recording): three-finger scroll feel,
four-finger move consistency and lag, window spawn on the cursor's monitor, edge scrolling while dragging.

## Recording a scenario (screen recorder + pixel analyzer)

The recorded screen is the ground truth for where windows really are; `tools/planor-rec` (see its README)
records every monitor and checks Planor's claims against the pixels. Scenarios can opt in; **it is off unless
`PLANOR_RECORD=1`**, and then only for scenarios that call it, so normal runs are unchanged.

```rust
let rec = Recording::start("my_test_name", "s6-work");   // 2nd arg: the label given to Daemon::new
// ... the scenario ...
rec.mark("after Alt+Right");                            // optional step names on the timeline
assert_recording_clean(&rec);                           // fails on any `fail` finding, never on `unmeasurable`
```

- `Recording::start(test, daemon_label)` (in `tests/common/helpers.rs`) starts `planor-rec record` as a child
  process with its own folder `<temp>\planor-rec-<test>-<pid>` (kept), its own pipe name, no global hotkeys, the
  test name as a mark and `--trace <the test daemon's trace>`. The isolated test daemon writes its trace under
  its own `LOCALAPPDATA` (`<temp>\planor-live-<label>-<pid>\planor\trace.log`), not the real one, so the
  recorder is told where it is; with `PLANOR_RECORD=1` `Daemon::new` also switches Planor's diagnostics on in
  the test config, otherwise there would be no `EXPECT` lines to check.
- Create it before the sentinels and the daemon so the whole scenario is on the recording. On drop (also when
  the test panics) it stops the recorder, runs `planor-rec analyze --trace ...` and prints
  `[recording] <test>: report N checks (F fail, U unmeasurable, P pass); <report.md>` plus one line per failure.
- `assert_recording_clean(&rec)` stops and analyzes at that point and panics listing every `fail` finding
  (position, lag, border, interior, monitor black/frozen, pacing, `capture_lost`). `unmeasurable` findings
  (parked windows, dark windows, frames dropped by the recorder) never fail a test.
- The binary is `PLANOR_REC_BIN`, else `tools/planor-rec-bin/planor-rec.exe`, else `target/release/planor-rec.exe`.
  With `PLANOR_RECORD=1` and no binary the test fails with a message saying so.
- **Backdrop.** With `PLANOR_RECORD=1`, `Recording::start` first puts one plain window per monitor (class
  `ExternalTestBackdrop`, solid RGB(0,0,90) = the recorder's wallpaper colour, COLORREF `0x005A0000`) over the
  monitor's work area, below everything the test creates afterwards, and removes them after the recorder has
  stopped. Without it the analyzer measured the developer's own windows (a full-screen app, a browser) behind
  the tiles as if they were the tiles ("9 px" mismatches, a bogus `border_missing`). The backdrops are
  invisible to Planor's isolated test daemon: no `WS_THICKFRAME` (never eligible for tiling), `WS_EX_NOACTIVATE`
  (never "external", never foreground), not topmost, shown without activation, and `PLANOR_HOLES` is unset
  so nothing is carved out. **Create the guard before the sentinels** (both example scenarios do), so the
  sentinels sit above the backdrops. Further layer: if another program's window still shows through, the
  analyzer reports `environment_not_clean` (`unmeasurable`) and does not fail position or border on that
  monitor.
- **The first scene is the startup scene.** The daemon builds its GPU overlay and window capture sources
  before the first placement, so the initial layout lands about 0.5 to 0.8 s after Planor's first `EXPECT`
  line. That is accepted behaviour but measured: the recording gets a `startup_latency` finding with the
  arrival time in ms, failing only above 1500 ms, and the strict 50 ms lag threshold applies to every later
  scene (Alt+Right, drops, ...). Position, border and interior checks wait until a window has arrived.
- **`daemon-stopped`.** `Daemon::stop()`/`kill()` mark `daemon-stopped` on the active recording; the analyzer
  expects nothing from Planor after it (windows go back where they were while the recording is still
  running). A window being dragged by hand is in no scene and is not expected at its old rectangle.
- Wired into two scenarios as examples: `scn_drag.rs::drag_to_other_landscape_monitor` and
  `scn_taskbar.rs::tiles_stay_inside_work_area`.
- Cost: the recorder uses about 4 CPU cores while it runs and `analyze` takes a minute or two per recorded
  minute. Needs the three-monitor machine and the solid RGB(0,0,90) wallpaper; run with no real `planor.exe`.

```powershell
$env:PLANOR_RECORD = "1"
cargo test --test scn_taskbar tiles_stay_inside_work_area -- --ignored --test-threads=1 --nocapture
```
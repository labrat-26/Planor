# Planor — Morning Report

Generated 2026-11-19 on Windows 11 build 26300 (3 monitors attached).

## Release binary
- Path: `target/release/planor.exe`
- Size: 1,703,936 bytes (≈1.62 MiB), statically linked CRT, Per-Monitor V2 manifest embedded.
- `cargo build --release`: clean, **0 warnings**.

## Test results (all run on this machine)
| Suite | Command | Result |
|---|---|---|
| Phase 1 core | `cargo test --test core` | **10 / 10 passed** |
| Phase 2 (malformed TOML, IPC round-trip, log caps, journal recovery) | `cargo test` (bin unit tests) | **4 / 4 passed** (`tests::malformed`, `tests::round_trip`, `diag::tests::caps`, `daemon::tests::stale_journal`) |
| Phase 5 live Win32 + DXGI | `cargo test --test live -- --ignored --test-threads=1` | **7 / 7 passed**, run twice back-to-back |

Live tests: `stop_restores_exact_rect`, `forced_kill_recovery`, `owned_dialog_excluded`,
`owned_modal_follows_display_transfer`, `bounded_rejection_backoff`, `desktop_round_trip` (A → B → A),
`compute_visual`.

## `build/live/visual_report.json` (latest run)
```json
{ "bleed_events": 0, "border_delta_px": 0, "captured_samples": 21, "edge_tolerance_px": 2,
  "frozen_frames": 0, "input_to_first_pixel_ms": 25.9, "median_dt_ms": 16.84,
  "missing_edge_frames": 0, "non_monotonic_jumps": 0, "p99_dt_ms": 23.93, "primary_frames": 21 }
```
Cross-monitor bleed was actually measured (a secondary monitor was present), not skipped.
Final vision sanity check: `build/live/filmstrip.png` (regenerated with `PLANOR_FILMSTRIP=1`) shows
magenta → cyan → yellow sentinels sliding with the focus border tracking and no anomaly boxes.

## Fixed while finishing the project
1. **Desktop switch A → B → A left the window misplaced** (`desktop_round_trip` failed deterministically).
   Root causes, all in the renderer/daemon path:
   - Returning to a desktop triggered a 180 ms animation from a transient frame; desktop switches now
     re-place instantly (`Engine::instant`, `src/daemon.rs`).
   - A window within 2 px of its target is treated as settled instead of animated/parked
     (`src/render.rs`, scene handling).
   - `frame()` can report a transiently inset frame right after a window is placed or uncloaked, which
     skewed the border delta by 1 px per side. The renderer now re-checks frames ~250 ms after settling
     and re-places anything not exactly on target (up to 2 corrective passes, cancelled by any new scene).
2. `stop_restores_exact_rect` captured `WINDOWPLACEMENT` but never asserted it; it now does (exact
   pre-Planor placement restore). Removes the last compiler warning.
3. `desktop_round_trip` now waits 900 ms for animation + verification before capturing its baseline.

## 3-attempt fallbacks
None triggered. No Win32/COM/DirectComposition call needed to be isolated behind a fallback.
`winvd` works on build 26300 (its `ComNotInitialized` retry message in test output is its own benign
auto-retry, not an error).

## Known limitations / notes
- The `Windows Snap is enabled…` warning prints because Snap is on for this user; that is the spec'd warning.
- The corrective frame re-check cannot fix windows whose app enforces a larger minimum size; it stops after
  2 passes rather than fighting the app.

## PENDING_MANUAL_VERIFICATION
- Physical 3-finger touchpad feel (axis lock, 1:1 scroll, snap on lift). Only synthetic HID is tested.
- Multi-monitor visual inspection by eye (mixed DPI / portrait / negative coordinates). Automated bleed check
  passed, but this needs a human look.
- Physically unplugging a monitor (minimize on disconnect, restore on reconnect, `pull-orphans`).
- `modal follows display transfer` and `bleed` were exercised with synthetic move events, not a real mouse drag.

## 5-minute morning verification checklist
1. `target\release\planor.exe start` — console shows only the Snap warning (if Snap is on); open 3 windows and
   confirm first-pair 50/50 split, then 3rd opens right of the focused tile.
2. `Alt+Left/Right` moves focus and scrolls; peek sliver visible; `Alt+Up/Down` after `Alt+S` cycles a stack.
3. Three-finger swipe horizontally scrolls the strip; vertically cycles a stack; two-finger scroll is untouched.
4. Switch virtual desktops (Win+Ctrl+Arrow) away and back: windows return exactly on their tiles, no flicker/offset.
5. Drag a window over a tile's edge (reflow gap) and over its center for 350 ms (stack preview, hold Shift to freeze).
6. `planor diag live` shows frame stats; `planor reload` after editing `%APPDATA%\planor\planor.toml`.
7. If you have a spare monitor cable: unplug it, then `planor pull-orphans`.
8. `planor stop` (or `Ctrl+Alt+Shift+Backspace`) — every window returns to its original rect.

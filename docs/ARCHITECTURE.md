# Planor Architecture Reference

Guide for agents working on Planor (Windows 11 headless scrolling tiling window manager, Rust 2024). Every detail below is verified against `src/`.

## Thread & Dataflow Overview

```mermaid
flowchart LR
  subgraph OS["OS Events Thread (Planor.Events)"]
    WINEVENT["SetWinEventHook (6 ranges)"]
    RAWINPUT["WM_INPUT (Raw HID precision touchpad)"]
    HOTKEYS["WM_HOTKEY (RegisterHotKey)"]
    RELOAD_TMR["WM_TIMER (200 ms reload poll)"]
    DISPCFG["WM_DISPLAYCHANGE / WTS / Power"]
  end

  subgraph HOOKS["LL Input Hooks Thread (skipped in test_mode)"]
    MOUSEHOOK["WH_MOUSE_LL (peek clicks)"]
    KEYHOOK["WH_KEYBOARD_LL (Win+W note)"]
    REINSTALL["WM_TIMER (4000 ms reinstall)"]
  end

  subgraph DAEMON["Main Daemon Thread (32 ms tick + PipeServer)"]
    LOOP["crossbeam select!(events, tick 32 ms)"]
    ENGINE["Engine + Strip + Journal"]
    PIPE["PipeServer (\\\\.\\pipe\\planor-ipc)"]
  end

  subgraph RENDER["Render Thread (COINIT_APARTMENTTHREADED)"]
    HOSTS["Host (Planor.Composition) + D2D hole mask / thumb SetWindowRgn"]
    SURF["D3D11 + DirectComposition + Direct2D Surface (device-lost recovery)"]
    WGC["Windows Graphics Capture Source + DWM Thumbnails"]
    PLACE["win32::place + DwmFlush + 250 ms verify"]
  end

  subgraph WATCHDOG["Watchdog Thread (50 ms check)"]
    STALL["Detects >= 300 ms stall -> STALL + ring dump"]
    COUNTERS["250 ms CPU / RAM / GDI / USER / handles sampler"]
  end

  OS -->|crossbeam Event| LOOP
  HOOKS -->|Peek / note::open| LOOP
  PIPE -->|Command| ENGINE
  ENGINE -->|render::Message::Scene| RENDER
  RENDER -->|Event::Rejected / Event::Frame| LOOP
  DAEMON -.->|atomic QPC heartbeat| WATCHDOG
  RENDER -.->|atomic QPC heartbeat| WATCHDOG
```

## Thread Responsibilities

1. **Main Daemon Thread** (`src/daemon.rs::run`, COM `COINIT_MULTITHREADED`, parameterized `Engine<B: WindowBackend>` via `src/backend.rs::Win32Backend`):
   - Acquires single-instance mutex `Local\Planor.Daemon[-<PLANOR_INSTANCE>]` and creates non-blocking named pipe `\\.\pipe\planor-ipc[-<PLANOR_INSTANCE>]`.
   - On startup, restores any leftover `recovery.json`, runs `win32::tweak_system` (only when `PLANOR_INSTANCE` is unset), loads merged config, runs `Engine::topology()`, scans existing windows (unless `tile_existing_on_startup = false`, batching `Journal::save` once across the initial scan), and runs the main loop:
     - Calls `engine.layout()` when `dirty` is true — both at the top of the loop (for event wakeups) and immediately after `server.poll()` (for zero-wait IPC hotkey/command dispatch).
     - Polls `PipeServer::poll()` for IPC commands (`stop`, `reload`, `pause`, `resume`, `pin`, `pull-orphans`, `diag`, `state`, `perf`, `key`).
     - `crossbeam_channel::select!` over `events` (from `win32::events` & `render`) and a **32 ms** `tick` timer (`Engine::tick`, with monitor enumeration throttled to **500 ms** and virtual-desktop checks to **150 ms**).
   - **Simulated Backend** (`src/backend.rs::sim::FakeBackend` + `src/sim_tests.rs`): implements `WindowBackend` over `Arc<Mutex<SimDesktop>>`, enabling deterministic multi-monitor, display hot-unplug, drag, focus, group, fullscreen, and journal recovery tests in `< 0.5 s` without OS desktop dependencies.
2. **OS Events Thread** (`src/win32.rs::events`, hidden `Planor.Events` message window):
   - Installs `SetConsoleCtrlHandler(ctrl)` (blocks up to **4500 ms** polling `SHUTDOWN_DONE` so console close/shutdown restores windows before process termination).
   - Calls `WTSRegisterSessionNotification` (session lock/unlock via `WM_WTSSESSION_CHANGE`).
   - Registers raw HID precision touchpad sink (`usUsagePage = 13`, `usUsage = 5`, `RIDEV_INPUTSINK`) unless `test_mode()`.
   - Registers `[hotkeys]` via `RegisterHotKey` (`input::bindings`) unless `test_mode()`; a **200 ms** `WM_TIMER` re-applies bindings on `Command::Reload`.
   - Hooks 6 `SetWinEventHook` ranges (`WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS`): `EVENT_SYSTEM_FOREGROUND`, `EVENT_OBJECT_SHOW..HIDE`, `EVENT_OBJECT_LOCATIONCHANGE`, `EVENT_OBJECT_DESTROY`, `EVENT_SYSTEM_MINIMIZESTART..END`, `EVENT_SYSTEM_MOVESIZESTART..END`.
3. **Low-Level Input Hooks Thread** (`src/win32.rs::events`, skipped when `test_mode()`):
   - Dedicated thread so LL hooks never block on HID parsing or window enumeration.
   - Installs `WH_MOUSE_LL` (intercepts left clicks inside `win32::PEEKS` and emits `Event::Peek(hwnd)`) and, when diagnostics are enabled, `WH_KEYBOARD_LL` (intercepts `Win+W` -> `note::open()` and injects dummy `0xE8` key up/down so Start Menu does not pop).
   - **4000 ms** `WM_TIMER` uninstalls and reinstalls both hooks because Windows silently drops hooks that ever stall or across session transitions.
4. **Render Thread** (`src/render.rs::start`, COM `COINIT_APARTMENTTHREADED`):
   - Maintains one `Host` (`Planor.Composition` popup window, `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE`) per monitor bounds `m.2`.
   - `WS_EX_TRANSPARENT` makes every pixel click-through to the taskbar and underlying apps; `WM_NCHITTEST` cannot forward cross-process clicks on Windows.
   - Owns D3D11 + DirectComposition + Direct2D `Surface` (with cached RGB `ID2D1SolidColorBrush` instances) for drawing borders and WGC (`GraphicsCaptureItem`) backing bitmaps (`Source`), plus `DwmRegisterThumbnail` handles for DWM stand-in thumbnails. Caches failing WGC `Source::new` keys for a **5 s** cooldown (`failed_sources`) so un-capturable windows do not stall subsequent warm scenes.
   - **GPU device-lost recovery**: tracks a monotonic shared-device generation across `SHARED` (`ID3D11Device` / `IDCompositionDevice` / `ID2D1Device`). On `DXGI_ERROR_DEVICE_REMOVED` / `DEVICE_HUNG` / `DEVICE_RESET` / `DRIVER_INTERNAL_ERROR` / `D2DERR_RECREATE_TARGET` (or a throttled idle `GetDeviceRemovedReason` poll), resets `SHARED` at most once per dead generation, invalidates every host on that generation (bumping `host.generation` so in-flight WGC worker deliveries from the dead device are dropped), and retries recreation with bounded exponential backoff (`SurfaceRecovery`: 250 ms → 500 ms → 1000 ms → 2000 ms, reset only after a full normal draw succeeds).
   - Uses adaptive `recv_timeout` (`0 ms` animating, `33 ms` idle with active sources/verify, `250 ms` during surface recovery, `50 ms` idle otherwise), animates active `Scene` at monitor refresh cadence (`DwmFlush()`), and preempts immediately whenever `render::EPOCH` advances.
   - Schedules post-settle `verify` **250 ms** after `t >= 1.0` (up to **2** corrective passes, tagged with the scene's `epoch`) for windows whose DWM extended frame transiently mismatched on uncloak/desktop switch; only emits `Event::Rejected` for responsive, size-stable constraints matching the verified epoch.
5. **Watchdog Thread** (`src/watchdog.rs`):
   - Polls atomic QPC heartbeats for `daemon` and `render` every **50 ms**; emits `STALL <thread> <ms> span=<span_name>` if no progress occurs for **>= 300 ms** (configurable via `PLANOR_STALL_MS`), and triggers a stall capture bundle (plus `planor-rec dump` if ring recorder is running).
   - Samples process/thread CPU, working set, GDI/USER object counts, and OS handles every **250 ms** for `perf`.
6. **Optional Auxiliary Threads**:
   - `record_input()` (`src/daemon.rs`, every **15 ms**, only when `diagnostics.enabled = true && !test_mode()`): polls `GetCursorPos` and `GetAsyncKeyState` for `IN` trace lines (text keys counted, never named).
   - `note::open()` (`src/note.rs`): spawns one transient thread per `Win+W` note UI (`Planor.Note`).

## Tile Rect vs OS Outer Rect

- Windows 11 top-level windows have invisible resize shadow margins (~7 px on left/right/bottom).
- **Tile rect** (`win32::frame(id)`): queries `DwmGetWindowAttribute(..., DWMWA_EXTENDED_FRAME_BOUNDS, ...)` (falling back to `GetWindowRect`). All layout math in `src/core.rs` (`Strip`, `Rect`) and `EXPECT` / `PLACE` trace lines operate on **extended frame coordinates**.
- **OS rect compensation** (`win32::place`): computes inset delta `full - vis` from `GetWindowRect` vs `win32::frame` and shifts/resizes the outer window via `BeginDeferWindowPos` -> `DeferWindowPos(..., SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER)` -> `EndDeferWindowPos` so the visible frame lands at the exact tile rect.

## Parked Windows, DWM Stand-Ins, and WGC Bitmaps

- **Park X coordinate**: `park = s.monitors.iter().map(|m| m.2.x + m.2.w).max().unwrap_or(0) + 1024`.
- **When a real window is parked (`proxy = true`)**: any non-pinned tile that is `moving` (`t < 1.0 && origin != target`), clipped by `bound` (`clip != Some(rs)`), a stack `preview`, or `hidden` (stack member beyond active +/- 1) has its real HWND moved off-screen to `(park, tile.rect.y)` while the overlay host renders its stand-in clipped to `bound`:
  - **WGC bitmap (`Source`)**: captured at the window's content size and drawn onto the Direct2D `Surface` at `rs` (keeping `o.w, o.h` constant while sliding so contents do not stretch during size changes).
  - **DWM thumbnail (`Host::thumbnail`)**: live DWM composition thumbnail cropped to `clip`, hidden during motion when a WGC bitmap exists so the bitmap and thumbnail do not double-draw.
- **`SetPeeks`**: whenever `proxy && !moving && !full`, the clipped rectangle is published to `win32::PEEKS` so the `WH_MOUSE_LL` hook turns clicks on peek slivers into `Event::Peek(hwnd)`.

## Overlay Holes (D2D Masking + Z-Order Subtraction + UIPI)

- Unmanaged visible top-level windows (`win32::overlay_occluder(w)` true and not managed/pinned, e.g., Task Manager, `action = "ignore"` rules, system flyouts, or Planor's own floating UI) must never be covered by Planor's topmost overlay when they sit above managed tiles.
- **Z-order subtraction (`Engine::holes`)**: walks `windows()` top-to-bottom. Visible, uncloaked, non-iconic managed and pinned windows on the current virtual desktop are accumulated in `managed_above`; each unmanaged occluder has `managed_above` subtracted (`subtract_rects`) so an unmanaged window sitting *behind* managed tiles never erases their focus borders. `Engine::tick()` polls `self.holes()` every **200 ms** and marks `self.dirty = true` if the list changed.
- **Direct2D hole masking (`Surface::draw`)**: borders, drop zones, and cover cards are masked directly in Direct2D using `D2D1_PRIMITIVE_BLEND_COPY` with a transparent brush, avoiding `SetWindowRgn` mutations during normal desktop use (which caused DWM black flashes while moving Task Manager).
- **Thumbnail-only `SetWindowRgn`**: `Host` only applies `CreateRectRgn` + `CombineRgn(..., RGN_DIFF)` + `SetWindowRgn(host.h, whole, 0)` (`bRedraw = 0`) for holes that actually intersect an active DWM thumbnail (`holes_intersecting_rects`), and always draws/commits and updates the region *before* calling `ShowWindow(SW_SHOWNOACTIVATE)`.
- **Windows UIPI & elevated apps (e.g., Task Manager)**: Planor runs unprivileged (standard user integrity), so Windows User Interface Privilege Isolation (UIPI) prevents Planor from reordering elevated windows or forcing them above/below its topmost overlay via `SetWindowPos`. Instead, Planor leaves elevated windows unmanaged, removes the focus highlight when an elevated window (or its owned dialog) holds foreground, and punches Z-order-aware holes in the overlay wherever the elevated window is visible in front of managed tiles. Users who want Task Manager to stay above all non-elevated windows can enable Task Manager's own **Settings → Always on top** option.
- In isolated instances (`!PLANOR_INSTANCE.is_empty()`), `Engine::holes()` skips general desktop apps unless `PLANOR_HOLES=<ClassName>` is set, avoiding false cut-outs from the developer's background windows.

## Touchpad Gesture Pipeline

1. `WM_INPUT` (`win32::proc`) -> `input::contacts(l)` parses HID precision touchpad usages (page 13 link collections, contact count `0x54`, tip switch `0x42`, contact ID `0x51`, X/Y `0x30`/`0x31`), converts to pixel-scaled units using `cursor_dpi()` (`MonitorFromPoint` + `GetDpiForMonitor` — never `WindowFromPoint` so hung apps cannot stall input).
2. `core::ContactFrame::packet` assembles split/hybrid HID contact reports into complete 3- or 4-finger frames.
3. `core::Gesture::sample` classifies:
   - **3 fingers**: picks axis after a tiny deadzone (`deadzone.min(3.0)`), emitting `Swipe::Scroll(dx)` sideways, `Swipe::ScrollV(dy)` vertically, and `Swipe::ScrollEnd` on lift.
   - **4 fingers**: after `deadzone` distance, emits `Swipe::DragStart`, `Swipe::DragMove(dx, dy)`, and `Swipe::DragEnd`.
4. `Engine` handling (`src/daemon.rs`):
   - **Axes follow the monitor** (`scroll_by`): fingers *along* the strip (sideways on landscape, up/down on portrait) scroll it 1:1 (`Strip::scroll`, `self.instant = true`) + `Strip::focus_nearest_center()`; fingers *across* it `Strip::roll` the focused group, or with no group hop to the adjacent monitor that way after 90 units (`hopped`: once per gesture). On lift (`pad_scroll_end`), `inertia` (decay `0.9^(dt/0.032)`, stepped by the **8 ms** `fast_tick` arm of the main loop, only while `wants_fast_tick()`) if recent velocity > 150, else `pad_settle` in **220 ms** (or **120 ms** after inertia). Foreground events are ignored while a scroll settles.
   - **Gesture lock** (`Gesture::sample`): once a drag or scroll started, a change between 3 and 4 fingers re-bases the centroid instead of ending it; it ends below 3 fingers.
   - **Win + wheel** (`WH_MOUSE_LL` -> `Event::Wheel` -> `Engine::wheel`): mode is decided at the start of a wheel burst and held until settle (260 ms). Plain `Win + wheel` scrolls the strip under the cursor on its axis (`mouse.win_wheel_step_px`), or rolls the focused group one member per notch when a group is focused; `Win + Shift + wheel` inverts that (scrolling the strip when a group is focused); tilt wheel scrolls on landscape and rolls on portrait.
   - **Drop** (`drag_update` / `drag_end`, `core::drag`, `core::stack_zone`): positional only. Zones (`drop_zones`) = the middle `stack_zone_percent` (50 %) of each column's active window along the strip axis, spanning its full depth (full height on landscape, full width on portrait, #103; the same rects are hit-tested and drawn) on the monitor under the cursor, sent as `Scene::zones` and filled by the renderer. In a zone: `Stack`; else same monitor: `Swap` (`Strip::swap`, also within a group onto a sibling peek); a grouped window onto another window or any drop from another monitor: `Before`/`After` by half. No preview reflow.
   - **4-finger drag (`pad_begin` / `pad_move` / `pad_end`)**: grabs active window at center, moves cursor + window via rate-limited (**8 ms**) async `SetWindowPos(..., SWP_ASYNCWINDOWPOS)`, and on lift triggers cross-monitor cursor-only fling if lift speed >= `swipe_fling_speed` before running `drag_end`.

## Config Merge & Reload

- `src/main.rs::config()` ensures `%APPDATA%\planor\planor.toml` exists (seeded from embedded `include_str!("../planor.example.toml")`).
- `parse()` loads the embedded `planor.example.toml` as defaults, then merges every key from `%APPDATA%\planor\planor.toml` with type/range checks (`0..=100000` for ints, finite `> 0` for floats, `#RRGGBB` hex for `*_color`, known enums for `curve` and `drag_stack_modifier`).
- `[[rules]]` in user config replaces the default `rules` array wholesale (each entry must have valid `action` in `ignore|float|tile` and at least `process` or `class`).
- `planor reload` re-reads config, bumps `config_version`, forwards updated config to `win32::events` over `reload` channel to rebind hotkeys, updates strip gaps, and triggers layout.

## Recovery Journal & System Tweaks

- **Journal** (`%LOCALAPPDATA%\planor\recovery.json`): `Journal::capture(hwnd)` records `pid`, unique `cookie` (also attached as HWND property `Planor.Recovery.Identity`), `WINDOWPLACEMENT`, `GetWindowRect`, and `WS_EX_TOPMOST` before tiling, persisting atomically via `recovery.tmp` -> `sync_all()` -> `fs::rename`.
- **Restore**: `Journal::restore()` validates `pid` and `Planor.Recovery.Identity` cookie on every entry, applies `SetWindowPlacement` + `SetWindowPos`, removes the property, and deletes `recovery.json` once empty. Stale dead entries are pruned automatically.
- **Shutdown & Crash Rescue**: `Command::Stop`, `Event::Stop`, `panic_exit`, and `CTRL_*` restore all journaled windows before exit (`SHUTDOWN_DONE`). If the daemon was killed (`SIGKILL`/Task Manager), `planor stop` calls `daemon::recover()` directly from CLI, and the next `planor start` restores any journal at startup before tiling.
- **System Tweaks** (`win32::tweak_system`): disables Windows Snap Assist / JointResize in HKCU (backing up original values to `%LOCALAPPDATA%\planor\system-settings.json`, skipped when `PLANOR_INSTANCE` is set) and restores them on stop/recover.

## Trace Line Glossary (`%LOCALAPPDATA%\planor\trace.log`)

Lines are stamped with UTC wall clock `HH:MM:SS.mmm` (`trace.prev.log` holds the previous run):

| Prefix | Meaning |
|---|---|
| `EXPECT id=... rect=x,y,w,h monitor=... kind=tile\|peek\|parked\|fullscreen hidden=0\|1 animate_ms=N` | Target rectangle Planor expects for each window in a scene |
| `SCENE seq=N tiles=N holes=N` | Scene dispatched from daemon to render thread (`seq` monotonically increases per daemon run) |
| `PLACE id (old) -> (new)` | First placement pass per scene when current DWM frame differs from target |
| `FG hwnd managed=bool external=bool pad=bool` | `EVENT_SYSTEM_FOREGROUND` arrival |
| `EV <name> ...` | Dispatched `win32::Event` (`window`, `key`, `topology`, `suspend`, `rejected`, `peek`) |
| `PAD ...` | Touchpad gesture transitions (`contacts`, `scroll`, `DragStart`, `DragEnd`, `ScrollEnd`, `step`) |
| `IN ...` | Polled cursor position / button / non-text key events (text keys aggregated as `IN text-keys xN`) |
| `SLOW <span_or_step> <ms> ...` | Operation exceeding its latency threshold |
| `STALL <thread> <ms> span=<span_name>` | Watchdog detection of daemon or render thread stuck >= 300 ms |
| `INVARIANT <name> <details>` | Violated layout or settled-parked invariant |
| `NOTE <text> [bundle=<dir>]` | User dictation submitted via `Win+W` (with attached context bundle path) |

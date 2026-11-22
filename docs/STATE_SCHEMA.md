# `planor state --json` Schema (`schema_version = 1`)

Versioned state dump returned by `planor state --json` over the IPC pipe (`\\.\pipe\planor-ipc[-<PLANOR_INSTANCE>]`).

## Top-Level Object

| Field | Type | Description |
|---|---|---|
| `schema_version` | `u32` | Always `1` for this format |
| `uptime_ms` | `u64` | Milliseconds since daemon startup |
| `config_version` | `u64` | Starts at `1`; increments by `1` on each successful `planor reload` |
| `pid` | `u32` | Daemon process ID |
| `instance` | `string` | `PLANOR_INSTANCE` suffix (empty for the default user daemon) |
| `test_mode` | `bool` | Whether global hotkeys / raw HID / low-level hooks are disabled |
| `paused` | `bool` | Whether tiling is paused |
| `locked` | `bool` | Whether session is locked / suspended |
| `seq` | `u64` | Monotonic scene sequence counter (`SCENE seq=N`) |
| `active_desktop` | `string \| null` | Current virtual desktop UUID (`u128` hex) |
| `active_monitor` | `string` | Active monitor device identifier |
| `park_x` | `i32` | Off-screen X coordinate (`max(monitor.x + monitor.w) + 1024`) |
| `stalls` | `u64` | Watchdog-detected stall count since start |
| `monitors` | `MonitorState[]` | Attached display topology |
| `strips` | `StripState[]` | Strips on the active virtual desktop |
| `tiles` | `TileState[]` | Every managed tile + pinned window on the active desktop |
| `parked` | `ParkedState[]` | Subset of tiles expected off-screen (`preview`, `hidden`, or scrolled off monitor) |
| `pinned` | `i64[]` | Window handles (`HWND`) currently pinned floating topmost |
| `orphans` | `i64[]` | Window handles currently parked/minimized due to a detached monitor |
| `homes` | `Record<string, string>` | Map `hwnd -> home monitor name` for displaced windows |

## Nested Objects

- **`Rect`**: `{ "x": i32, "y": i32, "w": i32, "h": i32 }` (physical pixels, DWM extended frame coordinates).
- **`MonitorState`**: `{ "name": string, "work": Rect, "bounds": Rect, "horizontal": bool }`.
- **`ColumnState`**: `{ "index": usize, "members": i64[], "active_index": usize, "active_id": i64, "length": i32, "minimum": i32, "frac": f64 }`.
- **`StripState`**: `{ "desktop": string, "monitor": string, "area": Rect, "gap": i32, "share": f64, "split": bool, "focus": usize, "offset": i32, "fullscreen_id": i64 | null, "columns": ColumnState[] }`.
- **`TileState`**:
  - `id`: `i64` (HWND)
  - `monitor`: `string`
  - `column_index`: `usize | null` (`null` for pinned windows)
  - `expected_rect`: `Rect` (strip target rectangle before parking)
  - `bound`: `Rect` (clipping boundary: monitor work area or full bounds when fullscreen)
  - `os_rect`: `Rect | null` (live `win32::frame(id)` sampled during `state`)
  - `color`: `string | null` (border hex color or `null`)
  - `fullscreen`: `bool`
  - `preview`: `bool` (non-active stack member peeking behind active member)
  - `hidden`: `bool` (stack member hidden beyond active ± 1)
  - `pinned`: `bool`
  - `parked`: `bool` (expected at `park_x` because clipped, preview, or hidden)
- **`ParkedState`**: `{ "id": i64, "monitor": string, "expected_rect": Rect, "os_rect": Rect | null, "reason": "clipped" | "preview" | "hidden" }`.

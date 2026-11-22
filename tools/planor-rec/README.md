# planor-rec — screen recorder and pixel analyzer

`planor-rec` records every monitor of a Windows desktop with DXGI desktop duplication, encodes it
through FFmpeg, and records mouse/keyboard activity, OS window rectangles and Planor's expected tile
rectangles on **one clock** (QueryPerformanceCounter, microseconds). `planor-rec analyze` then decodes
the video and checks Planor's claims against pixels using **pixel maths only — no LLM**. Only the few
frames the pixel checks cannot explain are exported (`review-pack`) for a Haiku subagent to look at.

- Crate: `tools/planor-rec` (workspace member of the Planor repo). Binary: `planor-rec`.
- Windows 11, physical pixels, per-monitor DPI aware.
- FFmpeg is only used to encode/decode; it is found via `PLANOR_FFMPEG`, then `PATH`, then the Winget
  Gyan full build, then the Winget essentials build.
- A copy of the binary for everyday use lives in `tools/planor-rec-bin/planor-rec.exe`, so rebuilding the
  crate never fails because the recorder is running.

## Build and test

```powershell
cargo build --release -p planor-rec
cargo test  --release -p planor-rec
```

The analyzer unit tests use synthetic frames and event lists only; no test needs a real screen. The
recorder's dropout handling is tested with a fake duplicator, clock and encoder (`src/timeline.rs`).

## Usage

```text
planor-rec record --out <dir> [--mic] [--mic-device <name>] [--monitors all|N] [--fps N]
                  [--max-size-mb N] [--max-minutes M] [--preset ultrafast] [--encoder-threads 3]
                  [--trace <planor trace.log>] [--mark <text>] [--pipe <name>] [--no-hotkeys]
planor-rec stop   [--pipe <name>]
planor-rec mark   [--pipe <name>] [text]
planor-rec analyze <dir> [--trace <planor trace.log>]
planor-rec review-pack <dir> [--max-images 12] [--include-unmeasurable]
planor-rec prune <dir-of-recordings> --keep N [--dry-run]
planor-rec probe <dir> --monitor N --rect x,y,w,h --from-ms A --to-ms B [--row Y]
```

### Recording

```powershell
planor-rec record --out "$env:TEMP\planor-run1" --monitors all
```

`record` runs in the foreground. It starts recording immediately and stops on:

- `planor-rec stop` (named pipe `\\.\pipe\planor-rec`, or `...-<name>` with `--pipe`),
- `Ctrl+C`,
- the global hotkey **Ctrl+Alt+Shift+R**,
- `--max-size-mb N` (the video/audio files reached N MB) or `--max-minutes M`; the recording is finished
  cleanly (`meta.json` written) and `meta.json` / the `session` stop event say why.

**Ctrl+Alt+Shift+M** adds a marker to the timeline. (`Ctrl+Alt+Shift+Backspace` is Planor's panic
exit and is deliberately not used.) At start the recorder says for each hotkey whether it was registered;
if another program already owns one, it prints `WARNING: hotkey Ctrl+Alt+Shift+R is already registered by
another program (Windows error 1409) ...`, stores it in `meta.json` (`hotkeys`) and in `report.md`, and the
other hotkey and `planor-rec stop` / `mark` keep working.

Each selected monitor produces:

| file | contents |
| --- | --- |
| `video-<monitor>.mp4` | H.264 CRF 16, constant `--fps` timeline (default: the highest current refresh rate of any monitor, owner rule; 60 if unreadable), `--preset` default `ultrafast` |
| `frames-<monitor>.csv` | `index,present_qpc_us,changed,state` — one row per encoded frame; `changed=0`: the previous picture was repeated because the screen did not change; `state` = `ok`, `lost` (no live capture, last picture repeated) or `reopened` (first fresh picture after a loss) |
| `ffmpeg-<monitor>.log` | FFmpeg stderr |

Row *n* of the CSV is video frame *n*. `index` is the timeline tick; it skips only when the encoder was
behind and a frame was dropped on purpose (see below).

Shared files:

| file | contents |
| --- | --- |
| `events.jsonl` | one JSON object per line, all `t` values on the QPC microsecond clock |
| `meta.json` | monitors (DXGI index, device, desktop rect, work area, DPI, primary), taskbar rects, QPC frequency, QPC value and UTC wall clock at start/end, fps, hotkey status, per-monitor capture statistics, topology changes, audio placement, stop reason |
| `audio.wav` | only with `--mic`: dshow microphone `"Microphone Array (AMD Audio Device)"`, 16 kHz mono 16-bit PCM, 44-byte header |
| `trace-start.log` / `trace.log` | copies of Planor's `%LOCALAPPDATA%\planor\trace.log` (or the `--trace` file) at start/stop, if present |

### Monitor dropouts (a monitor going black / blinking)

When a monitor changes mode or goes off and on (a DisplayPort link through a USB-C dock, a UAC prompt,
a driver reset), DXGI desktop duplication fails (`DXGI_ERROR_ACCESS_LOST` and friends). The recorder:

1. keeps the timeline at `--fps`: every tick still writes a frame, repeating the last good picture, and
   marks the CSV row `state=lost`;
2. reopens that monitor's duplication with back-off (100, 200, 400, 800, 1600, 2000 ms ...), looking the
   output up by device name because DXGI output indices can be renumbered;
3. marks the first fresh picture `state=reopened`;
4. watches the monitor list every 2 s; a change (monitor added/removed, a size or position changed)
   writes a `capture`/`topology` event with all monitor rectangles and reopens every capture;
5. if a monitor comes back with a different size the video cannot change size, so frames stay `lost`
   (one `capture`/`topology` event with `old_size`, `new_size`, `new_rect`) until the old size returns.

`capture` events in `events.jsonl` (`kind:"capture"`, `monitor`, `event`): `lost` (with the HRESULT text in
`reason`), `reopen_failed` (first attempt and every 5th), `reopened` (`lost_ms`, `attempts`, `rect`),
`topology`, `topology_reopen`, `encoder_behind` (`dropped`), `lost_at_stop`.

The analyzer reports each lost stretch as its own finding `capture_lost` (verdict `fail`; `time_ms` is the
start, `duration_ms` the length, `monitor`, and the logged reason in `detail`). No check runs on lost
rows, and running black/flat/frozen trackers are reset there, so a dropout is never reported as a black or
frozen monitor or a position failure. A move whose arrival could not be seen because frames were lost or
dropped is `unmeasurable`, not a lag failure. If the monitor's rectangle changed mid-recording
(`topology_change`, `unmeasurable`) later frames are not compared with the old rectangle.

Not testable without unplugging a monitor: the real DXGI error codes of a particular dock. The logic is
covered by unit tests with a fake duplicator that fails with `DXGI_ERROR_ACCESS_LOST`, fails to reopen
three times and then recovers (`src/timeline.rs`), and by an end-to-end analyzer test with a recording that
contains a 500 ms `lost` stretch (`tests/analyze_synthetic.rs`).

### Long sessions: size, CPU, memory, pacing

Memory cannot grow without limit: each monitor hands frames to its FFmpeg writer through a bounded queue
of 4 frames. If FFmpeg falls behind, the frame is dropped (no video frame and no CSV row for that tick, so
the video simply has a gap there), the recorder never stalls, and a `capture`/`encoder_behind` event says how
many were dropped; the analyzer reports it as `frames_dropped` (`unmeasurable`).

Measured on this machine (16 logical cores, three monitors: 1920x1200, 2560x1440, 1440x2560), `record` for
90 s, desktop mostly idle with some activity:

| setting | ffmpeg x3 CPU | ffmpeg x3 peak memory | planor-rec CPU / memory | size, 3 monitors | max frame gap |
| --- | --- | --- | --- | --- | --- |
| `--preset ultrafast --encoder-threads 3` (default) | 4.1 cores (26% of the machine) | 397 MB | 0.65 core / 248 MB | 25.6 MB (about 17 MB/min) | 33 / 33 / 57 ms, 0 dropped |
| `--preset veryfast --encoder-threads 3` | 5.4 cores (34%) | 1,098 MB | 1.0 core / 238 MB | 9.2 MB (about 6.4 MB/min) | 36 / 33 / 41 ms, 0 dropped |
| `--preset veryfast --encoder-threads 0` (x264 decides) | 6.8 cores (42%) | 2,860 MB | 0.9 core / 233 MB | 9.2 MB | 46 / 40 / 56 ms, 0 dropped |

The mean frame gap is 16.67 ms in every run. Memory is flat after the first seconds (no growth over 90 s).
Sizes depend on screen activity far more than on time: an idle monitor costs under 1 MB/min, a monitor
with animation or video several times that. Plan for roughly **6 to 20 MB per minute for all three
monitors**; `--max-size-mb` stops a recording at a size you choose.

`ultrafast` is the default because `veryfast` could not keep up with three monitors of live motion
(during a real Planor scroll animation it dropped 51 and 8 frames on two monitors even with 6 encoder
threads, and the recorder's own CPU load slowed Planor down: lag measured on that recording is not
trustworthy). With `ultrafast` the same animation recorded with 0 dropped frames. The analyzer's accuracy
was checked on `ultrafast` recordings of a healthy desktop (0 failures, same position passes as `veryfast`).

`planor-rec prune <dir-of-recordings> --keep N` deletes the oldest recordings in a folder (by the start
time in `meta.json`) and keeps the newest N. Only direct sub-folders that contain a `meta.json` written by
`planor-rec` are ever touched; `--dry-run` lists what would go.

A caution that cost a bug: NTFS directory entries show a stale size (often 0) for a file another process
still has open, so `Get-ChildItem` shows 0-byte videos while FFmpeg is recording. The recorder asks the
file handle for the size (`--max-size-mb` depends on it).

### Microphone

`--mic` records the microphone through a separate FFmpeg (dshow) process into `audio.wav`
(`microphone recording: "<device>" -> ...\audio.wav (16 kHz mono, local only)` is announced on the
console). Without `--mic-device` the recorder prefers `Microphone Array (AMD Audio Device)` and otherwise
uses the first dshow audio device (with a warning). On stop the recorder sends `q` so FFmpeg finishes the WAV
header itself (and writes no LIST chunk, so the data starts at byte 44); if FFmpeg has to be killed the
header lengths are patched by walking the chunks.

Verified with a real recording (a 4.5 s file from a ~5.3 s recording): RIFF/WAVE, PCM, 1 channel, 16000 Hz,
16-bit, header sizes consistent with the file, audio clearly not silent. **FFmpeg's dshow start-up takes about
1 s**, so the audio begins late. `meta.json` therefore records the placement of the audio on the recorder's
clock (`audio`): `spawn_qpc_us`, `end_qpc_us`, `duration_ms`, `start_qpc_us` (= stop time minus the length
actually recorded), `offset_to_video_start_ms`. The report adds, for each monitor, the audio start relative to
that monitor's first video frame (`audio_offsets_to_first_frame_ms`): the frame for a spoken moment at audio
time `a` ms is the first row of `frames-N.csv` with `present_qpc_us >= audio.start_qpc_us + a*1000`.
Accuracy is about ±100 ms (it assumes the audio ended when the stop request was sent).

### Events (`events.jsonl`)

All events have `t` (QPC µs) and `kind`:

- `input` / `input:"mouse"`: `x,y` when the cursor moves (~60 Hz polling).
- `input` / `input:"button"`: `button` in `left|right|middle`, `down` bool.
- `input` / `input:"key"`: `key` name, `down` bool. Only **non-text** keys are ever named:
  modifiers, arrows, F1–F24, Enter, Backspace, Esc, Tab, Win.
- `input` / `input:"text"`: `count`, `total` — number of text-producing key presses. **No key code
  and no character is written**, so a recording cannot reveal what was typed.
- `os`: full snapshot of every visible top-level window on change (and at least once a second):
  `hwnd`, `class`, `rect`, `z` (0 = top), `cloaked`, `foreground`, `minimized`. Window titles are
  never recorded.
- `mark`: `text`, optional `source` (`pipe`, `hotkey` or `cli`).
- `capture`: dropouts and topology changes, see above.
- `session`: start/stop bookkeeping (`start`, `hotkeys`, `hotkey`, `stop` with `reason`).

### Analysis

```powershell
planor-rec analyze "$env:TEMP\planor-run1"
```

Writes `report.json`, `report.md` and `suspects/`:

- `report.json`: every check with `kind`, `verdict` (`pass|fail|unmeasurable`), timestamp,
  expected / OS-reported / measured rectangle, per-side differences, detail and nearest marker/note;
  `duration_ms` for lost stretches, `arrival_ms` / `late_ms` for lag; `recording.audio`, `.capture`,
  `.hotkeys`, `.audio_offsets_to_first_frame_ms`.
- `report.md`: human summary sorted by severity (fail, unmeasurable, pass) with the nearest `mark` or
  Planor `NOTE`, a "Lag measurements" section, the capture statistics and the audio alignment.
- `suspects/suspect-<id>.png`, `suspect-<id>-monitor.png`: crop around the window plus a low-resolution
  full monitor; `suspects/suspects.json` links each to its report check and the nearby events.
  `analyze --trace <file>` uses that Planor trace instead of the copy in the recording.

Checks:

1. **Position** — every side of a settled expected tile is measured from what Planor's layout and
   the known colours make unambiguous:
   - a side flush with the monitor/work-area/taskbar edge is confirmed by window pixels just inside
     it, and its position is that edge;
   - an inter-tile gap or open desktop is the dark-blue wallpaper strip touching the window; the
     measured side is the strip's **near** boundary (the far side is the neighbour and is deliberately
     never used, so the old 7 px "neighbour edge" error cannot happen). Shadow-darkened wallpaper
     still counts as wallpaper;
   - only when neither applies is a luma/edge step used as a fallback.

   Near-pure-black pixels (RGB ≤ 3 on every channel) count as wallpaper in shadow: a focused window's shadow
   can turn the 6 px inter-tile gap black (seen on a real recording), and without this the side fell back to a
   luma edge and landed on the neighbour's edge 7 px away. Genuinely dark content such as RGB(10,10,10)
   is still a window.

   Position *and* size are measured this way. Compare expected vs OS-reported vs measured, fail above
   the tolerance (2 px default). If no side can be measured the rectangle is `unmeasurable`, never
   `pass`; a side that could not be measured keeps its expected coordinate while the other sides are
   still verified (`Fit.sides` says which sides were really measured).
   **A position (or border, or interior) check only runs once the expectation has arrived.** A window that
   has not been placed yet is not at its rectangle, and measuring there reports the same lateness twice (and
   showed bogus "9 px" findings on the first scene). Each expectation is tied to its lag track; until the
   track has matched the target, or its watch time after the EXPECT line has passed (1 s, 2 s for the startup
   scene), only the `lag` / `startup_latency` finding applies. A position sample that falls in that time is
   held and run as soon as the arrival resolves; border scans wait until every live tile has arrived.

   **What is "expected":** a window is expected from the scene that lists it until the next scene that does
   not list it (a window being dragged by hand is in no scene: its old rectangle is not measured), and until
   the first `daemon-stopped` mark (`planor-rec mark daemon-stopped`; the test helpers add it in
   `Daemon::stop` / `kill`), after which Planor's windows go back where they were. A window that returns to
   the same rectangle after being away has to arrive again and is lag-tracked again.
2. **Lag** — for a changed expected rectangle, the first frame whose rectangle matches the target on
   **every side that lies inside the frame** (a fit that only verified the flush top and bottom of a tile
   would "match" while the tile is still 100 px away; that false match was found on a real recording and is
   fixed). Frames are watched from Planor's EXPECT line on. Reported: `arrival_ms` (EXPECT line to the first
   matching frame) and `late_ms` (arrival minus `animate_ms`; negative: early). Above 50 ms late fails, plus the
   recorder's own stall on the arrival frame: a rotated (portrait) monitor's frame is stamped after its GPU copy and
   CPU rotation (20 to 70 ms per frame under recording load), so `frames-N.csv` records that work per frame in the
   `rotate_stall_us` column (0 on unrotated monitors and in recordings made before the column existed) and the limit
   is `lag_ms` + that stall for the arrival frame. A lag that passes only with that allowance is reported with the
   stall in its detail. A lag beyond it, or never arriving within 1 s after the animation, fails (`unmeasurable` when the edge was never visible or
   frames were lost/dropped in the window). OS-rect changes are also measured for windows Planor does not
   manage. Targets that are mostly off the monitor are skipped (the position check calls them
   `unmeasurable`).

   **Startup latency (`startup_latency`, a separate check).** The first scene after a daemon start (`SCENE
   seq=1`; also the first `SCENE` of a trace file and any scene whose `seq` goes back down) is not held to
   the 50 ms threshold. Planor has to create the GPU overlay and the window capture sources before it can
   place anything (the trace says `SLOW render scene handling: hosts X ms, sources Y ms`), which takes about
   0.5 to 0.8 s and is accepted behaviour, but it is **measured, not hidden**: the finding `startup_latency`
   carries `arrival_ms` (EXPECT line to the window being at its rectangle) and passes up to 1500 ms
   (`STARTUP_LATENCY_MS`), fails above it or if the window never shows up within 2 s, and is `unmeasurable`
   when frames were lost or the edge was never visible. It appears in the "Lag measurements" list as
   `STARTUP`. Every later scene keeps the strict `lag_ms` threshold unchanged.
   Real values: 799 and 830 ms on monitor 0 in the last recorded run of `tiles_stay_inside_work_area`.
3. **Border placement** — pixels matching Planor's border colours (`#3B82F6` focused, `#A855F7`
   stacked, `#F59E0B` pinned, `#10B981` drop target, `#93C5FD` group hover) must lie on an expected
   tile outline, must not overlap a taskbar rect and must stay inside the monitor work area. A
   detected ring is checked pixel by pixel: at least 90% of its colour pixels must be within tolerance
   of an outline, so a stale border painted 6 px off one edge fails even when the other three edges
   line up. **Lookalikes from app content are not failures:** a thin line (under 12 px thick) is only a
   failure over the taskbar or outside the work area, and a box that is not on an outline (inside a tile,
   off every outline) must in addition be a rectangle with **at least 3 of its 4 sides present**
   (`image::sides_present`) before it is called a stray border, so an Explorer selected row (two 2 px
   light-blue lines, or an L-shaped accent) passes while a closed stray ring still fails. The foreground
   managed tile must also carry a border in one of those colours; missing borders are only reported after
   Planor has been seen painting borders at all, so a `borders.enabled=false` configuration is not called
   broken.
4. **Interior** — black or flat interior for more than 300 ms fails; an interior that does not
   separate from the background is `unmeasurable`.
5. **Monitor black / frozen** — mean luminance near zero, or no pixel change for more than 300 ms **after**
   windows on that monitor changed rectangle or the composited pointer moved over it. A screen that sat idle
   and then had activity is judged only once the picture has stayed unchanged for the threshold after the
   activity began (an earlier version flagged the first identical frame after an idle period). The first OS
   snapshot is a baseline, not activity. Borders are only checked while every expected tile has settled (a
   border animates with its tile).
6. **Frame pacing** — present-time gaps above 100 ms. The timeline ticks at a constant rate whether or not the
   screen changed (a static screen repeats the picture, CSV `changed=0`), so a gap in `present_qpc_us` is
   never "no new frame was delivered": it is the recorder's own tick loop having been stuck. Whenever one
   capture call takes 40 ms or more the recorder writes a `capture`/`slow_acquire` event with where the time
   went (`acquire_next_frame_ms`, `gpu_copy_map_ms`, `cpu_readback_copy_ms`, `cpu_rotate_ms`), and a pacing
   finding that overlaps such a call says so in its detail ("a recorder stall, not a display stall").
   The finding itself stays a failure: the video of that monitor is then late by that much and the lag
   numbers in it are only as good as the pacing.
   The cause that was found on the rotated (portrait) monitor is described under "Pacing gaps on the rotated
   monitor" below.
7. **Capture lost** — `capture_lost`, `frames_dropped`, `topology_change`, see above.
8. **Environment** — `environment_not_clean`. In the free work area (outside every live tile plus a 24 px
   margin, and outside the taskbar) the wallpaper must be the solid RGB(0,0,90). On every settled and arrived
   sample the share of non-wallpaper pixels there is measured; when it is 20% or more in at least half of
   the samples of a monitor, another program's window (a full-screen app, a browser) is behind or beside
   Planor's tiles. The analyzer then reports one `unmeasurable` finding `environment_not_clean` and turns
   that monitor's failing `position`, `border` and `border_missing`, `lag` and `startup_latency` findings into `unmeasurable` ones whose
   detail starts with `environment_not_clean`, instead of judging pixels it cannot attribute to Planor.
   Lag and `startup_latency` (read from the same pixels) are downgraded too; interior, pacing, capture and frozen/black findings are not. (The taskbar strip is not
   part of the criterion: its contents cannot be told from a foreign window by colour alone.)
   Checked on a real recording made before the backdrop existed (user windows on monitor 2): the check fires
   on monitor 2 (7 of 7 samples, 100% foreign) and not on monitor 0.

Planor's expected rectangles come from its trace: `EXPECT id=<hwnd> rect=<x>,<y>,<w>,<h>
monitor=<name> kind=<tile|peek|parked|fullscreen> hidden=<0|1> animate_ms=<duration>` and
`PLACE <hwnd> (x,y,w,h) -> (x,y,w,h)`. The analyzer converts the trace's UTC wall-clock stamps to the
QPC clock with `meta.json`; this was checked against a real recording: Planor's `IN down vk0x12` line and the
recorder's own Alt-down event are 3 ms apart. If no Planor trace with EXPECT lines is available it falls back to OS rectangles
(labelled `os`, border checks skipped), so it never claims to know Planor's intent when it does not. When the
trace has EXPECT lines but none for a monitor (the windows there are the user's own), that monitor gets **no
position or border check** at all: OS frame rectangles are not their visible pixels (invisible borders and
shadows) and checking them produced false failures; `report.json` lists such monitors in
`monitors_without_planor_tiles`. Black, frozen, pacing and capture checks still run there.

### Lag on a real animation (what was measured, what is missing)

Triggered with Alt+Right / Alt+Left only (Planor focus moves that scroll the strip) while recording, then
`analyze`: 12 window moves on the 2560x1440 monitor, `animate_ms=180`: arrival 168 to 195 ms after the EXPECT
line (average 185 ms), i.e. 11 ms early to 15 ms late against the end of the animation (average +5 ms), all
within the 50 ms threshold. Sequence seen in the trace: key down, EXPECT/SCENE 29 ms later, the real window
placed (`PLACE`) at about +190 ms. The video resolution is the 60 fps timeline (±17 ms).

What the trace does **not** contain, so the check cannot do more today: the moment the renderer actually
starts and ends the animation, the rectangle a tile moves *from*, and mid-animation positions. The lag
number is therefore "EXPECT line to the target on screen", not a per-frame check of the easing curve (`planor-rec probe`
prints the fitted rectangle per frame for a manual look). Key-press-to-EXPECT latency is in the trace (`IN
down` lines) but is not computed by the analyzer.

### Pacing gaps on the rotated monitor (cause found, 2026-11-22)

In recorded tests the 1440x2560 portrait monitor showed gaps of 60 to 139 ms between frames (threshold 100 ms);
the two landscape monitors never exceeded 49 ms. Evidence (instrumented recorder, `slow_acquire` events):

- Every gap row has `changed=1` and is followed by a catch-up burst of 2 to 4 ms frames: the tick loop was stuck
  inside one capture call, not waiting for a frame that never came. A static screen cannot cause a gap (the
  timeline repeats the picture at a constant rate), so a display or desktop-duplication "no new frame" artefact
  was ruled out and the pacing check was **not** relaxed.
- Inside that call the time was the CPU copy and rotation of the frame (the duplication frame of a rotated monitor
  must be turned into desktop orientation), not `AcquireNextFrame` (0 to 17 ms) nor the GPU copy and map
  (5 to 24 ms). The readback copy is 3 to 5 ms; the per-pixel rotation took 22 to 67 ms per frame (up to 116 ms)
  while three encoders, Planor and the recorder shared the CPU, against 9 ms measured on an idle machine.
- Fixes: rotation in 32x32 tiles, split over 4 threads (3.2 ms per frame idle, was 9 to 14), from a reused
  cached copy instead of a fresh 15 MB buffer per frame, the tick loop and rotation threads at higher thread
  priority, and the recorder process opted out of power throttling. Afterwards the portrait monitor's max gap in
  recorded runs was 70 to 94 ms and no pacing finding was raised in 9 of 10 runs; remaining rotation stalls
  (12 to 67 ms) still show as `slow_acquire` events. Whether Windows efficiency mode was a contributor was not
  proven.

### Review pack for a Haiku subagent

```powershell
planor-rec review-pack "$env:TEMP\planor-run1"
```

Copies only the flagged suspect crops (failures first; `--include-unmeasurable` adds the rest; near
duplicates and image-less kinds such as `capture_lost` are skipped; capped at `--max-images`, default 12) into
`<dir>\review\` — never the video — with `manifest.json` (image, kind, monitor, time, the finding's
expected / reported / measured rectangles, the nearest mark or `NOTE`, the surrounding events without mouse
spam) and `PROMPT.md`, a narrow instruction for the subagent (answer one line per image: glitch / ok / cannot
tell, with the glitches enumerated: border over the taskbar, blank window interior, half drawn window, black
monitor; do not guess). Crops are downscaled to 800 px and the monitor overview to 480 px and the pack stops
at 3 MB. `review-pack` is local files only; it contains no network code and sends nothing anywhere.

### Thresholds

Optional `planor-rec.toml`, looked for in the recording directory then the current directory
(`tools/planor-rec/planor-rec.example.toml` is a copy-ready example):

```toml
position_tolerance_px = 2
lag_ms = 50
black_flat_ms = 300
frame_gap_ms = 100
edge_search_px = 24
min_edge_score = 0.10
border_tolerance_px = 2
border_colour_tolerance = 40
settle_margin_ms = 80
gap_px = 6
```

Any missing key keeps its default. The values are validated into sane ranges.

## Recording automated tests

`tests/common/helpers.rs` has a `Recording` guard and `assert_recording_clean`, enabled only with
`PLANOR_RECORD=1` (see `docs/TEST_PLAN.md`, "Recording a scenario"). The guard starts `planor-rec record` with
its own `--out` folder, `--pipe` name, `--no-hotkeys`, the test name as a `--mark` and `--trace` pointing at the
isolated test daemon's trace (the daemon writes it to its own `LOCALAPPDATA`); on drop it stops the recorder,
runs `analyze --trace` and prints the summary. With an explicit `--trace` the recorder **follows** that file
and keeps its own copy (`trace.log` in the recording) as it grows: the isolated test daemon deletes its folder,
trace included, when it exits, which can be before the recording is stopped (this was found by running the
drag scenario). `analyze --trace <missing file>` falls back to that copy.

**Current state (2026-11-22, after the fixes below).** With a quiet desktop `tiles_stay_inside_work_area`
passes (0 fail, 4 unmeasurable) in recorded runs; `drag_to_other_landscape_monitor` still reports real
`lag` findings after a drop (arrival 237 to 353 ms after EXPECT, i.e. +57 to +173 ms against the 180 ms
animation, varying per run; the trace shows the drop scene spending about 95 ms in `SLOW render scene
handling` before it animates) and sometimes a marginal OS-rectangle lag of 53 to 55 ms on the portrait
monitor. Those are left as findings on purpose. **A recorded scenario needs a quiet desktop:** if the
developer's own window is raised over the tiles during the run (the tests drive real mouse and keyboard
input), the scenario's own assertions can fail and the analyzer reports `environment_not_clean`.
The notes below describe the earlier state and are kept for the history of the findings.

Both example scenarios were run with `PLANOR_RECORD=1`. `tiles_stay_inside_work_area` (three runs of the
recorder over time, same result in the last two) reports 5 failing findings and stops the test:
the initial layout is placed late (EXPECT at +2.6 s, the first `PLACE` 0.65 s later and the final placements
0.83 s later: 2 lag findings of about 860 ms and 2 position findings at +340 ms, which is the real startup
latency of the test daemon according to its own trace) and one 100 to 113 ms pacing gap on monitor 2 about
2.5 s after the recording started (which could be the recorder or the display, it was not separated). The
settled positions of the three tiles pass with exact measured rectangles. `drag_to_other_landscape_monitor` reports 13
findings in the same startup moment plus a `border_missing` on monitor 1, one lag of 241 ms (+61 ms) after the
drop and two more pacing gaps; those extra three were not triaged. So a recorded scenario currently goes red
on startup latency; either accept that as a finding or start asserting after a mark.

## Planor-side change

Diagnostics, when enabled, also writes one line per tile from `src/daemon.rs::layout`, immediately
before the `SCENE` line is emitted:

```text
EXPECT id=<hwnd> rect=<x>,<y>,<w>,<h> monitor=<name> kind=<tile|peek|parked|fullscreen> hidden=<0|1> animate_ms=<duration>
```

`animate_ms` is the configured animation duration for the current action (0 when animation is off).
The analyzer uses it to wait for the window to settle before measuring, and to time lag. The running
daemon emits these lines (checked on the real trace).

## Limits that cannot be fixed here

- **A real signal loss between the GPU and the monitor is invisible to desktop duplication.** If the cable,
  dock or panel drops the picture, Windows and the GPU still composite the desktop and DXGI keeps handing
  over frames, so the video shows a perfectly normal screen while the real monitor is black. Only a
  `capture_lost` finding (when Windows itself resets the output), the OS window events and Planor's trace can
  show such a dropout; the recording cannot prove a monitor was lit.
- **HDR is not supported.** Duplication of an HDR or 10-bit output is not 8-bit BGRA; the recorder refuses
  such a monitor with an explicit message. Turn HDR off for the recorded monitors.
- **The wallpaper must stay solid RGB(0,0,90).** The pixel classifier recognises wallpaper by blue
  dominance around that colour; with another wallpaper the measurement falls back or reports `unmeasurable`,
  never a guessed pass.
- **Parked and occluded windows are `unmeasurable`.** Planor parks the real window off every monitor and
  shows a stand-in image (peeks, group previews). That image is only measured where it is visible at the
  expected rectangle; if it is occluded, mostly off the monitor or not drawn there, the check is
  `unmeasurable` (never a false fail).
- H.264 CRF 16 is analysed, not lossless video; tolerances are sized for it.
- Each side is searched within ±24 px, so a window moved or resized by more than 24 px is reported
  `unmeasurable` by the pixel check instead of being measured; the expected-vs-OS comparison still
  catches the discrepancy when Windows and Planor disagree.
- A monitor that returns with a different size cannot be recorded into the same video (the size is fixed);
  its frames stay `lost` until the size returns.
- Missing borders are only inferred for the *foreground managed tile* (the trace carries no per-tile
  colour). A missing border is reported only after at least one Planor border was seen in the recording.
- Border colour matching is per-channel tolerant (default 40) because chroma subsampling shifts
  colours. Shape, coverage and the 3-sides rule reject most app content, but unusual UI can still produce a
  stray-colour suspect; `suspects/` and `review-pack` exist for that judgement.
- The recorder loads the machine: three encoders cost 4 cores (`ultrafast`) to 6 cores (`veryfast`) on this
  machine. Timing measured while the encoders are saturating the CPU (dropped frames) reflects that load.
- The microphone offset is estimated (about ±100 ms), not sample-exact.
- The mouse pointer is composited into the recorded frames by the recorder (DXGI supplies the shape
  separately). Cursor shape changes are cached between frames.
- Verified by hand with synthetic key events (`keybd_event`): Ctrl+Alt+Shift+M adds a hotkey marker,
  Ctrl+Alt+Shift+R stops the recording, and a hotkey owned by another program is reported (error 1409) without
  stopping the recorder. Not covered by automated tests (they need a real desktop).
- Not verified: a real monitor dropout. Unplugging or switching real monitors was out of bounds, so the
  reopen path is proven with a fake duplicator, not with a real dock.

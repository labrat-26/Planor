<#
.SYNOPSIS
  Feel suite: drive every Planor feature on the real desktop with real apps while planor-rec records the screen
  (at the highest monitor refresh rate) and Planor profiles itself, then report every animation hitch, slow
  step and dropped frame per scenario.

.DESCRIPTION
  Uses the owner's running planor.exe (rebuilt and restarted first, owner rule) and the owner's recorded touchpad
  gestures (tests\fixtures\gestures\recorded, replayed at their real pace through `planor gestures replay`).
  Scenarios: open apps, keyboard scroll, touchpad scroll, group + roll + ungroup, four-finger drags, move,
  dialog pop-up, maximize, fullscreen, mouse edge resize, close apps.

  Output: build\feel\<stamp>\  - rec\ (video, frames csv, events, report.md, suspects\), perf-<scenario>.json,
  feel.md (one table: per scenario animation frames, late %, dropped, worst frame, DWM compose fps, JANK and
  SLOW/STALL counts, analyzer failures), review\ (frames for the Haiku review).
  Do not use the machine while it runs (about 2 minutes).

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\feel.ps1
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\feel.ps1 -Only scroll,group
#>
param(
  [string]$Only = "",
  [switch]$NoRestart,
  [switch]$NoRecord
)
$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$exe = Join-Path $root "target\release\planor.exe"
$rec = Join-Path $root "tools\planor-rec-bin\planor-rec.exe"
$fixtures = Join-Path $root "tests\fixtures\gestures\recorded"
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$out = Join-Path $root "build\feel\$stamp"
New-Item -ItemType Directory -Force $out | Out-Null

if (-not $NoRestart) {
  Write-Host "== rebuilding and restarting planor.exe (owner rule)"
  $rb = & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "rebuild-daemon.ps1") 2>&1
  $rb | Set-Content (Join-Path $out "rebuild-daemon.log")
  if (-not ($rb | Where-Object { "$_" -match "^Ready:" })) { Write-Host "rebuild failed, see $out\rebuild-daemon.log" -ForegroundColor Red; exit 2 }
  Write-Host ("   " + ($rb | Where-Object { "$_" -match "^Ready:" } | Select-Object -Last 1))
}

Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices;
public static class U {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint f, IntPtr e);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
}
'@

function Planor([string[]]$a) { & $exe @a 2>&1 | Out-String }
function Key([string]$name, [int]$times = 1, [int]$pauseMs = 450) { for ($i = 0; $i -lt $times; $i++) { Planor @("key", $name) | Out-Null; Start-Sleep -Milliseconds $pauseMs } }
function Mark([string]$t) { if (-not $NoRecord) { & $rec mark $t 2>&1 | Out-Null } }
function Tap([byte]$vk, [byte[]]$mods = @()) { foreach ($m in $mods) { [U]::keybd_event($m, 0, 0, [IntPtr]::Zero) }; [U]::keybd_event($vk, 0, 0, [IntPtr]::Zero); [U]::keybd_event($vk, 0, 2, [IntPtr]::Zero); foreach ($m in $mods) { [U]::keybd_event($m, 0, 2, [IntPtr]::Zero) } }
function State { (Planor @("state", "--json")) | ConvertFrom-Json }
function Replay([string]$file, [string]$step, [int]$waitMs) {
  $r = Planor @("gestures", "replay", (Join-Path $fixtures $file), "--step", $step, "--gap-ms", "500")
  Write-Host "   replay [$step] $($r.Trim())"
  Start-Sleep -Milliseconds $waitMs
}
$launched = [System.Collections.Generic.List[object]]::new()

$scenarios = [ordered]@{
  "open_apps" = {
    # Windows are tracked by handle from Planor's own state: Calculator, Notepad tabs and console windows open
    # inside existing processes, so tracking new processes left them open (and could pick up strangers).
    $before = @((State).tiles | ForEach-Object { $_.id })
    foreach ($app in @("notepad.exe", "calc.exe", "mspaint.exe", "explorer.exe", "cmd.exe") | Where-Object { Get-Command $_ -ErrorAction SilentlyContinue }) {
      Start-Process $app | Out-Null
      Start-Sleep -Milliseconds 1400
    }
    Start-Sleep -Milliseconds 800
    @((State).tiles | Where-Object { $before -notcontains $_.id }) | ForEach-Object { $launched.Add([pscustomobject]@{ hwnd = [IntPtr][int64]$_.id }) }
    Write-Host "   opened $($launched.Count) window(s)"
  }
  "keyboard_scroll" = { Key "focus_left" 6 380; Key "focus_right" 6 380 }
  "touchpad_scroll" = {
    Replay "owner-guided-full-range.jsonl" "swipe LEFT, 3 times" 2500
    Replay "owner-guided-full-range.jsonl" "swipe RIGHT, 3 times" 2500
    Replay "owner-guided-full-range.jsonl" "FLICK right" 2500
    Replay "owner-guided-grouping.jsonl" "very SLOWLY" 6000
  }
  "group_roll" = {
    Key "stack_toggle" 1 700
    Replay "owner-guided-grouping.jsonl" "swipe UP and DOWN" 9000
    Key "focus_down" 2 500; Key "focus_up" 2 500
    Key "stack_toggle" 1 900
  }
  "drag" = {
    Replay "owner-guided-full-range.jsonl" "drag LEFT, 3 times" 4000
    Replay "owner-guided-full-range.jsonl" "drag RIGHT, 3 times" 4000
  }
  "move" = { Key "move_left" 3 500; Key "move_right" 3 500 }
  "popup" = {
    $np = Get-Process notepad -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
    if ($np) { [U]::SetForegroundWindow($np.MainWindowHandle) | Out-Null; Start-Sleep -Milliseconds 500 }
    Tap 0x4F @(0x11)            # Ctrl+O: the open dialog pops up
    Start-Sleep -Milliseconds 1500
    Tap 0x1B                    # Esc closes it
    Start-Sleep -Milliseconds 1000
  }
  "maximize" = {
    $h = [U]::GetForegroundWindow()
    [U]::ShowWindow($h, 3) | Out-Null; Start-Sleep -Milliseconds 1200   # SW_MAXIMIZE
    [U]::ShowWindow($h, 3) | Out-Null; Start-Sleep -Milliseconds 400
    [U]::ShowWindow($h, 9) | Out-Null; Start-Sleep -Milliseconds 1200   # SW_RESTORE
  }
  "fullscreen" = {
    $fg = Get-Process explorer | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
    $h = if ($fg) { $fg.MainWindowHandle } else { [IntPtr]::Zero }
    if ($h -ne [IntPtr]::Zero) { [U]::SetForegroundWindow($h) | Out-Null; Start-Sleep -Milliseconds 600 }
    Tap 0x7A; Start-Sleep -Milliseconds 1800   # F11 in, out
    Tap 0x7A; Start-Sleep -Milliseconds 1800
  }
  "resize" = {
    $s = State
    $t = @($s.tiles | Where-Object { -not $_.parked -and $_.expected_rect }) | Select-Object -First 1
    if ($t) {
      $r = $t.expected_rect
      $x = $r.x + $r.w - 2; $y = $r.y + [int]($r.h / 2)
      [U]::SetCursorPos($x, $y); Start-Sleep -Milliseconds 300
      [U]::mouse_event(0x2, 0, 0, 0, [IntPtr]::Zero)
      for ($i = 1; $i -le 30; $i++) { [U]::SetCursorPos($x - 8 * $i, $y); Start-Sleep -Milliseconds 16 }
      [U]::mouse_event(0x4, 0, 0, 0, [IntPtr]::Zero); Start-Sleep -Milliseconds 1200
      [U]::SetCursorPos($x - 240, $y); [U]::mouse_event(0x2, 0, 0, 0, [IntPtr]::Zero)
      for ($i = 1; $i -le 30; $i++) { [U]::SetCursorPos($x - 240 + 8 * $i, $y); Start-Sleep -Milliseconds 16 }
      [U]::mouse_event(0x4, 0, 0, 0, [IntPtr]::Zero); Start-Sleep -Milliseconds 1200
    }
  }
  "close_apps" = {
    # Only the windows this run opened (WM_CLOSE: apps may still ask to save; nothing is forced).
    foreach ($w in $launched) { [U]::PostMessage($w.hwnd, 0x10, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null; Start-Sleep -Milliseconds 900 }
    Start-Sleep -Milliseconds 1000
  }
}
$want = if ($Only) { $Only.Split(",") | ForEach-Object { $_.Trim() } } else { @() }
$run = @($scenarios.Keys)
if ($want) { $run = @($scenarios.Keys | Where-Object { $k = $_; $want | Where-Object { $k -like "*$_*" } }) }
# Always open apps first and close them last when anything uses them.
if ($run -notcontains "open_apps") { $run = @("open_apps") + $run }
if ($run -notcontains "close_apps") { $run += "close_apps" }

Planor @("diag", "on") | Out-Null       # EXPECT / SCENE trace lines for the analyzer
$recProc = $null
if (-not $NoRecord) {
  $recDir = Join-Path $out "rec"
  $recProc = Start-Process -FilePath $rec -ArgumentList @("record", "--out", "`"$recDir`"", "--monitors", "all", "--no-hotkeys") -WindowStyle Hidden -PassThru
  Start-Sleep -Seconds 3
}
$rows = @()
foreach ($name in $run) {
  Write-Host "== $name"
  Planor @("perf", "--reset") | Out-Null
  Mark "scenario $name"
  $t0 = Get-Date
  & $scenarios[$name]
  Start-Sleep -Milliseconds 600
  $json = Planor @("perf", "--json")
  $json | Set-Content (Join-Path $out "perf-$name.json") -Encoding UTF8
  $p = $json | ConvertFrom-Json
  Planor @("perf", "--trace", (Join-Path $out "perf-$name.trace.json"), "--seconds", "30") | Out-Null
  $a = @($p.animation | Where-Object { $_.action -eq "all" }) | Select-Object -First 1
  $slow = @($p.spans | Where-Object { $_.slow_count -gt 0 } | ForEach-Object { "$($_.name)x$($_.slow_count)" }) -join " "
  $rows += [pscustomobject]@{
    scenario = $name; seconds = [Math]::Round(((Get-Date) - $t0).TotalSeconds, 1)
    frames = if ($a) { $a.frames } else { 0 }; late = if ($a) { $a.late } else { 0 }; dropped = if ($a) { $a.dropped } else { 0 }
    late_pct = if ($a -and $a.frames) { [Math]::Round(100.0 * $a.late / $a.frames, 1) } else { 0 }
    p95_ms = if ($a) { [Math]::Round($a.p95_ms, 1) } else { 0 }; worst_ms = if ($a) { [Math]::Round($a.worst_ms, 1) } else { 0 }
    dwm_fps = [Math]::Round([double]$p.process.gpu.dwm_compose_fps, 0); stalls = $p.stalls; slow = $slow
    per_action = (@($p.animation | Where-Object { $_.action -ne "all" } | ForEach-Object { "$($_.action):$($_.late)/$($_.frames)" }) -join " ")
  }
}
Mark "feel done"
if ($recProc) {
  & $rec stop 2>&1 | Out-Null
  $recProc.WaitForExit(30000) | Out-Null
  Write-Host "== analyzing recording"
  & $rec analyze $recDir 2>&1 | Select-Object -Last 3 | ForEach-Object { Write-Host "   $_" }
  & $rec review-pack $recDir --max-images 16 2>&1 | Select-Object -Last 2 | ForEach-Object { Write-Host "   $_" }
}
Planor @("diag", "off") | Out-Null

$md = [System.Collections.Generic.List[string]]::new()
$md.Add("# Feel suite $stamp")
$md.Add("")
$md.Add("| scenario | s | anim frames | late (>1.5 refresh) | dropped (>2.5) | p95 ms | worst ms | DWM fps | stalls | slow spans | late/frames per action |")
$md.Add("|---|---|---|---|---|---|---|---|---|---|---|")
foreach ($r in $rows) { $md.Add("| $($r.scenario) | $($r.seconds) | $($r.frames) | $($r.late) ($($r.late_pct)%) | $($r.dropped) | $($r.p95_ms) | $($r.worst_ms) | $($r.dwm_fps) | $($r.stalls) | $($r.slow) | $($r.per_action) |") }
if ($recProc -and (Test-Path (Join-Path $recDir "report.json"))) {
  $rep = Get-Content (Join-Path $recDir "report.json") -Raw | ConvertFrom-Json
  $checks = @($rep.checks); if (-not $checks) { $checks = @($rep.findings) }
  $md.Add("")
  $md.Add("## Recording analysis (pixels)")
  $md.Add("")
  $checks | Group-Object { "$($_.kind) $($_.verdict)" } | Sort-Object Count -Descending | ForEach-Object { $md.Add("- $($_.Name): $($_.Count)") }
}
$md -join "`n" | Set-Content (Join-Path $out "feel.md") -Encoding UTF8
Write-Host ""
$md | ForEach-Object { Write-Host $_ }
Write-Host ""
Write-Host "Report: $out\feel.md"

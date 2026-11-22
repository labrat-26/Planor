<#
.SYNOPSIS
  Clean up stale Planor verification runs, watchdog stall dumps, temp test directories,
  and report any lingering daemon/recorder processes while preserving active user config & latest pointers.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\clean-sessions.ps1
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\clean-sessions.ps1 -KeepVerify 3 -PurgeStalls
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\clean-sessions.ps1 -DryRun
#>
param(
  [int]$KeepVerify = 3,
  [int]$KeepStalls = 3,
  [switch]$PurgeStalls,
  [switch]$CleanLiveBuild,
  [switch]$IncludeTarget,
  [switch]$DryRun
)

$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$removedItems = 0
$reclaimedBytes = [int64]0

function Measure-PathBytes([string]$path) {
  if (-not (Test-Path $path)) { return [int64]0 }
  $item = Get-Item $path -ErrorAction SilentlyContinue
  if (-not $item) { return [int64]0 }
  if (-not $item.PSIsContainer) { return [int64]$item.Length }
  $sum = [int64]0
  Get-ChildItem $path -Recurse -File -Force -ErrorAction SilentlyContinue | ForEach-Object {
    $sum += $_.Length
  }
  return $sum
}

function Remove-Tracked([string]$path, [string]$kind) {
  $bytes = Measure-PathBytes $path
  $script:removedItems++
  $script:reclaimedBytes += $bytes
  $mb = [Math]::Round($bytes / 1MB, 2)
  if ($DryRun) {
    Write-Host ("[dry-run] would remove {0}: {1} ({2} MB)" -f $kind, $path, $mb) -ForegroundColor DarkGray
  } else {
    Remove-Item $path -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host ("removed {0}: {1} ({2} MB)" -f $kind, $path, $mb) -ForegroundColor Gray
  }
}

# 1. Prune build\verify\20* runs, keeping newest $KeepVerify (preserve latest.* and cache.json).
$verifyRoot = Join-Path $root "build\verify"
if (Test-Path $verifyRoot) {
  $runDirs = @(Get-ChildItem $verifyRoot -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -match "^20\d{6}-\d{6}$" } | Sort-Object Name -Descending)
  if ($runDirs.Count -gt $KeepVerify) {
    foreach ($d in $runDirs[$KeepVerify..($runDirs.Count - 1)]) {
      Remove-Tracked $d.FullName "verify-run"
    }
  }
}

# 2. Optional stale build\live artifacts.
$liveDir = Join-Path $root "build\live"
if ($CleanLiveBuild -and (Test-Path $liveDir)) {
  Remove-Tracked $liveDir "build-live"
}

# 3. Prune %LOCALAPPDATA%\planor\stalls\* while never touching %APPDATA%\planor\planor.toml or wallpaper backup.
$localPlanor = Join-Path $env:LOCALAPPDATA "planor"
$stallsDir = Join-Path $localPlanor "stalls"
if (Test-Path $stallsDir) {
  $stalls = @(Get-ChildItem $stallsDir -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending)
  $keep = if ($PurgeStalls) { 0 } else { $KeepStalls }
  if ($stalls.Count -gt $keep) {
    foreach ($s in $stalls[$keep..($stalls.Count - 1)]) {
      Remove-Tracked $s.FullName "stall-bundle"
    }
  }
}

# 4. Clean stale Planor temp test folders/journals (safe when no test daemon is active).
$activeProcs = @(Get-Process planor, planor-rec, ffmpeg -ErrorAction SilentlyContinue)
$cutoff = (Get-Date).AddMinutes(-10)
$tempPatterns = @("planor-sim-*", "planor-live-*", "planor-test-*", "planor-ring-*", "planor-stall-*", "planor-watchdog-*", "journal-*.json")
foreach ($pat in $tempPatterns) {
  Get-ChildItem $env:TEMP -Filter $pat -ErrorAction SilentlyContinue | ForEach-Object {
    if ($activeProcs.Count -eq 0 -or $_.LastWriteTime -lt $cutoff) {
      Remove-Tracked $_.FullName "temp"
    }
  }
}

# 5. Optional cargo clean.
if ($IncludeTarget -and (Test-Path (Join-Path $root "target"))) {
  $tBytes = Measure-PathBytes (Join-Path $root "target")
  $script:removedItems++
  $script:reclaimedBytes += $tBytes
  if ($DryRun) {
    Write-Host ("[dry-run] would run cargo clean (~{0} MB)" -f [Math]::Round($tBytes / 1MB, 2)) -ForegroundColor DarkGray
  } else {
    cargo clean
  }
}

$totalMb = [Math]::Round($reclaimedBytes / 1MB, 2)
Write-Host ""
Write-Host ("clean-sessions summary: items={0}, reclaimed={1} MB, active_procs={2}" -f $removedItems, $totalMb, $activeProcs.Count) -ForegroundColor Cyan
if ($activeProcs.Count -gt 0) {
  $activeProcs | Select-Object Name, Id, WorkingSet64 | Format-Table -AutoSize
}

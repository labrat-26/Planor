<#
.SYNOPSIS
  Compare two verify.ps1 results.json runs (defaults to the two newest runs in build\verify\).
  Reports total duration delta, verdict changes, per-test speedups/regressions, and perf span deltas.

.EXAMPLE
  .\scripts\compare-runs.ps1
  .\scripts\compare-runs.ps1 -Base build\verify\20261122-080000\results.json -Head build\verify\latest.json
#>
param(
  [string]$Base = "",
  [string]$Head = "",
  [int]$Top = 10
)
$root = Split-Path -Parent $PSScriptRoot

function Load-Run([string]$path) {
  $raw = Get-Content $path -Raw -Encoding UTF8 | ConvertFrom-Json
  if ($raw -is [System.Array]) {
    return [pscustomobject]@{
      run_id = (Split-Path -Leaf (Split-Path -Parent $path))
      total_duration_ms = $null
      perf = $null
      results = @($raw)
    }
  }
  return $raw
}

if (-not $Head -or -not $Base) {
  $files = @(Get-ChildItem (Join-Path $root "build\verify") -Directory -ErrorAction SilentlyContinue |
    Sort-Object Name |
    ForEach-Object { Join-Path $_.FullName "results.json" } |
    Where-Object { Test-Path $_ })
  if ($files.Count -lt 2) {
    throw "need at least two results.json runs under build\verify\ (or pass -Base and -Head)"
  }
  if (-not $Base) { $Base = $files[$files.Count - 2] }
  if (-not $Head) { $Head = $files[$files.Count - 1] }
}

$b = Load-Run $Base
$h = Load-Run $Head

Write-Host "Compare: $($b.run_id) -> $($h.run_id)" -ForegroundColor Cyan
if ($null -ne $b.total_duration_ms -and $null -ne $h.total_duration_ms) {
  $deltaMs = [int64]$h.total_duration_ms - [int64]$b.total_duration_ms
  $pct = if ($b.total_duration_ms -gt 0) { [Math]::Round(100.0 * $deltaMs / $b.total_duration_ms, 1) } else { 0.0 }
  Write-Host ("Total duration: {0:N2}s -> {1:N2}s ({2:+#0.00;-#0.00;0.00}s, {3:+#0.0;-#0.0;0.0}%)" -f ($b.total_duration_ms / 1000.0), ($h.total_duration_ms / 1000.0), ($deltaMs / 1000.0), $pct)
}

$bMap = @{}
foreach ($r in $b.results) { $bMap["$($r.Suite)::$($r.Test)"] = $r }
$hMap = @{}
foreach ($r in $h.results) { $hMap["$($r.Suite)::$($r.Test)"] = $r }

$changes = @()
$timingDeltas = @()
foreach ($k in $hMap.Keys) {
  $hr = $hMap[$k]
  $br = $bMap[$k]
  if (-not $br) {
    $changes += "NEW     $k -> $($hr.Verdict)"
    continue
  }
  if ($br.Verdict -ne $hr.Verdict) {
    $changes += "VERDICT $k : $($br.Verdict) -> $($hr.Verdict)"
  }
  if ($null -ne $br.DurationMs -and $null -ne $hr.DurationMs) {
    $d = [int64]$hr.DurationMs - [int64]$br.DurationMs
    $timingDeltas += [pscustomobject]@{
      Name = $k
      BaseMs = [int64]$br.DurationMs
      HeadMs = [int64]$hr.DurationMs
      DeltaMs = $d
    }
  }
}

if ($changes.Count -gt 0) {
  Write-Host "Verdict changes:" -ForegroundColor Yellow
  $changes | ForEach-Object { Write-Host "  $_" }
} else {
  Write-Host "Verdict changes: none"
}

$speedups = @($timingDeltas | Where-Object DeltaMs -lt 0 | Sort-Object DeltaMs | Select-Object -First $Top)
$slowdowns = @($timingDeltas | Where-Object DeltaMs -gt 0 | Sort-Object DeltaMs -Descending | Select-Object -First $Top)
if ($speedups.Count -gt 0) {
  Write-Host "Biggest speedups:" -ForegroundColor Green
  foreach ($s in $speedups) {
    Write-Host ("  {0,-48} {1,6:N2}s -> {2,6:N2}s ({3:+0.00;-0.00;0.00}s)" -f $s.Name, ($s.BaseMs / 1000.0), ($s.HeadMs / 1000.0), ($s.DeltaMs / 1000.0))
  }
}
if ($slowdowns.Count -gt 0) {
  Write-Host "Biggest slowdowns:" -ForegroundColor Red
  foreach ($s in $slowdowns) {
    Write-Host ("  {0,-48} {1,6:N2}s -> {2,6:N2}s ({3:+0.00;-0.00;0.00}s)" -f $s.Name, ($s.BaseMs / 1000.0), ($s.HeadMs / 1000.0), ($s.DeltaMs / 1000.0))
  }
}

if ($null -ne $b.perf -and $null -ne $h.perf -and $b.perf.spans -and $h.perf.spans) {
  Write-Host "Perf span p99 deltas (ms):" -ForegroundColor Cyan
  $bp = @{}
  foreach ($sp in $b.perf.spans) { $bp[$sp.name] = $sp }
  foreach ($hp in $h.perf.spans) {
    $bsp = $bp[$hp.name]
    if (-not $bsp) { continue }
    $dp99 = ($hp.p99_us - $bsp.p99_us) / 1000.0
    if ([Math]::Abs($dp99) -ge 0.2) {
      Write-Host ("  {0,-22} p99 {1,6:N2}ms -> {2,6:N2}ms ({3:+0.00;-0.00;0.00}ms)" -f $hp.name, ($bsp.p99_us / 1000.0), ($hp.p99_us / 1000.0), $dp99)
    }
  }
}

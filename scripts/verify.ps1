<#
.SYNOPSIS
  One command to build and test Planor the way agents should: safe daemon stop/restart, every live test run on
  its own, failures re-run to separate flakes from real failures, one results table with durations, evidence kept.

.EXAMPLE
  .\scripts\verify.ps1                       # unit tests + every live test
  .\scripts\verify.ps1 -Tier unit            # fast, does not touch your daemon
  .\scripts\verify.ps1 -Only scn_display     # one live test file (or part of a test name)
  .\scripts\verify.ps1 -Record               # PLANOR_RECORD=1: screen-record and analyze (wired tests only)
  .\scripts\verify.ps1 -Retries 3 -UpdateDocs

  Results: build\verify\<timestamp>\results.md + results.json + one log per run.
  build\verify\latest.md and build\verify\latest.json are copies of the latest run.
  Verdicts: PASS, FLAKY (failed, then passed on a re-run), FAIL (failed every run).
#>
param(
  [ValidateSet("unit", "live", "all")][string]$Tier = "all",
  [string]$Only = "",
  [int]$Retries = 2,
  [switch]$Record,
  [switch]$UpdateDocs
)
$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$runSw = [System.Diagnostics.Stopwatch]::StartNew()
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$out = Join-Path $root "build\verify\$stamp"
New-Item -ItemType Directory -Force $out | Out-Null
$release = Join-Path $root "target\release\planor.exe"
$gitCommit = (git rev-parse --short HEAD 2>$null)
if (-not $gitCommit) { $gitCommit = "unknown" }

function Invoke-Cargo([string]$cargoArgs, [string]$log) {
  # cmd keeps cargo's stderr progress lines from turning into PowerShell errors
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  cmd /c "cargo $cargoArgs > `"$log`" 2>&1"
  $code = $LASTEXITCODE
  $sw.Stop()
  return [pscustomobject]@{
    Code = $code
    DurationMs = [int64][Math]::Round($sw.Elapsed.TotalMilliseconds)
  }
}
function Get-FailureLine([string]$log) {
  $t = Get-Content $log -Raw -ErrorAction SilentlyContinue
  if (-not $t) { return "" }
  $m = [regex]::Matches($t, "panicked at [^\r\n]+\r?\n([^\r\n]+)")
  if ($m.Count -gt 0) { return (($m | ForEach-Object { $_.Groups[1].Value.Trim() }) -join " || ") }
  $e = [regex]::Match($t, "(?m)^error[^\r\n]*").Value
  return $e
}
function Format-Dur([int64]$ms) {
  return ("{0:N2}s" -f ($ms / 1000.0))
}

# Was the user's real daemon running? Remember, stop it, always restore it.
$wasRunning = [bool](Get-Process planor -ErrorAction SilentlyContinue)
$results = @()
try {
  if ($Tier -ne "live") {
    Write-Host "== unit tests" -ForegroundColor Cyan
    foreach ($spec in @("--workspace --lib --bins", "--test core", "-p planor-rec")) {
      $log = Join-Path $out ("unit-" + ($spec -replace "[^a-z0-9]+", "_") + ".log")
      $res = Invoke-Cargo "test $spec" $log
      $verdict = if ($res.Code -eq 0) { "PASS" } else { "FAIL" }
      $detail = if ($res.Code -ne 0) { Get-FailureLine $log } else { "" }
      Write-Host ("{0,-6} unit :: {1} ({2})" -f $verdict, $spec, (Format-Dur $res.DurationMs))
      $results += [pscustomobject]@{
        Suite = "unit"
        Test = $spec
        Verdict = $verdict
        Runs = 1
        DurationMs = $res.DurationMs
        RunDurationsMs = @($res.DurationMs)
        Detail = $detail
        Log = $log
      }
    }
  }
  if ($Tier -ne "unit") {
    if ($wasRunning) { Write-Host "stopping your running planor.exe (restored at the end)"; & $release stop | Out-Null; Start-Sleep 2 }
    $env:PLANOR_RECORD = $(if ($Record) { "1" } else { "0" })
    $env:PLANOR_KEEP_ARTIFACTS = $out   # honoured by tests/common/helpers.rs: traces and reports of failed runs are copied here
    Write-Host "== building test binaries" -ForegroundColor Cyan
    $blog = Join-Path $out "build.log"
    $buildRes = Invoke-Cargo "test --no-run" $blog
    if ($buildRes.Code -ne 0) {
      $results += [pscustomobject]@{
        Suite = "build"
        Test = "cargo test --no-run"
        Verdict = "FAIL"
        Runs = 1
        DurationMs = $buildRes.DurationMs
        RunDurationsMs = @($buildRes.DurationMs)
        Detail = (Get-FailureLine $blog)
        Log = $blog
      }
      throw "build failed"
    }
    $files = Get-ChildItem tests -Filter "*.rs" | Where-Object { $_.Name -ne "core.rs" } | ForEach-Object { $_.BaseName }
    foreach ($f in $files) {
      $list = cmd /c "cargo test --test $f -- --ignored --list 2>NUL" | Where-Object { $_ -match ": test$" } | ForEach-Object { ($_ -split ":")[0] }
      foreach ($name in $list) {
        if ($Only -and ("$f $name" -notlike "*$Only*")) { continue }
        $runs = 0; $passes = 0; $detail = ""; $lastlog = ""; $runDurations = @(); [int64]$totalMs = 0
        for ($i = 1; $i -le (1 + $Retries); $i++) {
          $runs++
          $log = Join-Path $out ("$f-$name-run$i.log")
          $res = Invoke-Cargo "test --test $f $name -- --ignored --exact --test-threads=1 --nocapture" $log
          $runDurations += $res.DurationMs
          $totalMs += $res.DurationMs
          $lastlog = $log
          if ($res.Code -eq 0) { $passes++; break } else { $detail = Get-FailureLine $log }
        }
        $verdict = if ($passes -gt 0 -and $runs -eq 1) { "PASS" } elseif ($passes -gt 0) { "FLAKY" } else { "FAIL" }
        Write-Host ("{0,-6} {1} :: {2} ({3} run(s), {4})" -f $verdict, $f, $name, $runs, (Format-Dur $totalMs))
        $results += [pscustomobject]@{
          Suite = $f
          Test = $name
          Verdict = $verdict
          Runs = $runs
          DurationMs = $totalMs
          RunDurationsMs = $runDurations
          Detail = $(if ($verdict -ne "PASS") { $detail } else { "" })
          Log = $lastlog
        }
      }
    }
  }
}
finally {
  if ($wasRunning -and -not (Get-Process planor -ErrorAction SilentlyContinue)) {
    Start-Process -FilePath $release -ArgumentList "start" -WindowStyle Hidden
    Start-Sleep 2
    Write-Host "restarted your planor.exe: pid $((Get-Process planor -ErrorAction SilentlyContinue | Select-Object -First 1).Id)"
  }
  Remove-Item Env:PLANOR_RECORD, Env:PLANOR_KEEP_ARTIFACTS -ErrorAction SilentlyContinue
}
$runSw.Stop()
$totalRunMs = [int64][Math]::Round($runSw.Elapsed.TotalMilliseconds)

$pass = ($results | Where-Object Verdict -eq "PASS").Count
$flaky = ($results | Where-Object Verdict -eq "FLAKY").Count
$fail = ($results | Where-Object Verdict -eq "FAIL").Count
$slowest10 = @($results | Sort-Object DurationMs -Descending | Select-Object -First 10)

$md = @("# Planor test results", "", "Run: $stamp  ·  tier: $Tier  ·  retries: $Retries  ·  recording: $([bool]$Record)  ·  total: $(Format-Dur $totalRunMs)  ·  commit: $gitCommit", "",
  "**$pass pass · $flaky flaky · $fail fail**", "", "| Verdict | Suite | Test | Runs | Duration | Detail |", "|---|---|---|---|---|---|")
foreach ($r in ($results | Sort-Object @{e = { @{FAIL = 0; FLAKY = 1; PASS = 2 }[$_.Verdict] } }, Suite, Test)) {
  $md += "| $($r.Verdict) | $($r.Suite) | $($r.Test) | $($r.Runs) | $(Format-Dur $r.DurationMs) | $(($r.Detail -replace '\|', '/'))" + " |"
}
if ($slowest10.Count -gt 0) {
  $md += "", "## 10 slowest tests", "", "| Rank | Suite | Test | Verdict | Runs | Duration |", "|---|---|---|---|---|---|"
  $rank = 0
  foreach ($s in $slowest10) {
    $rank++
    $md += "| $rank | $($s.Suite) | $($s.Test) | $($s.Verdict) | $($s.Runs) | $(Format-Dur $s.DurationMs) |"
  }
}
$md += "", "Logs: ``$out``"
$md -join "`n" | Set-Content (Join-Path $out "results.md") -Encoding utf8

$jsonDoc = [pscustomobject]@{
  schema_version = 1
  run_id = $stamp
  tier = $Tier
  only = $Only
  retries = $Retries
  recording = [bool]$Record
  git_commit = $gitCommit
  total_duration_ms = $totalRunMs
  summary = [pscustomobject]@{
    pass = $pass
    flaky = $flaky
    fail = $fail
    cached = 0
    total = $results.Count
  }
  slowest_top10 = @($slowest10 | ForEach-Object {
    [pscustomobject]@{
      Suite = $_.Suite
      Test = $_.Test
      Verdict = $_.Verdict
      Runs = $_.Runs
      DurationMs = $_.DurationMs
    }
  })
  perf = $null
  results = @($results)
}
$jsonDoc | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $out "results.json") -Encoding utf8
Copy-Item (Join-Path $out "results.md") (Join-Path $root "build\verify\latest.md") -Force
Copy-Item (Join-Path $out "results.json") (Join-Path $root "build\verify\latest.json") -Force
if ($UpdateDocs) { Copy-Item (Join-Path $out "results.md") (Join-Path $root "docs\TEST_RESULTS.md") -Force }

Write-Host ""
Write-Host "== 10 slowest tests" -ForegroundColor Cyan
$rank = 0
foreach ($s in $slowest10) {
  $rank++
  Write-Host ("  {0,2}. {1,7}  {2} :: {3} ({4}, {5} run(s))" -f $rank, (Format-Dur $s.DurationMs), $s.Suite, $s.Test, $s.Verdict, $s.Runs)
}
Write-Host ""
Write-Host "$pass pass, $flaky flaky, $fail fail in $(Format-Dur $totalRunMs). Report: $out\results.md" -ForegroundColor $(if ($fail -eq 0) { "Green" } else { "Red" })
exit $(if ($fail -eq 0) { 0 } else { 1 })

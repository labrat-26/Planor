<#
.SYNOPSIS
  One command to build and test Planor the way agents should: safe daemon stop/restart, every live test run on
  its own, failures re-run to separate flakes from real failures, one results table, evidence kept.

.EXAMPLE
  .\scripts\verify.ps1                       # unit tests + every live test
  .\scripts\verify.ps1 -Tier unit            # fast, does not touch your daemon
  .\scripts\verify.ps1 -Only scn_display     # one live test file (or part of a test name)
  .\scripts\verify.ps1 -Record               # PLANOR_RECORD=1: record screens and analyze (wired tests only)
  .\scripts\verify.ps1 -Retries 3 -UpdateDocs

  Results: build\verify\<timestamp>\results.md + results.json + one log per run. build\verify\latest.md is a copy.
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
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$out = Join-Path $root "build\verify\$stamp"
New-Item -ItemType Directory -Force $out | Out-Null
$release = Join-Path $root "target\release\planor.exe"

function Invoke-Cargo([string]$cargoArgs, [string]$log) {
  # cmd keeps cargo's stderr progress lines from turning into PowerShell errors
  cmd /c "cargo $cargoArgs > `"$log`" 2>&1"
  return $LASTEXITCODE
}
function Get-FailureLine([string]$log) {
  $t = Get-Content $log -Raw -ErrorAction SilentlyContinue
  if (-not $t) { return "" }
  $m = [regex]::Matches($t, "panicked at [^\r\n]+\r?\n([^\r\n]+)")
  if ($m.Count -gt 0) { return (($m | ForEach-Object { $_.Groups[1].Value.Trim() }) -join " || ") }
  $e = [regex]::Match($t, "(?m)^error[^\r\n]*").Value
  return $e
}

# Was the user's real daemon running? Remember, stop it, always restore it.
$wasRunning = [bool](Get-Process planor -ErrorAction SilentlyContinue)
$results = @()
try {
  if ($Tier -ne "live") {
    Write-Host "== unit tests" -ForegroundColor Cyan
    foreach ($spec in @("--workspace --lib --bins", "--test core", "-p planor-rec")) {
      $log = Join-Path $out ("unit-" + ($spec -replace "[^a-z0-9]+", "_") + ".log")
      $code = Invoke-Cargo "test $spec" $log
      $results += [pscustomobject]@{ Suite = "unit"; Test = $spec; Verdict = $(if ($code -eq 0) { "PASS" } else { "FAIL" }); Runs = 1; Detail = $(if ($code -ne 0) { Get-FailureLine $log }); Log = $log }
    }
  }
  if ($Tier -ne "unit") {
    if ($wasRunning) { Write-Host "stopping your running planor.exe (restored at the end)"; & $release stop | Out-Null; Start-Sleep 2 }
    $env:PLANOR_RECORD = $(if ($Record) { "1" } else { "0" })
    $env:PLANOR_KEEP_ARTIFACTS = $out   # honoured by tests/common/helpers.rs: traces and reports of failed runs are copied here
    Write-Host "== building test binaries" -ForegroundColor Cyan
    $blog = Join-Path $out "build.log"
    if ((Invoke-Cargo "test --no-run" $blog) -ne 0) {
      $results += [pscustomobject]@{ Suite = "build"; Test = "cargo test --no-run"; Verdict = "FAIL"; Runs = 1; Detail = (Get-FailureLine $blog); Log = $blog }
      throw "build failed"
    }
    $files = Get-ChildItem tests -Filter "*.rs" | Where-Object { $_.Name -ne "core.rs" } | ForEach-Object { $_.BaseName }
    foreach ($f in $files) {
      $list = cmd /c "cargo test --test $f -- --ignored --list 2>NUL" | Where-Object { $_ -match ": test$" } | ForEach-Object { ($_ -split ":")[0] }
      foreach ($name in $list) {
        if ($Only -and ("$f $name" -notlike "*$Only*")) { continue }
        $runs = 0; $passes = 0; $detail = ""; $lastlog = ""
        for ($i = 1; $i -le (1 + $Retries); $i++) {
          $runs++
          $log = Join-Path $out ("$f-$name-run$i.log")
          $code = Invoke-Cargo "test --test $f $name -- --ignored --exact --test-threads=1 --nocapture" $log
          $lastlog = $log
          if ($code -eq 0) { $passes++; break } else { $detail = Get-FailureLine $log }
        }
        $verdict = if ($passes -gt 0 -and $runs -eq 1) { "PASS" } elseif ($passes -gt 0) { "FLAKY" } else { "FAIL" }
        Write-Host ("{0,-6} {1} :: {2} ({3} run(s))" -f $verdict, $f, $name, $runs)
        $results += [pscustomobject]@{ Suite = $f; Test = $name; Verdict = $verdict; Runs = $runs; Detail = $(if ($verdict -ne "PASS") { $detail }); Log = $lastlog }
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

$pass = ($results | Where-Object Verdict -eq "PASS").Count
$flaky = ($results | Where-Object Verdict -eq "FLAKY").Count
$fail = ($results | Where-Object Verdict -eq "FAIL").Count
$md = @("# Planor test results", "", "Run: $stamp  ·  tier: $Tier  ·  retries: $Retries  ·  recording: $([bool]$Record)", "",
  "**$pass pass · $flaky flaky · $fail fail**", "", "| Verdict | Suite | Test | Runs | Detail |", "|---|---|---|---|---|")
foreach ($r in ($results | Sort-Object @{e = { @{FAIL = 0; FLAKY = 1; PASS = 2 }[$_.Verdict] } }, Suite, Test)) {
  $md += "| $($r.Verdict) | $($r.Suite) | $($r.Test) | $($r.Runs) | $(($r.Detail -replace '\|', '/'))" + " |"
}
$md += "", "Logs: ``$out``"
$md -join "`n" | Set-Content (Join-Path $out "results.md") -Encoding utf8
$results | ConvertTo-Json -Depth 3 | Set-Content (Join-Path $out "results.json") -Encoding utf8
Copy-Item (Join-Path $out "results.md") (Join-Path $root "build\verify\latest.md") -Force
if ($UpdateDocs) { Copy-Item (Join-Path $out "results.md") (Join-Path $root "docs\TEST_RESULTS.md") -Force }
Write-Host ""
Write-Host "$pass pass, $flaky flaky, $fail fail. Report: $out\results.md" -ForegroundColor $(if ($fail -eq 0) { "Green" } else { "Red" })
exit $(if ($fail -eq 0) { 0 } else { 1 })

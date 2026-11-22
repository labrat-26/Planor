<#
.SYNOPSIS
  One command to build and test Planor: progressive tiers, direct test-exe execution, condition-based
  waits, daemon coexistence, caching, change-based selection, cheap retries, perf budgets, and digests.

.EXAMPLE
  .\scripts\verify.ps1 -Tier unit
  .\scripts\verify.ps1 -Tier fast -Perf
  .\scripts\verify.ps1 -Tier smoke -NoDisplaySwitch -Perf
  .\scripts\verify.ps1 -Changed -Tier unit
  .\scripts\verify.ps1 -Tier live -Only state_json_matches_real_windows -Perf
#>
param(
  [ValidateSet("unit", "fast", "smoke", "live", "all")][string]$Tier = "all",
  [string]$Only = "",
  [int]$Retries = 2,
  [switch]$Record,
  [switch]$NoRecordOnRetry,
  [switch]$NoDisplaySwitch,
  [switch]$KeepAnim,
  [switch]$Changed,
  [switch]$Cache,
  [switch]$Perf,
  [switch]$Parallel,
  [int]$MaxJobs = 0,
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
$recBin = Join-Path $root "tools\planor-rec-bin\planor-rec.exe"
$hasRecBin = Test-Path $recBin
$cachePath = Join-Path $root "build\verify\cache.json"
$gitCommit = (git rev-parse --short HEAD 2>$null)
if (-not $gitCommit) { $gitCommit = "unknown" }

function Format-Dur([int64]$ms) {
  return ("{0:N2}s" -f ($ms / 1000.0))
}

function Invoke-ProcessCaptured([string]$exe, [string]$argLine, [string]$log) {
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  cmd /d /c "call `"$exe`" $argLine > `"$log`" 2>&1"
  $code = $LASTEXITCODE
  $sw.Stop()
  return [pscustomobject]@{
    Code = $code
    DurationMs = [int64][Math]::Round($sw.Elapsed.TotalMilliseconds)
  }
}

function Start-CapturedProcessBg([string]$exe, [string]$argLine, [string]$log, [hashtable]$envMap) {
  $psi = [System.Diagnostics.ProcessStartInfo]::new()
  $psi.FileName = "cmd.exe"
  $psi.Arguments = "/d /c call `"$exe`" $argLine > `"$log`" 2>&1"
  $psi.UseShellExecute = $false
  $psi.CreateNoWindow = $true
  $psi.WorkingDirectory = $root
  if ($envMap) {
    foreach ($k in $envMap.Keys) {
      if ($null -eq $envMap[$k]) {
        [void]$psi.EnvironmentVariables.Remove($k)
      } else {
        $psi.EnvironmentVariables[$k] = [string]$envMap[$k]
      }
    }
  }
  $proc = [System.Diagnostics.Process]::Start($psi)
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  return [pscustomobject]@{
    Process = $proc
    Stopwatch = $sw
    Log = $log
  }
}

function Invoke-ParallelStepJobs([array]$jobs, [int]$concurrency) {
  $queue = [System.Collections.Generic.Queue[object]]::new()
  foreach ($j in $jobs) {
    $queue.Enqueue([pscustomobject]@{
      Tag = $j.Tag
      Steps = @($j.Steps)
      StepIdx = 0
      Log = $j.Log
      EnvMap = $j.EnvMap
      TotalMs = [int64]0
      AllOk = $true
      Active = $null
      ActiveSubLog = ""
    })
  }
  $running = [System.Collections.Generic.List[object]]::new()
  $done = [System.Collections.Generic.List[object]]::new()

  while (($queue.Count -gt 0) -or ($running.Count -gt 0)) {
    while (($running.Count -lt $concurrency) -and ($queue.Count -gt 0)) {
      $item = $queue.Dequeue()
      if ($item.Steps.Count -eq 0) {
        $done.Add($item)
        continue
      }
      $step = $item.Steps[$item.StepIdx]
      $subLog = if ($item.StepIdx -eq 0) { $item.Log } else { "$($item.Log).part$($item.StepIdx)" }
      $item.ActiveSubLog = $subLog
      $item.Active = Start-CapturedProcessBg $step.Exe $step.Args $subLog $item.EnvMap
      $running.Add($item)
    }
    if ($running.Count -gt 0) {
      Start-Sleep -Milliseconds 30
      for ($idx = $running.Count - 1; $idx -ge 0; $idx--) {
        $item = $running[$idx]
        $proc = $item.Active.Process
        if (-not $proc.HasExited) { continue }
        $item.Active.Stopwatch.Stop()
        $item.TotalMs += [int64][Math]::Round($item.Active.Stopwatch.Elapsed.TotalMilliseconds)
        $code = $proc.ExitCode
        $proc.Dispose()
        if ($item.StepIdx -gt 0 -and (Test-Path $item.ActiveSubLog)) {
          Get-Content $item.ActiveSubLog | Add-Content $item.Log
          Remove-Item $item.ActiveSubLog -Force -ErrorAction SilentlyContinue
        }
        $running.RemoveAt($idx)
        if ($code -ne 0) {
          $item.AllOk = $false
          $done.Add($item)
        } elseif (($item.StepIdx + 1) -lt $item.Steps.Count) {
          $item.StepIdx++
          $step = $item.Steps[$item.StepIdx]
          $subLog = "$($item.Log).part$($item.StepIdx)"
          $item.ActiveSubLog = $subLog
          $item.Active = Start-CapturedProcessBg $step.Exe $step.Args $subLog $item.EnvMap
          $running.Add($item)
        } else {
          $done.Add($item)
        }
      }
    }
  }
  return @($done)
}

function Test-OnlyMatch([string]$onlyFilter, [string]$suite, [string]$name, [string]$fq) {
  if (-not $onlyFilter) { return $true }
  foreach ($tok in @($onlyFilter -split "," | ForEach-Object { $_.Trim() } | Where-Object { $_ })) {
    if ("$suite $name $fq" -like "*$tok*") { return $true }
  }
  return $false
}

function Get-FailureLine([string]$log) {
  $t = Get-Content $log -Raw -ErrorAction SilentlyContinue
  if (-not $t) { return "" }
  $m = [regex]::Matches($t, "panicked at [^\r\n]+\r?\n([^\r\n]+)")
  if ($m.Count -gt 0) { return (($m | ForEach-Object { $_.Groups[1].Value.Trim() }) -join " || ") }
  $e = [regex]::Match($t, "(?m)^error[^\r\n]*").Value
  return $e
}

function Get-FileSha([string]$path) {
  if ($path -and (Test-Path $path)) {
    return (Get-FileHash -Algorithm SHA256 -Path $path).Hash
  }
  return "none"
}

function Load-CacheMap([string]$path) {
  $map = @{}
  if (Test-Path $path) {
    try {
      $raw = Get-Content -Raw $path -Encoding UTF8 | ConvertFrom-Json
      if ($raw -and $raw.entries) {
        foreach ($prop in $raw.entries.PSObject.Properties) {
          $map[$prop.Name] = $prop.Value
        }
      }
    } catch {}
  }
  return $map
}

function Save-CacheMap([string]$path, [hashtable]$map) {
  $dir = Split-Path -Parent $path
  if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force $dir | Out-Null }
  $doc = [pscustomobject]@{
    schema_version = 1
    updated = (Get-Date).ToString("o")
    entries = $map
  }
  $doc | ConvertTo-Json -Depth 5 | Set-Content $path -Encoding UTF8
}

function Safe-ArtifactName([string]$suite, [string]$name) {
  $raw = "$suite-$name"
  return ($raw -replace "[^a-zA-Z0-9_-]", "_")
}

function Ensure-RealDaemonStopped([ref]$pausedByUs, [string]$releaseExe) {
  if ((Get-Process planor -ErrorAction SilentlyContinue) -and (Test-Path $releaseExe)) {
    Write-Host "  temporarily stopping running planor.exe for exclusive hotkey/exit test" -ForegroundColor Yellow
    & $releaseExe stop | Out-Null
    Start-Sleep -Milliseconds 1500
    $pausedByUs.Value = $true
  }
}

# Build once and collect all compiled test executables via --message-format=json.
Write-Host "== building workspace test executables" -ForegroundColor Cyan
$buildJsonLog = Join-Path $out "build-json.log"
$buildSw = [System.Diagnostics.Stopwatch]::StartNew()
cmd /d /c "cargo test --workspace --no-run --message-format=json > `"$buildJsonLog`" 2>&1"
$buildCode = $LASTEXITCODE
$buildSw.Stop()
if ($buildCode -ne 0) {
  $blog = Join-Path $out "build.log"
  Get-Content $buildJsonLog -ErrorAction SilentlyContinue | ForEach-Object {
    try {
      $o = $_ | ConvertFrom-Json
      if ($o.reason -eq "compiler-message" -and $o.message.rendered) { $o.message.rendered }
    } catch { $_ }
  } | Set-Content $blog -Encoding UTF8
  Write-Host "FAIL   build :: cargo test --no-run ($(Format-Dur ([int64]$buildSw.Elapsed.TotalMilliseconds)))" -ForegroundColor Red
  & (Join-Path $PSScriptRoot "digest.ps1") -Log $blog
  exit 1
}

$testExes = @{}
$unitBinExes = @{}
$debugDaemonExe = Join-Path $root "target\debug\planor.exe"
foreach ($line in (Get-Content $buildJsonLog -ErrorAction SilentlyContinue)) {
  if (-not $line.StartsWith("{")) { continue }
  try {
    $o = $line | ConvertFrom-Json
    if ($o.reason -ne "compiler-artifact") { continue }
    $exe = $o.executable
    if (-not $exe) { continue }
    $tname = $o.target.name
    $kinds = @($o.target.kind)
    if ($o.profile.test -eq $true) {
      if ($kinds -contains "test") {
        $testExes[$tname] = $exe
      } elseif ($kinds -contains "bin") {
        $unitBinExes["bin:$tname"] = $exe
      } elseif ($kinds -contains "lib") {
        $unitBinExes["lib:$tname"] = $exe
      }
    } elseif (($kinds -contains "bin") -and ($tname -eq "planor")) {
      $debugDaemonExe = $exe
    }
  } catch {}
}

$daemonSha = Get-FileSha $debugDaemonExe
$cfgSha = Get-FileSha (Join-Path $root "planor.example.toml")
$cacheMap = if ($Cache) { Load-CacheMap $cachePath } else { @{} }

# Tier & coexistence definitions (verified against --ignored --list names)
$fastLiveList = @(
  "live::state_json_matches_real_windows",
  "live::perf_spans_and_budgets",
  "live::stop_restores_exact_rect",
  "scn_focus::focus_walk_right_then_left_n3"
)
$smokeLiveList = @(
  "live::state_json_matches_real_windows",
  "live::perf_spans_and_budgets",
  "scn_focus::focus_walk_right_then_left_n3",
  "scn_scroll::settled_layout_geometry",
  "scn_drag::reorder_before_after",
  "scn_exit::stop_with_many_windows",
  "scn_taskbar::tiles_stay_inside_work_area",
  "scn_overlay::border_never_paints_over_an_unmanaged_window",
  "scn_display::short_blip_3s"
)
$exclusiveLiveSet = @{
  "scn_taskbar::clicking_a_peek_reveals_that_window" = $true
  "scn_exit::panic_hotkey" = $true
  "scn_exit::ctrl_break_restores_before_exit" = $true
}
$parallelSafeLiveSet = @{
  "live::state_json_matches_real_windows" = $true
  "live::stop_restores_exact_rect" = $true
  "live::owned_dialog_excluded" = $true
  "live::bounded_rejection_backoff" = $true
  "live::forced_kill_recovery" = $true
  "scn_exit::stop_with_many_windows" = $true
  "scn_taskbar::tiles_stay_inside_work_area" = $true
}
$unitParallelJobs = if ($MaxJobs -gt 0) { $MaxJobs } else { 4 }
$liveParallelJobs = if ($MaxJobs -gt 0) { $MaxJobs } else { 2 }

# Optional change-based selection (-Changed)
$changedUnitFilter = $null
$changedLiveFilter = $null
if ($Changed) {
  $changedFiles = @()
  $changedFiles += @(git diff --name-only HEAD 2>$null)
  $changedFiles += @(git ls-files --others --exclude-standard 2>$null)
  $changedFiles = @($changedFiles | Where-Object { $_ } | Select-Object -Unique)
  if ($changedFiles.Count -eq 0) {
    $changedFiles = @(git diff --name-only HEAD~1..HEAD 2>$null | Where-Object { $_ } | Select-Object -Unique)
  }
  Write-Host ("-- changed files ({0}): {1}" -f $changedFiles.Count, ($changedFiles -join ", "))
  $mapDoc = Get-Content -Raw (Join-Path $PSScriptRoot "test-map.json") -Encoding UTF8 | ConvertFrom-Json
  $uSet = [System.Collections.Generic.HashSet[string]]::new()
  $lSet = [System.Collections.Generic.HashSet[string]]::new()
  foreach ($f in $changedFiles) {
    $norm = $f -replace "\\", "/"
    if ($norm -match "^tests/([a-z0-9_]+)\.rs$" -and $norm -ne "tests/core.rs" -and $norm -ne "tests/prop_core.rs") {
      [void]$lSet.Add($Matches[1])
      continue
    }
    foreach ($rule in $mapDoc.rules) {
      if ($norm -match $rule.pattern) {
        foreach ($u in $rule.unit) { [void]$uSet.Add($u) }
        foreach ($lv in $rule.live) { [void]$lSet.Add($lv) }
        break
      }
    }
  }
  $changedUnitFilter = @($uSet)
  $changedLiveFilter = @($lSet)
}

$wasRunning = [bool](Get-Process planor -ErrorAction SilentlyContinue)
$realPausedByUs = $false
$results = @()
$perfReports = @()
$cachedCount = 0

try {
  if ($Perf) { $env:PLANOR_PERF = "1" } else { Remove-Item Env:PLANOR_PERF -ErrorAction SilentlyContinue }
  if ($KeepAnim) { $env:PLANOR_KEEP_ANIM = "1" } else { Remove-Item Env:PLANOR_KEEP_ANIM -ErrorAction SilentlyContinue }

  if ($Tier -ne "live") {
    Write-Host ("== unit tests{0}" -f $(if ($Parallel) { " (parallel max=$unitParallelJobs)" } else { "" })) -ForegroundColor Cyan
    $unitSpecs = @(
      @{
        Name = "--lib planor"
        Steps = @(@(
          @{ Exe = $unitBinExes["lib:planor"]; Args = "" }
        ) | Where-Object { $_.Exe })
      },
      @{
        Name = "--bin planor (sim_tests)"
        Steps = @(@(
          @{ Exe = $unitBinExes["bin:planor"]; Args = "sim_tests" }
        ) | Where-Object { $_.Exe })
      },
      @{
        Name = "--bin planor (other)"
        Steps = @(@(
          @{ Exe = $unitBinExes["bin:planor"]; Args = "--skip sim_tests" }
        ) | Where-Object { $_.Exe })
      },
      @{
        Name = "--test core"
        Steps = @(@(
          @{ Exe = $testExes["core"]; Args = "" }
        ) | Where-Object { $_.Exe })
      },
      @{
        Name = "--test prop_core"
        Steps = @(@(
          @{ Exe = $testExes["prop_core"]; Args = "--nocapture" }
        ) | Where-Object { $_.Exe })
      },
      @{
        Name = "-p planor-rec"
        Steps = @(@(
          @{ Exe = $unitBinExes["lib:planor_rec"]; Args = "" },
          @{ Exe = $unitBinExes["bin:planor-rec"]; Args = "" },
          @{ Exe = $testExes["analyze_synthetic"]; Args = "" }
        ) | Where-Object { $_.Exe })
      }
    )
    if ($null -ne $changedUnitFilter) {
      $unitSpecs = @($unitSpecs | Where-Object { $changedUnitFilter -contains $_.Name })
    }
    if ($Only) {
      $unitSpecs = @($unitSpecs | Where-Object { Test-OnlyMatch $Only "unit" $_.Name ("unit::" + $_.Name) })
    }
    $unitRunQueue = @()
    foreach ($spec in $unitSpecs) {
      $sname = $spec.Name
      $key = "unit::$sname"
      $shaParts = @($daemonSha, $cfgSha, "perf=$Perf") + @($spec.Steps | ForEach-Object { Get-FileSha $_.Exe })
      $hash = ($shaParts -join "|")
      if ($Cache -and $cacheMap.ContainsKey($key) -and $cacheMap[$key].hash -eq $hash -and $cacheMap[$key].verdict -eq "PASS") {
        $cachedCount++
        Write-Host ("CACHED unit :: {0} (0.00s)" -f $sname) -ForegroundColor DarkGray
        $results += [pscustomobject]@{
          Suite = "unit"; Test = $sname; Verdict = "CACHED"; Runs = 0; DurationMs = 0; RunDurationsMs = @(); Detail = ""; Log = ""
        }
        continue
      }
      $log = Join-Path $out ("unit-" + ($sname -replace "[^a-z0-9]+", "_") + ".log")
      $unitRunQueue += [pscustomobject]@{
        Tag = @{ Name = $sname; Key = $key; Hash = $hash }
        Steps = $spec.Steps
        Log = $log
        EnvMap = $null
      }
    }
    if ($Parallel -and $unitRunQueue.Count -gt 1) {
      $finished = Invoke-ParallelStepJobs $unitRunQueue $unitParallelJobs
      foreach ($fin in ($finished | Sort-Object { $_.Tag.Name })) {
        $sname = $fin.Tag.Name; $key = $fin.Tag.Key; $hash = $fin.Tag.Hash
        $verdict = if ($fin.AllOk) { "PASS" } else { "FAIL" }
        $detail = if (-not $fin.AllOk) { Get-FailureLine $fin.Log } else { "" }
        Write-Host ("{0,-6} unit :: {1} ({2})" -f $verdict, $sname, (Format-Dur $fin.TotalMs)) -ForegroundColor $(if ($verdict -eq "PASS") { "Green" } else { "Red" })
        if ($Cache -and $verdict -eq "PASS") {
          $cacheMap[$key] = [pscustomobject]@{ hash = $hash; verdict = "PASS"; stamp = $stamp }
        }
        $results += [pscustomobject]@{
          Suite = "unit"; Test = $sname; Verdict = $verdict; Runs = 1; DurationMs = $fin.TotalMs; RunDurationsMs = @($fin.TotalMs); Detail = $detail; Log = $fin.Log
        }
      }
    } else {
      foreach ($job in $unitRunQueue) {
        $sname = $job.Tag.Name; $key = $job.Tag.Key; $hash = $job.Tag.Hash; $log = $job.Log
        $totalMs = [int64]0
        $allOk = $true
        $firstRun = $true
        foreach ($step in $job.Steps) {
          $subLog = if ($firstRun) { $log } else { "$log.part" }
          $res = Invoke-ProcessCaptured $step.Exe $step.Args $subLog
          $totalMs += $res.DurationMs
          if (-not $firstRun -and (Test-Path $subLog)) {
            Get-Content $subLog | Add-Content $log
            Remove-Item $subLog -Force -ErrorAction SilentlyContinue
          }
          $firstRun = $false
          if ($res.Code -ne 0) { $allOk = $false; break }
        }
        $verdict = if ($allOk) { "PASS" } else { "FAIL" }
        $detail = if (-not $allOk) { Get-FailureLine $log } else { "" }
        Write-Host ("{0,-6} unit :: {1} ({2})" -f $verdict, $sname, (Format-Dur $totalMs)) -ForegroundColor $(if ($verdict -eq "PASS") { "Green" } else { "Red" })
        if ($Cache -and $verdict -eq "PASS") {
          $cacheMap[$key] = [pscustomobject]@{ hash = $hash; verdict = "PASS"; stamp = $stamp }
        }
        $results += [pscustomobject]@{
          Suite = "unit"; Test = $sname; Verdict = $verdict; Runs = 1; DurationMs = $totalMs; RunDurationsMs = @($totalMs); Detail = $detail; Log = $log
        }
      }
    }
  }

  if ($Tier -ne "unit") {
    Write-Host ("== live tests ({0}{1})" -f $Tier, $(if ($Parallel) { ", parallel-safe max=$liveParallelJobs" } else { "" })) -ForegroundColor Cyan
    $env:PLANOR_KEEP_ARTIFACTS = $out
    $liveFiles = @("live", "scn_focus", "scn_scroll", "scn_drag", "scn_exit", "scn_taskbar", "scn_overlay", "scn_display")
    if ($NoDisplaySwitch) {
      $liveFiles = @($liveFiles | Where-Object { $_ -ne "scn_display" })
    }
    $selectedTests = @()
    foreach ($f in $liveFiles) {
      $exe = $testExes[$f]
      if (-not $exe -or -not (Test-Path $exe)) { continue }
      $listed = & $exe --ignored --list 2>$null | Where-Object { $_ -match ": test$" } | ForEach-Object { ($_ -split ":")[0].Trim() }
      foreach ($name in $listed) {
        $fq = "$f::$name"
        if ($Tier -eq "fast" -and ($fastLiveList -notcontains $fq)) { continue }
        if ($Tier -eq "smoke" -and ($smokeLiveList -notcontains $fq)) { continue }
        if ($Only -and -not (Test-OnlyMatch $Only $f $name $fq)) { continue }
        if ($null -ne $changedLiveFilter -and -not ($changedLiveFilter | Where-Object { $fq -eq $_ -or $f -eq $_ })) { continue }
        $selectedTests += [pscustomobject]@{ Suite = $f; Name = $name; Fq = $fq; Exe = $exe }
      }
    }

    # Run parallel-safe subset first (concurrently when -Parallel), then non-exclusive serial tests, then exclusive tests.
    $ordered = @($selectedTests | Where-Object { -not $exclusiveLiveSet.ContainsKey($_.Fq) }) + @($selectedTests | Where-Object { $exclusiveLiveSet.ContainsKey($_.Fq) })
    $serialLiveQueue = @()
    if ($Parallel) {
      $parBatch = @()
      $parQueue = @()
      foreach ($t in $ordered) {
        $f = $t.Suite; $name = $t.Name; $fq = $t.Fq; $exe = $t.Exe
        $key = $fq
        $hash = (@($daemonSha, $cfgSha, (Get-FileSha $exe), "anim=$KeepAnim", "perf=$Perf", "rec=$Record") -join "|")
        if ($Cache -and $cacheMap.ContainsKey($key) -and $cacheMap[$key].hash -eq $hash -and $cacheMap[$key].verdict -eq "PASS") {
          $cachedCount++
          Write-Host ("CACHED {0} :: {1} (0.00s)" -f $f, $name) -ForegroundColor DarkGray
          $results += [pscustomobject]@{
            Suite = $f; Test = $name; Verdict = "CACHED"; Runs = 0; DurationMs = 0; RunDurationsMs = @(); Detail = ""; Log = ""
          }
          continue
        }
        if ($parallelSafeLiveSet.ContainsKey($fq)) {
          $log1 = Join-Path $out ("$f-$name-run1.log")
          $safeName = (Safe-ArtifactName $f $name)
          $envMap = @{
            PLANOR_TEST_NAME = $safeName
            PLANOR_RECORD = $(if ($Record) { "1" } else { "0" })
            PLANOR_KEEP_ARTIFACTS = $out
            PLANOR_PERF = $(if ($Perf) { "1" } else { $null })
            PLANOR_KEEP_ANIM = $(if ($KeepAnim) { "1" } else { $null })
          }
          $parQueue += [pscustomobject]@{
            Tag = @{ Suite = $f; Name = $name; Fq = $fq; Exe = $exe; Hash = $hash; SafeName = $safeName }
            Steps = @(@{ Exe = $exe; Args = "$name --ignored --exact --test-threads=1 --nocapture" })
            Log = $log1
            EnvMap = $envMap
          }
        } else {
          $serialLiveQueue += $t
        }
      }
      if ($parQueue.Count -eq 1) {
        $serialLiveQueue = @([pscustomobject]@{
          Suite = $parQueue[0].Tag.Suite; Name = $parQueue[0].Tag.Name; Fq = $parQueue[0].Tag.Fq; Exe = $parQueue[0].Tag.Exe
        }) + $serialLiveQueue
      } elseif ($parQueue.Count -gt 1) {
        $finishedPar = Invoke-ParallelStepJobs $parQueue $liveParallelJobs
        foreach ($fin in ($finishedPar | Sort-Object { $_.Tag.Fq })) {
          $tag = $fin.Tag
          $rawLog = Get-Content $fin.Log -Raw -ErrorAction SilentlyContinue
          if ($rawLog -match "(?m)^PERF_JSON (\{.+\})\r?$") {
            try { $perfReports += ($Matches[1] | ConvertFrom-Json) } catch {}
          }
          if ($fin.AllOk) {
            Write-Host ("{0,-6} {1} :: {2} (1 run(s), {3})" -f "PASS", $tag.Suite, $tag.Name, (Format-Dur $fin.TotalMs)) -ForegroundColor Green
            if ($Cache) {
              $cacheMap[$tag.Fq] = [pscustomobject]@{ hash = $tag.Hash; verdict = "PASS"; stamp = $stamp }
            }
            $results += [pscustomobject]@{
              Suite = $tag.Suite; Test = $tag.Name; Verdict = "PASS"; Runs = 1; DurationMs = $fin.TotalMs; RunDurationsMs = @($fin.TotalMs); Detail = ""; Log = $fin.Log
            }
          } else {
            # Failed parallel first run: retry serially with recording enabled per cheap-retry rule.
            $parBatch += [pscustomobject]@{
              Suite = $tag.Suite; Name = $tag.Name; Fq = $tag.Fq; Exe = $tag.Exe
              InitialMs = $fin.TotalMs; InitialDetail = (Get-FailureLine $fin.Log); LastLog = $fin.Log
            }
          }
        }
        foreach ($retryItem in $parBatch) {
          $serialLiveQueue = @($retryItem) + $serialLiveQueue
        }
      }
    } else {
      $serialLiveQueue = $ordered
    }

    foreach ($t in $serialLiveQueue) {
      $f = $t.Suite; $name = $t.Name; $fq = $t.Fq; $exe = $t.Exe
      $key = $fq
      $hash = (@($daemonSha, $cfgSha, (Get-FileSha $exe), "anim=$KeepAnim", "perf=$Perf", "rec=$Record") -join "|")
      $hadParallelRun1 = $null -ne $t.InitialMs
      if (-not $hadParallelRun1 -and $Cache -and $cacheMap.ContainsKey($key) -and $cacheMap[$key].hash -eq $hash -and $cacheMap[$key].verdict -eq "PASS") {
        $cachedCount++
        Write-Host ("CACHED {0} :: {1} (0.00s)" -f $f, $name) -ForegroundColor DarkGray
        $results += [pscustomobject]@{
          Suite = $f; Test = $name; Verdict = "CACHED"; Runs = 0; DurationMs = 0; RunDurationsMs = @(); Detail = ""; Log = ""
        }
        continue
      }
      if ($exclusiveLiveSet.ContainsKey($fq)) {
        Ensure-RealDaemonStopped ([ref]$realPausedByUs) $release
      }
      $runs = if ($hadParallelRun1) { 1 } else { 0 }
      $passes = 0
      $detail = if ($hadParallelRun1) { $t.InitialDetail } else { "" }
      $lastlog = if ($hadParallelRun1) { $t.LastLog } else { "" }
      $runDurations = if ($hadParallelRun1) { @($t.InitialMs) } else { @() }
      [int64]$totalMs = if ($hadParallelRun1) { $t.InitialMs } else { 0 }
      $startAttempt = if ($hadParallelRun1) { 2 } else { 1 }
      $env:PLANOR_TEST_NAME = (Safe-ArtifactName $f $name)
      for ($i = $startAttempt; $i -le (1 + $Retries); $i++) {
        $runs++
        $useRec = [bool]$Record -or (($i -gt 1) -and (-not $NoRecordOnRetry) -and $hasRecBin)
        $env:PLANOR_RECORD = if ($useRec) { "1" } else { "0" }
        $log = Join-Path $out ("$f-$name-run$i.log")
        $res = Invoke-ProcessCaptured $exe "$name --ignored --exact --test-threads=1 --nocapture" $log
        $runDurations += $res.DurationMs
        $totalMs += $res.DurationMs
        $lastlog = $log
        $rawLog = Get-Content $log -Raw -ErrorAction SilentlyContinue
        if ($rawLog -match "(?m)^PERF_JSON (\{.+\})\r?$") {
          try { $perfReports += ($Matches[1] | ConvertFrom-Json) } catch {}
        }
        if ($res.Code -eq 0) { $passes++; break } else { $detail = Get-FailureLine $log }
      }
      $verdict = if ($passes -gt 0 -and $runs -eq 1) { "PASS" } elseif ($passes -gt 0) { "FLAKY" } else { "FAIL" }
      $color = if ($verdict -eq "PASS") { "Green" } elseif ($verdict -eq "FLAKY") { "Yellow" } else { "Red" }
      Write-Host ("{0,-6} {1} :: {2} ({3} run(s), {4})" -f $verdict, $f, $name, $runs, (Format-Dur $totalMs)) -ForegroundColor $color
      if ($Cache -and $verdict -eq "PASS") {
        $cacheMap[$key] = [pscustomobject]@{ hash = $hash; verdict = "PASS"; stamp = $stamp }
      }
      if ($verdict -eq "FAIL" -and $lastlog) {
        $failDir = Join-Path $out (Safe-ArtifactName $f $name)
        New-Item -ItemType Directory -Force $failDir | Out-Null
        powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "digest.ps1") -Log $lastlog | Set-Content (Join-Path $failDir "digest.txt") -Encoding UTF8
      }
      $results += [pscustomobject]@{
        Suite = $f; Test = $name; Verdict = $verdict; Runs = $runs; DurationMs = $totalMs; RunDurationsMs = $runDurations; Detail = $(if ($verdict -ne "PASS") { $detail } else { "" }); Log = $lastlog
      }
    }
  }
}
finally {
  if ($realPausedByUs -and $wasRunning -and -not (Get-Process planor -ErrorAction SilentlyContinue) -and (Test-Path $release)) {
    Start-Process -FilePath $release -ArgumentList "start" -WindowStyle Hidden
    Start-Sleep 2
    Write-Host "restarted your planor.exe: pid $((Get-Process planor -ErrorAction SilentlyContinue | Select-Object -First 1).Id)"
  }
  Remove-Item Env:PLANOR_RECORD, Env:PLANOR_KEEP_ARTIFACTS, Env:PLANOR_TEST_NAME, Env:PLANOR_KEEP_ANIM, Env:PLANOR_PERF -ErrorAction SilentlyContinue
}

if ($Cache) { Save-CacheMap $cachePath $cacheMap }

$runSw.Stop()
$totalRunMs = [int64][Math]::Round($runSw.Elapsed.TotalMilliseconds)

$pass = @($results | Where-Object Verdict -eq "PASS").Count
$flaky = @($results | Where-Object Verdict -eq "FLAKY").Count
$fail = @($results | Where-Object Verdict -eq "FAIL").Count
$slowest10 = @($results | Where-Object DurationMs -gt 0 | Sort-Object DurationMs -Descending | Select-Object -First 10)
$perfLatest = if ($perfReports.Count -gt 0) { $perfReports[-1] } else { $null }
if ($perfLatest) {
  $perfJsonStr = $perfLatest | ConvertTo-Json -Depth 8
  $perfJsonStr | Set-Content (Join-Path $out "perf.json") -Encoding UTF8
  $perfJsonStr | Set-Content (Join-Path $root "build\verify\latest-perf.json") -Encoding UTF8
}

$md = @("# Planor test results", "", "Run: $stamp  ·  tier: $Tier  ·  parallel: $([bool]$Parallel)  ·  retries: $Retries  ·  recording: $([bool]$Record)  ·  perf: $([bool]$Perf)  ·  total: $(Format-Dur $totalRunMs)  ·  commit: $gitCommit", "",
  "**$pass pass · $flaky flaky · $fail fail · $cachedCount cached**", "", "| Verdict | Suite | Test | Runs | Duration | Detail |", "|---|---|---|---|---|---|")
$sortOrder = @{ FAIL = 0; FLAKY = 1; PASS = 2; CACHED = 3 }
foreach ($r in ($results | Sort-Object @{ e = { $sortOrder[$_.Verdict] } }, Suite, Test)) {
  $md += "| $($r.Verdict) | $($r.Suite) | $($r.Test) | $($r.Runs) | $(Format-Dur $r.DurationMs) | $(($r.Detail -replace '\|', '/')) |"
}
if ($slowest10.Count -gt 0) {
  $md += "", "## 10 slowest tests", "", "| Rank | Suite | Test | Verdict | Runs | Duration |", "|---|---|---|---|---|---|"
  $rank = 0
  foreach ($s in $slowest10) {
    $rank++
    $md += "| $rank | $($s.Suite) | $($s.Test) | $($s.Verdict) | $($s.Runs) | $(Format-Dur $s.DurationMs) |"
  }
}
if ($perfLatest) {
  $b = $perfLatest.budgets
  $md += "", "## Perf budgets", "",
    ("- startup_first_place_ms: {0} / {1} ms (ok={2})" -f $b.startup_first_place_ms, $b.startup_budget_ms, $b.startup_ok),
    ("- render_scene_warm_p99_ms: {0} / {1} ms (ok={2})" -f $b.render_scene_warm_p99_ms, $b.render_scene_budget_ms, $b.render_scene_ok),
    ("- gesture_to_place_p99_ms: {0} / {1} ms (ok={2})" -f $b.gesture_to_place_p99_ms, $b.gesture_budget_ms, $b.gesture_ok),
    ("- hook_callbacks_max_ms: {0} / {1} ms (ok={2})" -f $b.hook_callbacks_max_ms, $b.hook_budget_ms, $b.hook_ok),
    ("- process: ws={0} MB, handles={1}, gdi={2}, user={3}, stalls={4}" -f $perfLatest.process.working_set_mb, $perfLatest.process.os_handles, $perfLatest.process.gdi_objects, $perfLatest.process.user_objects, $perfLatest.stalls)
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
  perf_enabled = [bool]$Perf
  changed_only = [bool]$Changed
  cache_enabled = [bool]$Cache
  parallel = [bool]$Parallel
  max_jobs = $MaxJobs
  git_commit = $gitCommit
  total_duration_ms = $totalRunMs
  summary = [pscustomobject]@{
    pass = $pass
    flaky = $flaky
    fail = $fail
    cached = $cachedCount
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
  perf = $perfLatest
  results = @($results)
}
$jsonDoc | ConvertTo-Json -Depth 8 | Set-Content (Join-Path $out "results.json") -Encoding utf8
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
if ($perfLatest) {
  Write-Host ("== perf budgets all_pass={0} (ws={1} MB, render_p99={2}ms, startup={3}ms)" -f $perfLatest.budgets.all_pass, $perfLatest.process.working_set_mb, $perfLatest.budgets.render_scene_warm_p99_ms, $perfLatest.budgets.startup_first_place_ms) -ForegroundColor Cyan
}
Write-Host ""
Write-Host "$pass pass, $flaky flaky, $fail fail, $cachedCount cached in $(Format-Dur $totalRunMs). Report: $out\results.md" -ForegroundColor $(if ($fail -eq 0) { "Green" } else { "Red" })
exit $(if ($fail -eq 0) { 0 } else { 1 })

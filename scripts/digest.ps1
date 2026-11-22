<#
.SYNOPSIS
  Shrink Planor test, trace, recording, perf and bundle data into a short digest (<= Max lines).

.EXAMPLE
  .\scripts\digest.ps1                          # latest verify results (build\verify\latest.md)
  .\scripts\digest.ps1 -Rec <recording dir>     # planor-rec report (report.json)
  .\scripts\digest.ps1 -Trace <trace.log>       # daemon trace: latencies, STALL/SLOW lines, notes
  .\scripts\digest.ps1 -Log <cargo test log>    # one test log: failing assertion & context
  .\scripts\digest.ps1 -Perf [perf.json]        # perf report budgets, OS counters & top spans
  .\scripts\digest.ps1 -Bundle <bundle dir>     # Win+W note bundle or watchdog stall bundle
#>
param(
  [string]$Rec = "",
  [string]$Trace = "",
  [string]$Log = "",
  [switch]$Perf,
  [string]$Bundle = "",
  [int]$Max = 60,
  [Parameter(ValueFromRemainingArguments = $true)][string[]]$Rest
)
$root = Split-Path -Parent $PSScriptRoot
$lines = New-Object System.Collections.Generic.List[string]
function Add-Line([string]$s) { $lines.Add($s) }
function Short([string]$s, [int]$n = 170) { if ($s.Length -gt $n) { $s.Substring(0, $n) + "..." } else { $s } }
function Collapse([string[]]$items) {
  $items | Group-Object { ($_ -replace "\b\d{5,}\b", "#" -replace "\d+(\.\d+)? ?ms", "N ms") } | Sort-Object Count -Descending |
    ForEach-Object { if ($_.Count -gt 1) { "{0}  x{1}" -f (Short $_.Group[0]), $_.Count } else { Short $_.Group[0] } }
}

if ($Perf) {
  # `-Perf <file>` binds the file to the first positional parameter (-Rec) when run with -File.
  if (-not $Rest -and $Rec) { $Rest = @($Rec); $Rec = "" }
  $perfPath = if ($Rest -and $Rest.Count -gt 0 -and $Rest[0]) { $Rest[0] } else { Join-Path $root "build\verify\latest-perf.json" }
  if (-not (Test-Path $perfPath)) {
    $cand = Get-ChildItem (Join-Path $root "build\verify") -Filter "perf.json" -Recurse -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($cand) { $perfPath = $cand.FullName }
  }
  if (-not (Test-Path $perfPath)) { throw "no perf.json found (pass -Perf <path> or run .\scripts\verify.ps1 -Tier fast -Perf)" }
  $p = Get-Content -Raw $perfPath -Encoding UTF8 | ConvertFrom-Json
  $Max = [Math]::Min($Max, 20)
  Add-Line ("perf: $perfPath (pid=$($p.process.pid) uptime=$([Math]::Round($p.uptime_ms / 1000.0, 1))s stalls=$($p.stalls))")
  Add-Line ("os: cpu={0:N1}% ws={1:N1}MB peak={2:N1}MB priv={3:N1}MB gdi={4} user={5} handles={6}" -f $p.process.cpu_avg_pct, $p.process.working_set_mb, $p.process.peak_working_set_mb, $p.process.private_mb, $p.process.gdi_objects, $p.process.user_objects, $p.process.os_handles)
  $g = $p.process.gpu
  if ($g) {
    $gd = @($p.spans | Where-Object { $_.name -eq "gpu.draw" }) | Select-Object -First 1
    $gdText = if ($gd) { "gpu.draw n={0} p50={1:N3}ms p99={2:N3}ms" -f $gd.count, ($gd.p50_us / 1000.0), ($gd.p99_us / 1000.0) } else { "gpu.draw n=0" }
    $r = { param($v, $d) if ($null -eq $v) { "n/a" } else { [Math]::Round([double]$v, $d) } }
    Add-Line ("gpu: engine={0}% vram_local={1}MB shared={2}MB dwm={3}fpsHz late={5} dropped={6} missed={7} {8}" -f (& $r $g.engine_pct 1), (& $r $g.local_mb 1), (& $r $g.shared_mb 1), (& $r $g.dwm_compose_fps 0), (& $r $g.dwm_refresh_hz 0), $g.dwm_frames_late, $g.dwm_frames_dropped, $g.dwm_frames_missed, $gdText)
  }
  $b = $p.budgets
  Add-Line ("budgets all_pass=$($b.all_pass): startup=$($b.startup_first_place_ms)/$($b.startup_budget_ms)ms(ok=$($b.startup_ok)) render_p99=$($b.render_scene_warm_p99_ms)/$($b.render_scene_budget_ms)ms(ok=$($b.render_scene_ok)) gesture_p99=$($b.gesture_to_place_p99_ms)/$($b.gesture_budget_ms)ms(ok=$($b.gesture_ok)) hook_max=$($b.hook_callbacks_max_ms)/$($b.hook_budget_ms)ms(ok=$($b.hook_ok))")
  foreach ($s in ($p.spans | Sort-Object p99_us -Descending | Select-Object -First 8)) {
    Add-Line ("span {0,-20} n={1,5} p50={2,5:N2}ms p95={3,5:N2}ms p99={4,6:N2}ms max={5,6:N2}ms slow={6}" -f $s.name, $s.count, ($s.p50_us / 1000.0), ($s.p95_us / 1000.0), ($s.p99_us / 1000.0), ($s.max_us / 1000.0), $s.slow_count)
  }
  foreach ($th in ($p.threads | Select-Object -First 6)) {
    Add-Line ("thread {0,-10} tid={1} spans={2} cpu_user={3:N1}ms cpu_sys={4:N1}ms" -f $th.name, $th.tid, $th.span_count, $th.cpu_user_ms, $th.cpu_kernel_ms)
  }
}
elseif ($Bundle) {
  if (-not (Test-Path $Bundle)) { throw "no bundle directory $Bundle" }
  $Max = [Math]::Min($Max, 20)
  Add-Line "bundle: $Bundle"
  foreach ($metaFile in @("note.txt", "stall.txt")) {
    $mp = Join-Path $Bundle $metaFile
    if (Test-Path $mp) {
      Add-Line ("${metaFile}: " + ((Get-Content $mp -ErrorAction SilentlyContinue) -join " | "))
    }
  }
  $sp = Join-Path $Bundle "state.json"
  if (Test-Path $sp) {
    try {
      $st = Get-Content -Raw $sp -Encoding UTF8 | ConvertFrom-Json
      Add-Line ("state: monitors=$($st.monitors.Count) strips=$($st.strips.Count) tiles=$($st.tiles.Count) parked=$($st.parked.Count) seq=$($st.seq) stalls=$($st.stalls)")
    } catch { Add-Line "state.json: parse error" }
  }
  $pp = Join-Path $Bundle "perf.json"
  if (Test-Path $pp) {
    try {
      $pr = Get-Content -Raw $pp -Encoding UTF8 | ConvertFrom-Json
      Add-Line ("perf: all_pass=$($pr.budgets.all_pass) stalls=$($pr.stalls) ws=$($pr.process.working_set_mb)MB gdi=$($pr.process.gdi_objects) handles=$($pr.process.os_handles)")
    } catch { Add-Line "perf.json: parse error" }
  }
  $hasPng = Test-Path (Join-Path $Bundle "screenshot.png")
  $hasTrace = Test-Path (Join-Path $Bundle "perf.trace.json")
  $vidMp4 = Get-ChildItem (Join-Path $Bundle "video") -Filter "*.mp4" -ErrorAction SilentlyContinue | Select-Object -First 1
  $vidTxt = if ($vidMp4) { $vidMp4.Name } else { "none" }
  Add-Line "files: screenshot=$hasPng perf.trace=$hasTrace video=$vidTxt"
  $tp = Join-Path $Bundle "trace.tail.log"
  if (Test-Path $tp) {
    Add-Line "trace tail (last 5):"
    Get-Content $tp -Tail 5 | ForEach-Object { Add-Line ("  " + (Short $_ 150)) }
  }
}
elseif ($Log) {
  $t = Get-Content $Log -Raw
  $res = [regex]::Match($t, "test result: [^\r\n]+").Value
  Add-Line "log: $Log"
  Add-Line $res
  $matchedBlocks = [regex]::Matches($t, "(?m)^---- (\S+) stdout ----\r?\n(?<body>(?:.*\r?\n){0,8})")
  foreach ($m in $matchedBlocks) {
    Add-Line "FAIL $($m.Groups[1].Value)"
    ($m.Groups["body"].Value -split "\r?\n" | Where-Object { $_.Trim() -and $_ -notmatch "RUST_BACKTRACE" } | Select-Object -First 4) | ForEach-Object { Add-Line ("  " + (Short $_)) }
  }
  if ($matchedBlocks.Count -eq 0) {
    foreach ($pm in [regex]::Matches($t, "(?m)^thread '([^']+)'[^\r\n]*panicked at ([^\r\n]+)\r?\n([^\r\n]+)")) {
      Add-Line ("FAIL {0} ({1})" -f $pm.Groups[1].Value, (Short $pm.Groups[2].Value 80))
      Add-Line ("  " + (Short $pm.Groups[3].Value))
    }
  }
  $e = [regex]::Matches($t, "(?m)^error(\[E\d+\])?:[^\r\n]*") | Select-Object -First 5
  foreach ($x in $e) { Add-Line (Short $x.Value) }
}
elseif ($Rec) {
  $j = Join-Path $Rec "report.json"
  if (-not (Test-Path $j)) { throw "no report.json in $Rec (run: planor-rec analyze <dir>)" }
  $r = Get-Content $j -Raw -Encoding UTF8 | ConvertFrom-Json
  Add-Line "recording: $Rec"
  if ($r.summary) { Add-Line ($r.summary | ConvertTo-Json -Compress -Depth 3) }
  $findings = @($r.checks | Where-Object { $_.verdict -ne "pass" })
  $groups = $findings | Group-Object { "$($_.verdict) $($_.kind)" } | Sort-Object Count -Descending
  foreach ($g in $groups) {
    $first = $g.Group[0]
    $msg = if ($first.detail) { $first.detail } elseif ($first.note) { $first.note } else { "" }
    $when = if ($null -ne $first.time_ms) { " first@$($first.time_ms)ms" } else { "" }
    Add-Line ("{0} x{1}{2} mon{3}: {4}" -f $g.Name, $g.Count, $when, $first.monitor, (Short $msg 150))
  }
  if ($groups.Count -eq 0) { Add-Line "no failing or suspect findings" }
}
elseif ($Trace) {
  if (-not (Test-Path $Trace)) { throw "no such trace $Trace" }
  $all = Get-Content $Trace
  Add-Line "trace: $Trace  ($($all.Count) lines, $(($all[0] -split ' ')[0]) .. $(($all[-1] -split ' ')[0]))"
  $kinds = $all | ForEach-Object { ($_ -split " ", 3)[1] } | Group-Object | Sort-Object Count -Descending | Select-Object -First 8
  Add-Line ("kinds: " + (($kinds | ForEach-Object { "$($_.Name)=$($_.Count)" }) -join " "))
  $lat = @(); $sceneT = $null
  foreach ($l in $all) {
    if ($l -match "^(\d\d):(\d\d):(\d\d)\.(\d+) SCENE ") { $sceneT = ([int]$Matches[1] * 3600 + [int]$Matches[2] * 60 + [int]$Matches[3]) * 1000 + [int]$Matches[4].PadRight(3, "0").Substring(0, 3) }
    elseif ($sceneT -and $l -match "^(\d\d):(\d\d):(\d\d)\.(\d+) PLACE ") { $t = ([int]$Matches[1] * 3600 + [int]$Matches[2] * 60 + [int]$Matches[3]) * 1000 + [int]$Matches[4].PadRight(3, "0").Substring(0, 3); $lat += ($t - $sceneT); $sceneT = $null }
  }
  if ($lat.Count) { $s = $lat | Sort-Object; Add-Line ("scene->first PLACE ms: n={0} p50={1} max={2} first={3}" -f $s.Count, $s[[int]($s.Count / 2)], $s[-1], $lat[0]) }
  $stalls = $all | Where-Object { $_ -match " STALL " } | ForEach-Object { ($_ -split " ", 2)[1] }
  if ($stalls) { Add-Line "STALL (collapsed):"; Collapse $stalls | Select-Object -First 8 | ForEach-Object { Add-Line "  $_" } }
  $slow = $all | Where-Object { $_ -match " SLOW " } | ForEach-Object { ($_ -split " ", 2)[1] }
  if ($slow) { Add-Line "SLOW (collapsed):"; Collapse $slow | Select-Object -First 8 | ForEach-Object { Add-Line "  $_" } }
  $notes = $all | Where-Object { $_ -match " NOTE " }
  foreach ($n in ($notes | Select-Object -First 5)) { Add-Line (Short $n) }
  $odd = $all | Where-Object { $_ -match "INVARIANT|hook removed|reinstall|panic|error" } | ForEach-Object { ($_ -split " ", 2)[1] }
  if ($odd) { Add-Line "warnings (collapsed):"; Collapse $odd | Select-Object -First 6 | ForEach-Object { Add-Line "  $_" } }
}
else {
  $rj = Join-Path $root "build\verify\latest.md"
  if (-not (Test-Path $rj)) { throw "no build\verify\latest.md yet: run .\scripts\verify.ps1" }
  $md = Get-Content $rj -Encoding UTF8
  Add-Line ($md | Where-Object { $_ -match "^Run:" } | Select-Object -First 1)
  Add-Line ($md | Where-Object { $_ -match "^\*\*\d+ pass" } | Select-Object -First 1)
  $bad = $md | Where-Object { $_ -match "^\| (FAIL|FLAKY) " }
  foreach ($b in $bad) {
    $c = ($b -split "\|") | ForEach-Object { $_.Trim() }
    Add-Line ("{0} {1}::{2} ({3} runs, {4}): {5}" -f $c[1], $c[2], $c[3], $c[4], $c[5], (Short $c[6] 200))
  }
  if (-not $bad) { Add-Line "no failures or flakes" }
  $perfLine = $md | Where-Object { $_ -match "^- startup_first_place_ms:" -or $_ -match "^- render_scene_warm_p99_ms:" -or $_ -match "^- process:" }
  foreach ($pl in $perfLine) { Add-Line $pl }
  Add-Line "details: .\scripts\digest.ps1 -Log <build\verify\...\<file>-<test>-run1.log>"
}

$lines | Select-Object -First $Max
if ($lines.Count -gt $Max) { "... ($($lines.Count - $Max) more lines; raise -Max)" }

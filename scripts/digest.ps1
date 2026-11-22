<#
.SYNOPSIS
  Shrink Planor test and debug data into a short digest for an AI agent (or a human in a hurry).
  Never dumps raw logs: failures first, repeated lines collapsed to "xN", numbers kept, noise dropped.

.EXAMPLE
  .\scripts\digest.ps1                          # latest verify results (build\verify\latest.md + results.json)
  .\scripts\digest.ps1 -Rec <recording dir>     # a planor-rec report (report.json) grouped by finding kind
  .\scripts\digest.ps1 -Trace <trace.log>       # a daemon trace: latency pairs, SLOW lines, notes, counts
  .\scripts\digest.ps1 -Log <cargo test log>    # one test log: the failing assertion and the lines around it
  .\scripts\digest.ps1 -Trace <t> -Max 40       # cap the output at N lines (default 60)
#>
param(
  [string]$Rec = "",
  [string]$Trace = "",
  [string]$Log = "",
  [int]$Max = 60
)
$root = Split-Path -Parent $PSScriptRoot
$lines = New-Object System.Collections.Generic.List[string]
function Add-Line([string]$s) { $lines.Add($s) }
function Short([string]$s, [int]$n = 170) { if ($s.Length -gt $n) { $s.Substring(0, $n) + "..." } else { $s } }
function Collapse([string[]]$items) {
  # "text xN" for identical lines after masking numbers that vary run to run (ids, timestamps)
  $items | Group-Object { ($_ -replace "\b\d{5,}\b", "#" -replace "\d+(\.\d+)? ?ms", "N ms") } | Sort-Object Count -Descending |
    ForEach-Object { if ($_.Count -gt 1) { "{0}  x{1}" -f (Short $_.Group[0]), $_.Count } else { Short $_.Group[0] } }
}

if ($Log) {
  $t = Get-Content $Log -Raw
  $res = [regex]::Match($t, "test result: [^\r\n]+").Value
  Add-Line "log: $Log"
  Add-Line $res
  foreach ($m in [regex]::Matches($t, "(?m)^---- (\S+) stdout ----\r?\n(?<body>(?:.*\r?\n){0,8})")) {
    Add-Line "FAIL $($m.Groups[1].Value)"
    ($m.Groups["body"].Value -split "\r?\n" | Where-Object { $_.Trim() -and $_ -notmatch "RUST_BACKTRACE" } | Select-Object -First 4) | ForEach-Object { Add-Line ("  " + (Short $_)) }
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
  # latency: each SCENE to the first PLACE that follows it
  $lat = @(); $sceneT = $null
  foreach ($l in $all) {
    if ($l -match "^(\d\d):(\d\d):(\d\d)\.(\d+) SCENE ") { $sceneT = ([int]$Matches[1] * 3600 + [int]$Matches[2] * 60 + [int]$Matches[3]) * 1000 + [int]$Matches[4].PadRight(3, "0").Substring(0, 3) }
    elseif ($sceneT -and $l -match "^(\d\d):(\d\d):(\d\d)\.(\d+) PLACE ") { $t = ([int]$Matches[1] * 3600 + [int]$Matches[2] * 60 + [int]$Matches[3]) * 1000 + [int]$Matches[4].PadRight(3, "0").Substring(0, 3); $lat += ($t - $sceneT); $sceneT = $null }
  }
  if ($lat.Count) { $s = $lat | Sort-Object; Add-Line ("scene->first PLACE ms: n={0} p50={1} max={2} first={3}" -f $s.Count, $s[[int]($s.Count / 2)], $s[-1], $lat[0]) }
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
    Add-Line ("{0} {1}::{2} ({3} runs): {4}" -f $c[1], $c[2], $c[3], $c[4], (Short $c[5] 200))
  }
  if (-not $bad) { Add-Line "no failures or flakes" }
  Add-Line "details: .\scripts\digest.ps1 -Log <build\verify\...\<file>-<test>-run1.log>"
}
$lines | Select-Object -First $Max
if ($lines.Count -gt $Max) { "... ($($lines.Count - $Max) more lines; raise -Max)" }

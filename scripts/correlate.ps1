<#
.SYNOPSIS
    Join planor-rec report.json / meta.json failures with Planor QPC spans in perf.trace.json (<= 25 lines).
#>
param(
    [Parameter(Mandatory = $true)][string]$Dir,
    [string]$Trace,
    [int]$WindowMs = 50,
    [switch]$AllChecks
)

$ErrorActionPreference = 'Stop'
$reportPath = if ($Dir.EndsWith('.json')) { $Dir } else { Join-Path $Dir 'report.json' }
$baseDir = Split-Path -Parent $reportPath
if (-not $Trace) { $Trace = Join-Path $baseDir 'perf.trace.json' }
if (-not (Test-Path $reportPath)) { throw "report not found: $reportPath" }
if (-not (Test-Path $Trace)) { throw "perf.trace.json not found: $Trace" }

$rep = Get-Content -Raw $reportPath | ConvertFrom-Json
$metaPath = Join-Path $baseDir 'meta.json'
$qpcBase = if (Test-Path $metaPath) { [int64]((Get-Content -Raw $metaPath | ConvertFrom-Json).qpc_start_us) } else { 0 }
$perf = Get-Content -Raw $Trace | ConvertFrom-Json
$events = @($perf.traceEvents | Where-Object { $_.ph -in @('X', 'i') })
$winUs = [int64]$WindowMs * 1000

$targets = @($rep.checks | Where-Object { $AllChecks -or $_.verdict -ne 'pass' } | Select-Object -First 8)
if ($targets.Count -eq 0) {
    Write-Output "CORRELATE ok: 0 flagged checks in $reportPath ($($rep.summary.checks) checks, $($events.Count) trace events, qpc_start=$qpcBase)"
    exit 0
}

$lines = [System.Collections.Generic.List[string]]::new()
$lines.Add("CORRELATE $($targets.Count) flagged check(s) | qpc_start=$qpcBase | win=+/-${WindowMs}ms")
foreach ($c in $targets) {
    $t = [int64]$c.time_qpc_us
    $hits = @($events | Where-Object {
        $ts = [int64]$_.ts
        $dur = if ($null -ne $_.dur) { [int64]$_.dur } else { 0 }
        ($ts -le ($t + $winUs)) -and (($ts + $dur) -ge ($t - $winUs))
    } | Sort-Object { if ($null -ne $_.dur) { [int64]$_.dur } else { [int64]0 } } -Descending | Select-Object -First 3)
    $spanSummary = if ($hits.Count -gt 0) {
        ($hits | ForEach-Object {
            if ($_.ph -eq 'i') { "$($_.name)!" } else { "{0}({1:N1}ms)" -f $_.name, ([double]$_.dur / 1000.0) }
        }) -join ', '
    } else { '<none>' }
    if ($lines.Count -lt 25) {
        $lines.Add(("#{0} [{1}] {2} +{3}ms qpc={4} | {5}" -f $c.id, $c.verdict, $c.kind, $c.time_ms, $t, $spanSummary))
    }
}
$lines | Select-Object -First 25 | ForEach-Object { Write-Output $_ }

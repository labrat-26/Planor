<#
.SYNOPSIS
  Quick AI work journal: log what batch of steps an AI agent is working on and inspect recent handoff state.

.DESCRIPTION
  Stores append-only entries in build\ai-journal\journal.jsonl and renders a concise Markdown summary in
  build\ai-journal\CURRENT.md (gitignored under /build so frequent logging never dirties commits).
  Compatible with Windows PowerShell 5.1.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\ai-journal.ps1 -Show
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\ai-journal.ps1 -Step "Fixing open_landing handoff" -Status working -Agent antigravity -Details "Guard check_settled_parked while waiting_open_landing" -Next "Run unit tests and open_apps feel"
#>
param(
  [string]$Step = "",
  [ValidateSet("working","done","blocked","handoff")]
  [string]$Status = "working",
  [string]$Agent = "ai",
  [string]$Details = "",
  [string]$Next = "",
  [string]$Verified = "",
  [string[]]$Files = @(),
  [switch]$Show,
  [int]$Last = 8,
  [switch]$Json,
  [switch]$Archive
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$dir = Join-Path $root "build\ai-journal"
New-Item -ItemType Directory -Force $dir | Out-Null
$jsonlPath = Join-Path $dir "journal.jsonl"
$mdPath = Join-Path $dir "CURRENT.md"

function Read-Entries([string]$path) {
  $list = [System.Collections.Generic.List[object]]::new()
  if (-not (Test-Path $path)) { return ,$list }
  foreach ($line in (Get-Content -Path $path -Encoding UTF8)) {
    $t = "$line".Trim()
    if (-not $t) { continue }
    try {
      $list.Add(($t | ConvertFrom-Json))
    } catch {
      # Skip malformed partial line if an agent crashed mid-write
    }
  }
  return ,$list
}

function Escape-Md([string]$s) {
  if (-not $s) { return "-" }
  return ($s -replace "`r?`n", " " -replace "\|", "\|").Trim()
}

function Write-CurrentMd([System.Collections.Generic.List[object]]$entries, [string]$outPath) {
  $lines = [System.Collections.Generic.List[string]]::new()
  $lines.Add("# AI Work Journal")
  $lines.Add("")
  if ($entries.Count -eq 0) {
    $lines.Add("_No entries yet._")
    $lines -join "`n" | Set-Content -Path $outPath -Encoding UTF8
    return
  }
  $latest = $entries[$entries.Count - 1]
  $lines.Add("## Current Batch")
  $lines.Add("")
  $lines.Add("- **Status**: ``$($latest.status)`` | **Agent**: ``$($latest.agent)`` | **Time**: $($latest.ts)")
  $lines.Add("- **Step**: $($latest.step)")
  if ($latest.details)  { $lines.Add("- **Details**: $($latest.details)") }
  if ($latest.verified) { $lines.Add("- **Verified**: $($latest.verified)") }
  if ($latest.next)     { $lines.Add("- **Next**: $($latest.next)") }
  $lines.Add("- **Git**: ``$($latest.branch)`` @ ``$($latest.head)``")
  if ($latest.files -and $latest.files.Count -gt 0) {
    $lines.Add("- **Files ($($latest.files.Count))**: ``$($latest.files -join '``, ``')``")
  }
  $lines.Add("")
  $lines.Add("## Recent Batches (newest first)")
  $lines.Add("")
  $lines.Add("| Time | Status | Agent | Step | Verified | Next |")
  $lines.Add("| --- | --- | --- | --- | --- | --- |")
  $start = [Math]::Max(0, $entries.Count - 30)
  for ($i = $entries.Count - 1; $i -ge $start; $i--) {
    $e = $entries[$i]
    $row = "| $(Escape-Md $e.ts) | $(Escape-Md $e.status) | $(Escape-Md $e.agent) | $(Escape-Md $e.step) | $(Escape-Md $e.verified) | $(Escape-Md $e.next) |"
    $lines.Add($row)
  }
  $lines -join "`n" | Set-Content -Path $outPath -Encoding UTF8
}

if ($Archive) {
  if (Test-Path $jsonlPath) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $arch = Join-Path $dir "archive-$stamp.jsonl"
    Move-Item -Path $jsonlPath -Destination $arch -Force
    Write-Host "Archived journal to $arch"
  } else {
    Write-Host "No active journal to archive."
  }
  Write-CurrentMd ([System.Collections.Generic.List[object]]::new()) $mdPath
  if (-not $Step -and -not $Show) { exit 0 }
}

if ($Step) {
  $branch = "unknown"
  $head = "unknown"
  $autoFiles = [System.Collections.Generic.List[string]]::new()
  try {
    Push-Location $root
    $b = (& git rev-parse --abbrev-ref HEAD 2>$null)
    if ($b) { $branch = "$b".Trim() }
    $h = (& git rev-parse --short HEAD 2>$null)
    if ($h) { $head = "$h".Trim() }
    if ($Files.Count -eq 0) {
      $st = & git status --porcelain 2>$null
      foreach ($line in $st) {
        if ("$line".Length -lt 4) { continue }
        $p = "$line".Substring(3).Trim()
        if ($p -match ' -> ') { $p = ($p -split ' -> ')[-1].Trim() }
        if ($p -and $autoFiles.Count -lt 20) {
          $autoFiles.Add($p)
        }
      }
    }
  } finally {
    Pop-Location
  }
  $fileArr = if ($Files.Count -gt 0) { @($Files | Select-Object -First 20) } else { $autoFiles.ToArray() }
  $entry = [ordered]@{
    ts       = (Get-Date -Format "yyyy-MM-ddTHH:mm:ssK")
    status   = $Status
    agent    = $Agent
    branch   = $branch
    head     = $head
    step     = $Step
    details  = $Details
    next     = $Next
    verified = $Verified
    files    = $fileArr
  }
  $jsonLine = ($entry | ConvertTo-Json -Compress -Depth 4)
  Add-Content -Path $jsonlPath -Value $jsonLine -Encoding UTF8
  $all = Read-Entries $jsonlPath
  Write-CurrentMd $all $mdPath
  Write-Host ("Logged [{0}] ({1}) {2}" -f $entry.status, $entry.agent, $entry.step)
}

if ($Show -or (-not $Step -and -not $Archive)) {
  $all = Read-Entries $jsonlPath
  if ($all.Count -eq 0) {
    if ($Json) { Write-Output "[]" } else { Write-Host "ai-journal: no entries yet ($jsonlPath)" }
    exit 0
  }
  $n = if ($Last -lt 1) { 1 } else { $Last }
  $take = [Math]::Min($n, $all.Count)
  $start = $all.Count - $take
  $slice = @()
  for ($i = $start; $i -lt $all.Count; $i++) {
    $slice += $all[$i]
  }
  if ($Json) {
    $slice | ConvertTo-Json -Depth 5
    exit 0
  }
  $latest = $all[$all.Count - 1]
  Write-Host ("== AI Journal (showing last {0} of {1}; branch {2}@{3}) ==" -f $take, $all.Count, $latest.branch, $latest.head)
  Write-Host ("ACTIVE [{0}] ({1}) {2}" -f $latest.status, $latest.agent, $latest.step)
  if ($latest.details)  { Write-Host ("  details : {0}" -f $latest.details) }
  if ($latest.verified) { Write-Host ("  verified: {0}" -f $latest.verified) }
  if ($latest.next)     { Write-Host ("  next    : {0}" -f $latest.next) }
  if ($latest.files -and $latest.files.Count -gt 0) {
    Write-Host ("  files   : {0}" -f ($latest.files -join ", "))
  }
  if ($take -gt 1) {
    Write-Host "-- previous --"
    for ($i = $all.Count - 2; $i -ge $start; $i--) {
      $e = $all[$i]
      $v = if ($e.verified) { " | $($e.verified)" } else { "" }
      Write-Host ("  {0} [{1}] ({2}) {3}{4}" -f $e.ts, $e.status, $e.agent, $e.step, $v)
    }
  }
  Write-Host ("Journal: {0}" -f $mdPath)
}

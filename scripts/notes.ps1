<#
.SYNOPSIS
  Manage tracked Win+W notes in docs/notes.json and regenerate/check docs/NOTES_OPEN.md.

.EXAMPLE
  .\scripts\notes.ps1 -Generate
  .\scripts\notes.ps1 -Check
  .\scripts\notes.ps1 -Add -Time "09:15:00" -Text "window jumped on display plug"
  .\scripts\notes.ps1 -Close -Id N-002 -Test "tests/scn_focus.rs::system_focus_reveals_every_window" -Note "verified fix"
#>
param(
  [switch]$Generate,
  [switch]$Check,
  [switch]$Add,
  [switch]$Close,
  [string]$Id = "",
  [string]$Time = "",
  [string]$Text = "",
  [string]$Test = "",
  [string]$Commit = "",
  [string]$Note = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$jsonPath = Join-Path $root "docs\notes.json"
$mdPath = Join-Path $root "docs\NOTES_OPEN.md"

if (-not (Test-Path $jsonPath)) { throw "missing $jsonPath" }
$doc = Get-Content -Raw $jsonPath -Encoding UTF8 | ConvertFrom-Json

function Build-NotesMarkdown($doc) {
  $open = @($doc.notes | Where-Object { $_.status -ne "fixed" })
  $fixed = @($doc.notes | Where-Object { $_.status -eq "fixed" })
  $lines = [System.Collections.Generic.List[string]]::new()
  $lines.Add("# Open Win+W notes")
  $lines.Add("")
  $lines.Add("Generated from [notes.json](notes.json) via ``scripts\notes.ps1 -Generate``. Raw text lives in ``%LOCALAPPDATA%\planor\trace.log`` (timestamps UTC).")
  $lines.Add("")
  $lines.Add("| ID | Time (UTC) | Note | State |")
  $lines.Add("|---|---|---|---|")
  foreach ($n in $open) {
    $state = if ($n.note) { "$($n.status): $($n.note)" } else { $n.status }
    if ($n.test) { $state += " (``$($n.test)``)" }
    $lines.Add("| $($n.id) | $($n.time_utc) | $(($n.text -replace '\|', '/')) | $(($state -replace '\|', '/')) |")
  }
  $lines.Add("")
  $lines.Add("## Fixed (guarded by regression tests)")
  $lines.Add("")
  $lines.Add("| ID | Time (UTC) | Note | Test / Commit |")
  $lines.Add("|---|---|---|---|")
  foreach ($n in $fixed) {
    $proof = if ($n.commit) { "``$($n.test)`` ($($n.commit))" } else { "``$($n.test)``" }
    $lines.Add("| $($n.id) | $($n.time_utc) | $(($n.text -replace '\|', '/')) | $proof |")
  }
  return ($lines -join "`n")
}

if ($Add) {
  if (-not $Text) { throw "-Add requires -Text" }
  $maxNum = 0
  foreach ($n in $doc.notes) {
    if ($n.id -match "^N-(\d+)$") {
      $v = [int]$Matches[1]
      if ($v -gt $maxNum) { $maxNum = $v }
    }
  }
  $newId = if ($Id) { $Id } else { "N-{0:D3}" -f ($maxNum + 1) }
  $t = if ($Time) { $Time } else { (Get-Date).ToUniversalTime().ToString("HH:mm:ss") }
  $entry = [pscustomobject]@{
    id = $newId
    time_utc = $t
    text = $Text
    status = "open"
    note = $Note
  }
  $doc.notes = @($doc.notes) + $entry
  $doc | ConvertTo-Json -Depth 5 | Set-Content $jsonPath -Encoding UTF8
  $Generate = $true
  Write-Output "Added note $newId"
}

if ($Close) {
  if (-not $Id -or -not $Test) { throw "-Close requires -Id and -Test" }
  $hit = $doc.notes | Where-Object { $_.id -eq $Id } | Select-Object -First 1
  if (-not $hit) { throw "note id not found: $Id" }
  $hit.status = "fixed"
  $hit | Add-Member -NotePropertyName "test" -NotePropertyValue $Test -Force
  if ($Commit) { $hit | Add-Member -NotePropertyName "commit" -NotePropertyValue $Commit -Force }
  if ($Note) { $hit.note = $Note }
  $doc | ConvertTo-Json -Depth 5 | Set-Content $jsonPath -Encoding UTF8
  $Generate = $true
  Write-Output "Closed note $Id with test $Test"
}

if ($Generate) {
  $md = Build-NotesMarkdown $doc
  $md | Set-Content $mdPath -Encoding UTF8
  Write-Output "Generated $mdPath"
}

if ($Check) {
  $seenIds = @{}
  foreach ($n in $doc.notes) {
    if (-not $n.id -or -not $n.text) { throw "invalid note entry missing id/text" }
    if ($seenIds.ContainsKey($n.id)) { throw "duplicate note id: $($n.id)" }
    $seenIds[$n.id] = $true
    if ($n.status -eq "fixed") {
      if (-not $n.test) { throw "fixed note $($n.id) has no linked regression test" }
      if ($n.test -match "^([^:]+)::(.+)$") {
        $relFile = $Matches[1]
        $fnName = $Matches[2]
        $full = Join-Path $root $relFile
        if (-not (Test-Path $full)) { throw "note $($n.id) cites missing test file: $relFile" }
        $raw = Get-Content -Raw $full
        if ($raw -notmatch "fn\s+$([regex]::Escape($fnName))\b") {
          throw "note $($n.id) cites missing test function: $fnName in $relFile"
        }
      }
    }
  }
  $expected = (Build-NotesMarkdown $doc).Trim() -replace "\r\n", "`n"
  if (-not (Test-Path $mdPath)) { throw "missing $mdPath; run scripts\notes.ps1 -Generate" }
  $actual = (Get-Content -Raw $mdPath -Encoding UTF8).Trim() -replace "\r\n", "`n"
  if ($expected -ne $actual) {
    throw "$mdPath is out of sync with docs\notes.json; run scripts\notes.ps1 -Generate"
  }
  Write-Output "notes ok: $($doc.notes.Count) tracked note(s), all fixed notes linked to valid regression tests"
}

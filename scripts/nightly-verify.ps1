<#
.SYNOPSIS
  Unattended nightly verification + old run pruning (9.10).
#>
param(
  [switch]$NoDisplaySwitch,
  [int]$KeepRuns = 14
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

Write-Output "=== Planor Nightly Verification ($(Get-Date -Format 'u')) ==="
$verifyArgs = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", (Join-Path $PSScriptRoot "verify.ps1"), "-Tier", "smoke", "-Perf", "-UpdateDocs")
if ($NoDisplaySwitch) { $verifyArgs += "-NoDisplaySwitch" }
powershell @verifyArgs
$exitCode = $LASTEXITCODE

$rec = Join-Path $root "tools\planor-rec-bin\planor-rec.exe"
$verifyRoot = Join-Path $root "build\verify"
if ((Test-Path $rec) -and (Test-Path $verifyRoot)) {
  & $rec prune $verifyRoot --keep $KeepRuns
} else {
  $dirs = Get-ChildItem $verifyRoot -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending | Select-Object -Skip $KeepRuns
  foreach ($d in $dirs) { Remove-Item $d.FullName -Recurse -Force -ErrorAction SilentlyContinue }
}

Write-Output "=== Nightly digest ==="
powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "digest.ps1")
exit $exitCode

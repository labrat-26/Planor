<#
.SYNOPSIS
  Register or remove a daily 03:00 Task Scheduler job running scripts/nightly-verify.ps1.
#>
param(
  [string]$TaskName = "PlanorNightlyVerify",
  [string]$Time = "03:00",
  [switch]$DryRun,
  [switch]$Unregister
)

$ErrorActionPreference = "Stop"
$scriptPath = Join-Path $PSScriptRoot "nightly-verify.ps1"
$taskCmd = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$scriptPath`" -NoDisplaySwitch"

if ($Unregister) {
  $cmd = "schtasks /Delete /TN `"$TaskName`" /F"
  if ($DryRun) { Write-Output "[DryRun] $cmd"; exit 0 }
  cmd /c $cmd
  exit $LASTEXITCODE
}

$cmd = "schtasks /Create /F /SC DAILY /TN `"$TaskName`" /TR `"$($taskCmd.Replace('"', '\"'))`" /ST $Time"
if ($DryRun) {
  Write-Output "[DryRun] $cmd"
  exit 0
}
schtasks /Create /F /SC DAILY /TN $TaskName /TR $taskCmd /ST $Time
exit $LASTEXITCODE

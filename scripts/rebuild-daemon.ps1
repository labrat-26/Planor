<#
.SYNOPSIS
  Safely stop the running release daemon, rebuild target\release\planor.exe, restart it hidden, and verify readiness.
#>
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$release = Join-Path $root "target\release\planor.exe"

# Compile-check first: a build error must not leave the owner without a running daemon.
cargo check --release --bin planor
if ($LASTEXITCODE -ne 0) { throw "cargo check failed; the running daemon was left alone" }

$existing = @(Get-Process planor -ErrorAction SilentlyContinue)
if ($existing.Count -gt 0) {
  Write-Output ("Stopping existing planor.exe pid(s): " + (($existing | ForEach-Object { $_.Id }) -join ", "))
  if (Test-Path $release) {
    & $release stop | Out-Null
  }
  $deadline = (Get-Date).AddSeconds(6)
  while ((Get-Date) -lt $deadline) {
    if (-not (Get-Process planor -ErrorAction SilentlyContinue)) { break }
    Start-Sleep -Milliseconds 100
  }
  $stuck = @(Get-Process planor -ErrorAction SilentlyContinue)
  if ($stuck.Count -gt 0) {
    Write-Output "Forcing remaining planor.exe exit..."
    $stuck | Stop-Process -Force
    Start-Sleep -Milliseconds 300
  }
}

Write-Output "Building release target\release\planor.exe..."
$sw = [System.Diagnostics.Stopwatch]::StartNew()
cargo build --release --bin planor
if ($LASTEXITCODE -ne 0) { throw "cargo build --release --bin planor failed" }
$sw.Stop()

Write-Output "Starting updated daemon..."
Start-Process -FilePath $release -ArgumentList "start" -WindowStyle Hidden

$ready = $false
$state = $null
$deadline = (Get-Date).AddSeconds(8)
while ((Get-Date) -lt $deadline) {
  try {
    $raw = cmd /d /c "`"$release`" state --json 2>nul"
    if ($LASTEXITCODE -eq 0 -and $raw) {
      $state = ($raw -join "`n") | ConvertFrom-Json
      if ($state.schema_version -ge 1) {
        $ready = $true
        break
      }
    }
  } catch {}
  Start-Sleep -Milliseconds 100
}
if (-not $ready) { throw "updated daemon did not answer state --json within 8s" }

$newProc = Get-Process planor -ErrorAction SilentlyContinue | Select-Object -First 1
Write-Output ("Ready: pid={0} build={1:N2}s monitors={2} tiles={3}" -f $newProc.Id, $sw.Elapsed.TotalSeconds, $state.monitors.Count, $state.tiles.Count)

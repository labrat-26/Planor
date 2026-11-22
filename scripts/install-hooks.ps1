<#
.SYNOPSIS
  Install the Planor pre-commit hook (.git/hooks/pre-commit).
#>
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$hookDir = Join-Path $root ".git\hooks"
New-Item -ItemType Directory -Force $hookDir | Out-Null
$hookPath = Join-Path $hookDir "pre-commit"

$script = @'
#!/bin/sh
set -e

echo "[pre-commit] checking for println! in backend/daemon/render/win32/perf/watchdog/tray/config_watch..."
for f in src/backend.rs src/daemon.rs src/render.rs src/win32.rs src/perf.rs src/watchdog.rs src/tray.rs src/config_watch.rs; do
  if [ -f "$f" ]; then
    if awk '/#\[cfg\(test\)\]/{exit} /(^|[^a-zA-Z0-9_])println!/{print FILENAME ":" NR ": " $0; found=1} END{if(found) exit 1}' "$f"; then
      :
    else
      echo "[pre-commit] ERROR: println! outside tests in $f (use diag::trace)"
      exit 1
    fi
  fi
done

echo "[pre-commit] running core, property & simulated daemon unit tests..."
cargo test --test core --quiet
cargo test --test prop_core --quiet
cargo test --bin planor sim_tests --quiet

echo "[pre-commit] checking tracked notes (docs/notes.json <-> docs/NOTES_OPEN.md)..."
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/notes.ps1 -Check
'@

$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($hookPath, ($script -replace "`r`n", "`n"), $utf8NoBom)
Write-Output "Installed $hookPath"

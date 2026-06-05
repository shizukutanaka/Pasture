# Minimal bootstrap for Windows: build pasture and place it under a user bin dir.
# Usage:  powershell -ExecutionPolicy Bypass -File install.ps1
$ErrorActionPreference = "Stop"

Write-Host "Installing pasture..."

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "Rust/cargo not found. Install it from https://rustup.rs then re-run."
    exit 1
}

cargo build --release

$dest = if ($env:PASTURE_BIN_DIR) { $env:PASTURE_BIN_DIR } else { Join-Path $env:USERPROFILE ".local\bin" }
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Copy-Item "target\release\pasture.exe" (Join-Path $dest "pasture.exe") -Force
Write-Host "Installed: $dest\pasture.exe"

Write-Host ""
Write-Host "Next step:  pasture up    (downloads the model if needed, then starts the proxy)"

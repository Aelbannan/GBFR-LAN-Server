# Build gbfr-lan-server.exe (HTTP + WebSocket, no TLS). Cargo workspace build (see ../Cargo.toml).
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
Set-Location $root
$env:CARGO_TARGET_DIR = Join-Path $root "target"
cargo build --release -p gbfr-lan-server
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Copy-Item -Force (Join-Path $env:CARGO_TARGET_DIR "release\gbfr-lan-server.exe") (Join-Path $here "gbfr-lan-server.exe")
Write-Host "Wrote $here\gbfr-lan-server.exe"

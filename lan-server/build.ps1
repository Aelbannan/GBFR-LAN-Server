# Build gbfr-lan-server.exe (HTTP + WebSocket, no TLS).
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $here
$env:CARGO_TARGET_DIR = Join-Path $here "target"
cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Copy-Item -Force (Join-Path $here "target\release\gbfr-lan-server.exe") (Join-Path $here "gbfr-lan-server.exe")
Write-Host "Wrote $here\gbfr-lan-server.exe"


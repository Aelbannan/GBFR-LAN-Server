# Build gbfr_http.dll into this folder (cargo workspace build; see ../Cargo.toml).
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
Set-Location $root
$env:CARGO_TARGET_DIR = Join-Path $root "target"
cargo build --release -p steam-http-shim
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Copy-Item -Force (Join-Path $env:CARGO_TARGET_DIR "release\gbfr_http.dll") (Join-Path $here "gbfr_http.dll")
Write-Host "Wrote $here\gbfr_http.dll ($((Get-Item (Join-Path $here 'gbfr_http.dll')).Length) bytes)"

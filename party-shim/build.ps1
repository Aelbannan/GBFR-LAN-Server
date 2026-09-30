# Build PartyWin.dll into this folder (cargo workspace build; see ../Cargo.toml).
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
Set-Location $root
$env:CARGO_TARGET_DIR = Join-Path $root "target"
cargo build --release -p party-shim
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Copy-Item -Force (Join-Path $env:CARGO_TARGET_DIR "release\PartyWin.dll") (Join-Path $here "PartyWin.dll")
Write-Host "Wrote $here\PartyWin.dll ($((Get-Item (Join-Path $here 'PartyWin.dll')).Length) bytes)"

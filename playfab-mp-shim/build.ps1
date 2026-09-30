# Build PlayFabMultiplayerWin.dll into this folder (cargo workspace build; see ../Cargo.toml).
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
Set-Location $root
$env:CARGO_TARGET_DIR = Join-Path $root "target"
cargo build --release -p playfab-mp-shim
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Copy-Item -Force (Join-Path $env:CARGO_TARGET_DIR "release\PlayFabMultiplayerWin.dll") (Join-Path $here "PlayFabMultiplayerWin.dll")
Write-Host "Wrote $here\PlayFabMultiplayerWin.dll ($((Get-Item (Join-Path $here 'PlayFabMultiplayerWin.dll')).Length) bytes)"

# Build LAN PlayFabMultiplayerWin.dll into this folder.
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $here
rustc --crate-type cdylib --edition 2021 src\lib.rs -o PlayFabMultiplayerWin.dll -C opt-level=2
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host "Wrote $here\PlayFabMultiplayerWin.dll ($((Get-Item PlayFabMultiplayerWin.dll).Length) bytes)"

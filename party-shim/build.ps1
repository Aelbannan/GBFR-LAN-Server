# Build LAN PartyWin.dll into this folder.
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $here
$env:BUILD_STAMP = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds().ToString()
rustc --crate-type cdylib --edition 2021 src\lib.rs -o PartyWin.dll -C opt-level=2
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host "Wrote $here\PartyWin.dll ($((Get-Item PartyWin.dll).Length) bytes)"

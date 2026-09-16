# Build gbfr_http.dll (ISteamHTTP WinHTTP shim) into this folder.
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $here
$env:BUILD_STAMP = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds().ToString()
rustc --crate-type cdylib --edition 2021 src\lib.rs -o gbfr_http.dll -C opt-level=2 -l winhttp
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host "Wrote $here\gbfr_http.dll"

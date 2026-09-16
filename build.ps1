# Build all three shims and the LAN server into their component folders.
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path

$components = @(
    "steam-http-shim",
    "party-shim",
    "playfab-mp-shim",
    "lan-server"
)

foreach ($name in $components) {
    Write-Host ""
    Write-Host ">> $name"
    & (Join-Path $here "$name\build.ps1")
    if ($LASTEXITCODE -and $LASTEXITCODE -ne 0) {
        throw "build failed: $name (exit $LASTEXITCODE)"
    }
}

Write-Host ""
Write-Host "Build complete:"
Write-Host "  steam-http-shim\gbfr_http.dll"
Write-Host "  party-shim\PartyWin.dll"
Write-Host "  playfab-mp-shim\PlayFabMultiplayerWin.dll"
Write-Host "  lan-server\gbfr-lan-server.exe"

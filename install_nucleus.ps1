# Build the LAN shims + broker and copy them into a Nucleus Co-op handler folder.
# Nucleus deploys these into every instance; player 1 starts the server.
#
#   powershell -ExecutionPolicy Bypass -File install_nucleus.ps1
#   powershell -ExecutionPolicy Bypass -File install_nucleus.ps1 -SkipBuild
param(
    [switch]$SkipBuild,
    [string]$HandlerDir = ""
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path

if (-not $HandlerDir) {
    $HandlerDir = "C:\NucleusCoop\handlers\Granblue Fantasy Relink"
}

if (-not $SkipBuild) {
    & (Join-Path $here "build.ps1")
    if ($LASTEXITCODE -and $LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

function Require-File {
    param([string]$Path)
    if (-not (Test-Path $Path)) {
        throw "missing $Path - build it first or omit -SkipBuild"
    }
    return (Get-Item $Path)
}

$party = Require-File (Join-Path $here "party-shim\PartyWin.dll")
$mp = Require-File (Join-Path $here "playfab-mp-shim\PlayFabMultiplayerWin.dll")
$http = Require-File (Join-Path $here "steam-http-shim\gbfr_http.dll")
$server = Require-File (Join-Path $here "lan-server\gbfr-lan-server.exe")
$ini = Require-File (Join-Path $here "lan.ini")

$loadDlls = Join-Path $HandlerDir "steam_settings\load_dlls"
New-Item -ItemType Directory -Force -Path $HandlerDir | Out-Null
New-Item -ItemType Directory -Force -Path $loadDlls | Out-Null

Copy-Item -Force $party.FullName (Join-Path $HandlerDir "PartyWin.dll")
Copy-Item -Force $mp.FullName (Join-Path $HandlerDir "PlayFabMultiplayerWin.dll")
Copy-Item -Force $server.FullName (Join-Path $HandlerDir "gbfr-lan-server.exe")
Copy-Item -Force $ini.FullName (Join-Path $HandlerDir "lan.ini")
Copy-Item -Force $http.FullName (Join-Path $loadDlls "gbfr_http.dll")

# Experimental Goldberg blocks connect() to non-LAN IPs; an empty file disables that hook.
$disableLan = Join-Path $HandlerDir "steam_settings\disable_lan_only.txt"
if (-not (Test-Path $disableLan)) {
    [IO.File]::WriteAllText($disableLan, "")
}

Write-Host ""
Write-Host "Nucleus handler package:"
Get-ChildItem $HandlerDir, $loadDlls -File |
    Where-Object { $_.Name -match 'PartyWin|PlayFabMultiplayer|gbfr-lan-server|lan.ini|gbfr_http' } |
    ForEach-Object { Write-Host ("  {0}  {1} bytes" -f $_.FullName, $_.Length) }
Write-Host ""
Write-Host "Reload the handler in Nucleus. Player 1 starts gbfr-lan-server.exe."

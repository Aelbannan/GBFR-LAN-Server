# Install the LAN shims into a Granblue Fantasy: Relink folder.
#
#   powershell -ExecutionPolicy Bypass -File install.ps1 -GameDir "C:\...\Granblue Fantasy Relink"
#   powershell -ExecutionPolicy Bypass -File install.ps1 -GameDir "..." -SkipBuild
param(
    [Parameter(Mandatory = $true)]
    [string]$GameDir,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path

if (-not (Test-Path $GameDir)) {
    throw "game folder not found: $GameDir"
}
$GameDir = (Resolve-Path $GameDir).Path
if (-not (Test-Path (Join-Path $GameDir "granblue_fantasy_relink.exe"))) {
    throw "granblue_fantasy_relink.exe not found in $GameDir"
}

if (-not $SkipBuild) {
    & (Join-Path $here "build.ps1")
    if ($LASTEXITCODE -and $LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

# gbfr_http.dll loads into the game process through Goldberg's load_dlls folder.
$loadDlls = Join-Path $GameDir "steam_settings\load_dlls"
New-Item -ItemType Directory -Force -Path $loadDlls | Out-Null
Copy-Item -Force (Join-Path $here "steam-http-shim\gbfr_http.dll") (Join-Path $loadDlls "gbfr_http.dll")
Write-Host "Installed $loadDlls\gbfr_http.dll"

# PartyWin.dll / PlayFabMultiplayerWin.dll replace the shipping DLLs next to the exe.
# Back up the originals (multi-MB Microsoft DLLs) once, as <name>.ms.
function Install-GameDll {
    param(
        [string]$Name,
        [string]$Built,
        [int]$OrigMinBytes = 1000000
    )
    $dest = Join-Path $GameDir $Name
    $bak = Join-Path $GameDir ($Name + ".ms")
    if (Test-Path $dest) {
        $len = (Get-Item $dest).Length
        if ($len -ge $OrigMinBytes -and -not (Test-Path $bak)) {
            Copy-Item -Force $dest $bak
            Write-Host "Backed up $Name ($len bytes) -> $Name.ms"
        }
    }
    Copy-Item -Force $Built $dest
    Write-Host "Installed $dest"
}

Install-GameDll "PartyWin.dll" (Join-Path $here "party-shim\PartyWin.dll")
Install-GameDll "PlayFabMultiplayerWin.dll" (Join-Path $here "playfab-mp-shim\PlayFabMultiplayerWin.dll")

$gameIni = Join-Path $GameDir "lan.ini"
if (-not (Test-Path $gameIni)) {
    Copy-Item -Force (Join-Path $here "lan.ini") $gameIni
    Write-Host "Wrote $gameIni"
} else {
    Write-Host "Left existing $gameIni (not overwritten)"
}

Copy-Item -Force (Join-Path $here "lan-server\gbfr-lan-server.exe") (Join-Path $GameDir "gbfr-lan-server.exe")
Write-Host "Installed $GameDir\gbfr-lan-server.exe"

Write-Host ""
Write-Host "Next:"
Write-Host "  1. Edit $gameIni if the host is another PC ([server] host)."
Write-Host "  2. On the host PC, run $GameDir\gbfr-lan-server.exe"
Write-Host "  3. Launch the game and use the multiplayer quest counter."
Write-Host ""
Write-Host "Logs: steam_http_shim.log, playfab_mp_shim.log, party_shim.log next to the game exe."

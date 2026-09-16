# Allow the LAN broker and Party UDP through Windows Firewall (Private profile).
# Run once as Administrator on EVERY PC that hosts or joins.
#
#   powershell -ExecutionPolicy Bypass -File open_firewall.ps1
#   powershell -ExecutionPolicy Bypass -File open_firewall.ps1 -GameExe "C:\...\granblue_fantasy_relink.exe"
param(
    [string]$GameExe = ""
)

$ErrorActionPreference = "Stop"

$rules = @(
    @{ Name = "GBFR LAN broker HTTP"; Proto = "TCP"; Port = 8080 },
    @{ Name = "GBFR LAN broker WS";   Proto = "TCP"; Port = 8081 },
    @{ Name = "GBFR LAN Party UDP";   Proto = "UDP"; Port = 27015 }
)
foreach ($r in $rules) {
    $existing = Get-NetFirewallRule -DisplayName $r.Name -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "Already present: $($r.Name)"
        continue
    }
    New-NetFirewallRule -DisplayName $r.Name -Direction Inbound -Action Allow -Protocol $r.Proto -LocalPort $r.Port -Profile Private | Out-Null
    Write-Host "Added $($r.Name) $($r.Proto)/$($r.Port)"
}

if ($GameExe -and (Test-Path $GameExe)) {
    $name = "GBFR LAN Relink"
    if (-not (Get-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue)) {
        New-NetFirewallRule -DisplayName $name -Direction Inbound -Action Allow -Program $GameExe -Protocol Any -Profile Private | Out-Null
        Write-Host "Added inbound allow for $GameExe"
    } else {
        Write-Host "Already present: $name"
    }
}

Write-Host "Done. Guests only need UDP 27015 if they will host later."

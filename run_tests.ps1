# Build the four components and run every test suite, in one command.
#
#   powershell -ExecutionPolicy Bypass -File run_tests.ps1
#   ... -SkipBuild     reuse the binaries already built (faster inner loop)
#
# Suites (see each project's tests/ folder):
#   common/json_test          JSON field accessors shared by both shims
#   common/http_test          bounded HTTP client (connect cap, response cap, status parsing)
#   lan-server                cargo test: lobby filter, HTTP/WS integration, framing (in-process)
#   party-shim/reliable_test  loss/duplication/reordering of the reliable-delivery state machine
#   party-shim/wire_test      UDP header layout in both protocol versions + truncation safety
#   party-shim/broker_http    PartyWin.dll against a mock broker (full, loss, outage)
#   playfab-mp-shim           async broker contract, state-change queue, real-broker PostUpdate
#   steam-http-shim/pe_test   PE import patching (synthetic image + guard page) and ABI packing
#
# Exit code is 0 only when every suite passed.

param(
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $here

$results = New-Object System.Collections.ArrayList
$buildStamp = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds().ToString()

function Add-Result([string]$Name, [bool]$Ok, [double]$Seconds) {
    [void]$results.Add([pscustomobject]@{ Name = $Name; Ok = $Ok; Seconds = $Seconds })
}

function Invoke-Step {
    param([string]$Name, [scriptblock]$Body)
    Write-Host ""
    Write-Host ">> $Name" -ForegroundColor Cyan
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $ok = $true
    $global:LASTEXITCODE = 0
    try {
        & $Body
        if ($global:LASTEXITCODE -ne 0) { throw "exit code $global:LASTEXITCODE" }
    } catch {
        Write-Host "   FAILED: $_" -ForegroundColor Red
        $ok = $false
    }
    $sw.Stop()
    Add-Result $Name $ok $sw.Elapsed.TotalSeconds
    if ($ok) {
        Write-Host ("   ok ({0:N1}s)" -f $sw.Elapsed.TotalSeconds) -ForegroundColor Green
    }
}

# rustc-compiled suites: the shim build has no cargo and no dependencies.
function Build-Test {
    param([string]$Source, [string]$Output)
    Write-Host "   rustc $Source"
    rustc --edition 2021 -O $Source -o $Output
    if ($LASTEXITCODE -ne 0) { throw "rustc failed for $Source" }
}

function Run-Exe {
    param([string]$Exe, [string[]]$Arguments = @())
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Exe reported failures" }
}

if (-not $SkipBuild) {
    Invoke-Step "build all four components" {
        # A broker left running by a previous test run holds gbfr-lan-server.exe open and makes
        # the copy step of lan-server/build.ps1 fail with a file-in-use error.
        Stop-Process -Name gbfr-lan-server -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 200
        & (Join-Path $here "build.ps1")
        if ($LASTEXITCODE -ne 0) { throw "build.ps1 failed" }
    }
}

# ── common ──────────────────────────────────────────────────────────────────────────────
Invoke-Step "common/json_test" {
    Build-Test (Join-Path $here "common\json_test.rs") (Join-Path $here "common\json_test.exe")
    Run-Exe (Join-Path $here "common\json_test.exe")
}

Invoke-Step "common/http_test" {
    Build-Test (Join-Path $here "common\http_test.rs") (Join-Path $here "common\http_test.exe")
    Run-Exe (Join-Path $here "common\http_test.exe")
}

# ── lan-server ──────────────────────────────────────────────────────────────────────────
Invoke-Step "lan-server cargo test" {
    Push-Location (Join-Path $here "lan-server")
    try {
        # One target dir for the whole workspace, matching the component build scripts.
        $env:CARGO_TARGET_DIR = Join-Path $here "target"
        cargo test --release
        if ($LASTEXITCODE -ne 0) { throw "cargo test failed" }
    } finally {
        Pop-Location
    }
}

# ── party-shim ──────────────────────────────────────────────────────────────────────────
$partyTests = Join-Path $here "party-shim\tests"
New-Item -ItemType Directory -Force -Path $partyTests | Out-Null

Invoke-Step "party-shim/reliable_test" {
    Build-Test (Join-Path $here "party-shim\reliable_test.rs") (Join-Path $partyTests "reliable_test.exe")
    Run-Exe (Join-Path $partyTests "reliable_test.exe")
}

Invoke-Step "party-shim/wire_test" {
    Build-Test (Join-Path $here "party-shim\tests\wire_test.rs") (Join-Path $partyTests "wire_test.exe")
    Run-Exe (Join-Path $partyTests "wire_test.exe")
}

Invoke-Step "party-shim/broker_http_test (full, loss, outage)" {
    Build-Test (Join-Path $here "party-shim\broker_http_test.rs") (Join-Path $partyTests "broker_http_test.exe")
    # The test loads PartyWin.dll from next to the exe.
    Copy-Item -Force (Join-Path $here "party-shim\PartyWin.dll") (Join-Path $partyTests "PartyWin.dll")
    Run-Exe (Join-Path $partyTests "broker_http_test.exe")
    Run-Exe (Join-Path $partyTests "broker_http_test.exe") @("--loss")
    Run-Exe (Join-Path $partyTests "broker_http_test.exe") @("--outage")
}

# ── playfab-mp-shim ─────────────────────────────────────────────────────────────────────
$pfTests = Join-Path $here "playfab-mp-shim\tests"

Invoke-Step "playfab-mp-shim/async_broker_test" {
    Build-Test (Join-Path $pfTests "async_broker_test.rs") (Join-Path $pfTests "async_broker_test.exe")
    Run-Exe (Join-Path $pfTests "async_broker_test.exe") @((Join-Path $here "playfab-mp-shim\PlayFabMultiplayerWin.dll"))
}

Invoke-Step "playfab-mp-shim/pfqueue_smoke" {
    Build-Test (Join-Path $pfTests "pfqueue_smoke.rs") (Join-Path $pfTests "pfqueue_smoke.exe")
    Run-Exe (Join-Path $pfTests "pfqueue_smoke.exe") @((Join-Path $here "playfab-mp-shim\PlayFabMultiplayerWin.dll"))
}

Invoke-Step "playfab-mp-shim/postupdate_smoke (real broker)" {
    $broker = Join-Path $here "lan-server\gbfr-lan-server.exe"
    if (-not (Test-Path $broker)) { throw "broker not built: $broker" }
    Build-Test (Join-Path $pfTests "postupdate_smoke.rs") (Join-Path $pfTests "postupdate_smoke.exe")
    Stop-Process -Name gbfr-lan-server -Force -ErrorAction SilentlyContinue
    $proc = Start-Process -FilePath $broker `
        -ArgumentList @("--http-port", "18080", "--ws-port", "18081", "--ini", (Join-Path $here "lan.ini")) `
        -PassThru -WindowStyle Hidden
    try {
        Start-Sleep -Milliseconds 800
        $env:GBFR_LAN_STUB = "127.0.0.1:18080"
        Run-Exe (Join-Path $pfTests "postupdate_smoke.exe") @((Join-Path $here "playfab-mp-shim\PlayFabMultiplayerWin.dll"))
    } finally {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        Remove-Item Env:\GBFR_LAN_STUB -ErrorAction SilentlyContinue
    }
}

# ── steam-http-shim ─────────────────────────────────────────────────────────────────────
$shTests = Join-Path $here "steam-http-shim\tests"
New-Item -ItemType Directory -Force -Path $shTests | Out-Null

Invoke-Step "steam-http-shim/pe_test" {
    Build-Test (Join-Path $shTests "pe_test.rs") (Join-Path $shTests "pe_test.exe")
    Run-Exe (Join-Path $shTests "pe_test.exe")
}

# ── summary ─────────────────────────────────────────────────────────────────────────────
Write-Host ""
Write-Host "== summary ==" -ForegroundColor Cyan
$failed = 0
foreach ($r in $results) {
    if ($r.Ok) {
        Write-Host ("  PASS  {0,-52} {1,6:N1}s" -f $r.Name, $r.Seconds) -ForegroundColor Green
    } else {
        Write-Host ("  FAIL  {0,-52} {1,6:N1}s" -f $r.Name, $r.Seconds) -ForegroundColor Red
        $failed++
    }
}
if ($failed -gt 0) {
    Write-Host "$failed suite(s) failed" -ForegroundColor Red
    exit 1
}
Write-Host "all suites passed" -ForegroundColor Green
exit 0

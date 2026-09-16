# Build the PlayFab shim and run its whole test suite.
#
#   tests\run_tests.ps1
#
# Suites:
#   async_broker_test  - self-contained; in-process mock broker; proves the async contract
#                        (immediate returns, no HTTP on the tick, completion ordering,
#                        parked joins, async service errors, instance-generation guard).
#   pfqueue_smoke      - state-change queue lifecycle / heartbeat.
#   postupdate_smoke   - real broker on 18080; create/read/delete/scalar PostUpdate round trip.
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
$stub = Split-Path -Parent $root
Set-Location $root

Write-Host "== build shim =="
& "$root\build.ps1"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "== build tests =="
rustc --edition 2021 -O tests\async_broker_test.rs -o tests\async_broker_test.exe
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
rustc --edition 2021 -O tests\postupdate_smoke.rs -o tests\postupdate_smoke.exe
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
rustc --edition 2021 -O tests\pfqueue_smoke.rs -o tests\pfqueue_smoke.exe
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "== async_broker_test (mock broker) =="
& "$here\async_broker_test.exe" "$root\PlayFabMultiplayerWin.dll"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "== pfqueue_smoke =="
& "$here\pfqueue_smoke.exe" "$root\PlayFabMultiplayerWin.dll"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "== postupdate_smoke (real broker on 18080) =="
$broker = "$stub\lan-server\gbfr-lan-server.exe"
if (-not (Test-Path $broker)) { throw "broker not built: $broker" }
$proc = Start-Process -FilePath $broker `
    -ArgumentList @("--http-port", "18080", "--ws-port", "18081") `
    -PassThru -WindowStyle Hidden
try {
    Start-Sleep -Milliseconds 800
    $env:GBFR_LAN_STUB = "127.0.0.1:18080"
    & "$here\postupdate_smoke.exe" "$root\PlayFabMultiplayerWin.dll"
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    Remove-Item Env:\GBFR_LAN_STUB -ErrorAction SilentlyContinue
}

Write-Host "PlayFab shim tests: all suites passed"

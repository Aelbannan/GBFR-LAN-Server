# Multi-instance and Nucleus Co-op

The stack supports several game instances on one PC (Nucleus Co-op) because every client is
just another peer: one broker process, one UDP socket per instance, per-instance Steam IDs.

## Install

```powershell
powershell -ExecutionPolicy Bypass -File install_nucleus.ps1
powershell -ExecutionPolicy Bypass -File install_nucleus.ps1 -SkipBuild   # reuse the build
powershell -ExecutionPolicy Bypass -File install_nucleus.ps1 -HandlerDir "D:\Nucleus\handlers\GBFR"
```

The script builds (unless `-SkipBuild`) and copies into the handler folder
(`C:\NucleusCoop\handlers\Granblue Fantasy Relink` by default):

- `PartyWin.dll`, `PlayFabMultiplayerWin.dll`, `gbfr-lan-server.exe`, `lan.ini` → handler root
- `steam_settings\load_dlls\gbfr_http.dll` → loaded by Goldberg
- `steam_settings\disable_lan_only.txt` (created empty if missing) — disables Goldberg's
  experimental LAN-only `connect()` restriction

Then **reload the handler** in Nucleus. Player 1 starts the broker; every other instance
only launches the game.

## What a Nucleus handler does per instance

`install_nucleus.ps1` only prepares a package. The handler (a `.js` in the handlers folder)
is what makes the copies and launches:

1. Copies the handler package into each instance folder
   (`Context.CopyScriptFolder`), so each instance has its own shims, `lan.ini`, and logs.
2. Writes `steam_settings\configs.user.ini` with `account_steamid = Context.PlayerSteamID` —
   each instance needs a **distinct Steam ID**; duplicate IDs collapse the two players into
   one mesh member.
3. Copies `PartyWin.dll` / `PlayFabMultiplayerWin.dll` / `lan.ini` into the instance root and
   `gbfr_http.dll` into the instance's `steam_settings\load_dlls`.
4. For player 1: kills any stale `gbfr-lan-server.exe`, starts it from the instance folder,
   and waits for `127.0.0.1:8080` to listen. `gbfr-lan-server` is also in
   `KillProcessesOnClose`.
5. Sets up per-pad input blocking (XInputPlus) if the handler uses it.

Known-good handler settings: `SteamID = 881020`, `NeedsSteamEmulation = false`,
`CreateSteamAppIdByExe = true`, `DirExclusions = ["steam_settings"]` (so the emulator settings
are not overwritten per instance), `MaxPlayers = 8`, `MaxPlayersOneMonitor = 4`.

## Ports and addressing

| Traffic | Port | Note |
|---|---|---|
| HTTP broker | TCP 8080 | one process (player 1's instance); all instances use `127.0.0.1` |
| Cygames WebSocket | TCP 8081 | same broker |
| Party mesh | UDP `[party] udp_port` (27015 by default) | **per instance** |

The first instance to launch binds 27015; later instances find it busy and log
`bind_udp fallback ephemeral (27015 busy)`, then use a random high port. That is expected —
peers learn each other's actual port from the broker, so no per-instance INI edit is needed.
Set `[party] udp_port = 0` to always use an ephemeral port.

Leave `[party] advertise_ip` blank. Each shim advertises a routable address (the NIC used to
reach the broker when it is not loopback, otherwise a UDP connect probe, otherwise
`127.0.0.1`), and the broker rewrites a peer's loopback registration to its real address when
it can.

Windows Firewall: nothing is needed for same-PC instances. For play across PCs, run
`open_firewall.ps1` as Administrator, or allow **TCP 8080+8081** on the host and **UDP 27015**
on each client.

## Mixing same-PC and cross-PC

- Secondary PCs install with `install.ps1 -GameDir <path>` and set
  `[server] host = <host PC's LAN IP>` in their `lan.ini` (or launch with
  `GBFR_LAN_STUB=<host>:8080`).
- Only the hosting machine runs `gbfr-lan-server.exe`; two brokers on the same LAN give
  clients different lobby lists.
- With 2+ players per PC, the handler's per-instance Steam IDs are what keep the members
  distinct from the other PC's players.

## Troubleshooting

| Symptom | Check |
|---|---|
| Second instance cannot reach the broker | only player 1 runs the server; check `gbfr-lan-server.log` in the player-1 instance folder; stale servers are killed by the handler at launch |
| Both local players act as one | `configs.user.ini account_steamid` is distinct per instance; broker log `LoginWithSteam PlayfabId=…` should show two IDs |
| Mesh drops when both instances use 27015 | expected fallback to ephemeral; check `bind_udp fallback ephemeral` in the second instance's `party_shim.log` |
| Instances see stale shims after an update | re-run `install_nucleus.ps1` and **reload the handler**, then delete existing instance folders so the reset copies the new package |
| Goldberg blocks a non-LAN connection | `steam_settings\disable_lan_only.txt` exists in the handler/instance folder |
| One instance writes logs where another expects them | logs live in each instance's game folder (separate `steam_http_shim.log`, `playfab_mp_shim.log`, `party_shim.log`) |

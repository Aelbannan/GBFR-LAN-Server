# GBFR LAN

LAN / offline co-op for **Granblue Fantasy: Relink**. This replaces the game's online
services — Cygames HTTP/WebSocket, PlayFab lobbies, and PlayFab Party — with local
implementations, so you can host and join quests over your own network or entirely offline
on one PC.

- No TLS, no hosts file edits, no Administrator rights.
- The server only brokers lobbies and peer discovery. Quest bytes go directly between
  clients over UDP.
- Works across PCs on a LAN, and with multiple Nucleus Co-op instances on one PC.

> Requires your own copy of the game, a Steam emulator (Goldberg) that can load extra
> DLLs, and a Rust toolchain to build. No game files are included.

## Components

| Folder | Output | Role |
|---|---|---|
| `steam-http-shim` | `gbfr_http.dll` | ISteamHTTP + WinHTTP shim. Redirects Steam HTTP and WinHTTP requests to the LAN server and completes Steam API call results. Loaded through Goldberg's `steam_settings\load_dlls`. |
| `playfab-mp-shim` | `PlayFabMultiplayerWin.dll` | Drop-in replacement for `PlayFabMultiplayerWin.dll`. Lobbies are stored on the LAN server over HTTP instead of PlayFab. |
| `party-shim` | `PartyWin.dll` | Drop-in replacement for `PartyWin.dll`. Replaces the Azure Party mesh with direct UDP between clients. |
| `lan-server` | `gbfr-lan-server.exe` | The broker: Cygames HTTP/WS endpoints, PlayFab lobby REST, and the Party peer list. |

`common/lan_cfg.rs` holds the shared `lan.ini` / `GBFR_LAN_STUB` parsing used by the
shims and the server.

Ports: **TCP 8080** (HTTP), **TCP 8081** (Cygames WebSocket), **UDP 27015** (Party mesh,
configurable).

## Requirements

- Windows 10/11, x64
- *Granblue Fantasy: Relink* (Steam), current patch
- Goldberg Steam emulator configured so `steam_settings\load_dlls\*.dll` is loaded into
  the game process
- [Rust](https://rustup.rs) with the MSVC toolchain (to build from source)

## Build

From the repo root:

```powershell
powershell -ExecutionPolicy Bypass -File build.ps1
```

Result:

```
steam-http-shim\gbfr_http.dll
party-shim\PartyWin.dll
playfab-mp-shim\PlayFabMultiplayerWin.dll
lan-server\gbfr-lan-server.exe
```

Each component can also be built on its own with its `build.ps1`. The shims only need
`rustc`; the server builds with Cargo.

## Install

Close the game first. The installer builds everything and copies it into an existing
game folder (`-GameDir` must contain `granblue_fantasy_relink.exe`):

```powershell
powershell -ExecutionPolicy Bypass -File install.ps1 -GameDir "C:\Path\To\Granblue Fantasy Relink"
```

It performs these copies:

1. `gbfr_http.dll` → `<game>\steam_settings\load_dlls\`
2. `PartyWin.dll` → `<game>\` (original backed up to `PartyWin.dll.ms` once)
3. `PlayFabMultiplayerWin.dll` → `<game>\` (original backed up to `PlayFabMultiplayerWin.dll.ms` once)
4. `lan.ini` → `<game>\` (an existing file is left untouched)
5. `gbfr-lan-server.exe` → `<game>\` (only the host needs to run it)

Re-run with `-SkipBuild` to reinstall already-built binaries.

If the Goldberg build blocks `connect()` to non-LAN addresses (its experimental
`disable_lan_only` hook), create an empty `steam_settings\disable_lan_only.txt` in the game
folder.

### Nucleus Co-op

```powershell
powershell -ExecutionPolicy Bypass -File install_nucleus.ps1
```

Copies the shims, server, and `lan.ini` into
`C:\NucleusCoop\handlers\Granblue Fantasy Relink` (override with `-HandlerDir`). Reload
the handler afterwards; player 1 starts `gbfr-lan-server.exe`.

## Configure

`lan.ini` sits next to the game exe. Every client needs it; the host PC's address goes in
`[server] host`.

```ini
[server]
host = 127.0.0.1   ; host PC's LAN IP, e.g. 192.168.1.10, for other PCs
port = 8080
ws_port = 8081

[lobby]
max_players = 8          ; 2-32
override_game_max = true ; ignore the game's MaxPlayers (shipping build sends 4)

[party]
udp_port = 27015         ; 0 = random high port
advertise_ip =           ; blank = auto-detect; set this PC's LAN IP if needed
```

Same PC / Nucleus instances: keep `127.0.0.1`. The `GBFR_LAN_STUB=host:port` environment
variable overrides `[server]` for one process.

## Run

1. On the host PC, run `<game>\gbfr-lan-server.exe` (or `lan-server\gbfr-lan-server.exe`
   from a build). It logs to `gbfr-lan-server.log`.
2. On every PC, set `lan.ini` and launch the game normally.
3. At the multiplayer quest counter, create a lobby (host), then find and join it
   (guests).
4. Start a quest — Party traffic flows directly between clients over UDP.

Windows Firewall: the hosting PC must accept TCP 8080/8081, and clients need UDP 27015.
Run `open_firewall.ps1` as Administrator on each PC to add the rules.

Server flags: `--http-port N`, `--ws-port N`, `--max-players N`, `--title-id X`,
`--ini <path>`. The title id defaults to the game's boot-config value (`1AC1AD`).

## Troubleshooting

Logs are written next to the game exe:

| File | Written by |
|---|---|
| `steam_http_shim.log` | `gbfr_http.dll` — URL rewrites and Steam call completion |
| `playfab_mp_shim.log` | `PlayFabMultiplayerWin.dll` — lobby API calls |
| `party_shim.log` | `PartyWin.dll` — party state changes and UDP traffic |
| `gbfr-lan-server.log` | the server — HTTP/WS requests, lobbies, peers |

- **Stuck connecting at boot:** the client is not reaching `[server] host`. Check
  `playfab_mp_shim.log` and the server log.
- **Lobby created but guests see nothing / cannot join:** check `[server] host`, Windows
  Firewall, and `[lobby] max_players`.
- **Quest starts then hangs:** compare both `party_shim.log` files. Every send/recv is
  logged as `opcode=` + `len=`; after quest start both clients should show opcode `2`
  (type 21 on receive). Set `GBFR_PARTY_LOG_HEX=1` for full payload hex dumps.
- **A game update broke something:** parts of the shims patch the exe or track offsets,
  so updates can break them. The shim logs show the first step that failed.

Debug environment variables: `GBFR_LAN_STUB=host:port` (override server),
`GBFR_PARTY_LOG_HEX=1` (hex dump Party payloads), `GBFR_PARTY_FORCE_INLINE=1` (disable
Party worker threads), `GBFR_LAN_INI=<path>` (server's ini path).

## Tests

Reliable-delivery logic (pure, no DLL needed):

```powershell
cd party-shim
rustc -O -o reliable_test.exe reliable_test.rs
.\reliable_test.exe
```

Broker/transport integration for `PartyWin.dll` (mock broker runs in-process; build the
shim first and put `PartyWin.dll` next to the test exe):

```powershell
cd party-shim
rustc --edition 2021 -O -o broker_http_test.exe broker_http_test.rs
.\broker_http_test.exe            # full threaded flow
.\broker_http_test.exe --loss     # retransmit/ack over loss
.\broker_http_test.exe --outage   # broker silently down, then recovery
```

PlayFab lobby smoke test (start the broker on a test port first):

```powershell
lan-server\gbfr-lan-server.exe --http-port 18080 --ws-port 18081
$env:GBFR_LAN_STUB = "127.0.0.1:18080"
cd playfab-mp-shim
rustc tests\postupdate_smoke.rs -o tests\postupdate_smoke.exe
tests\postupdate_smoke.exe PlayFabMultiplayerWin.dll
```

## How it works

- `gbfr_http.dll` replaces the ISteamHTTP vtable after `SteamAPI_Init` and hooks
  `WinHttpOpenRequest` so `https://…/…` requests become `http://<lan.ini host>:<port>/…`.
  The secure flag is stripped so the plaintext connection succeeds; the Cygames WebSocket
  keeps port 8081.
- The game's boot config (`/dat/config/*.blob`) is served by the server as a
  ChaCha20-Poly1305 (IETF) blob.
- PlayFab lobby APIs are implemented as HTTP endpoints; the lobby list lives only in
  `gbfr-lan-server.exe`'s memory.
- `PartyWin.dll` publishes an opaque `network_descriptor` containing the host's UDP
  address through the lobby. Clients then exchange `PartyEndpointSendMessage` payloads
  directly, with a small reliable-delivery layer (acks/retransmits/ordering) so quest sync
  survives packet loss.

## Legal

- For use with legally obtained copies of the game. Offline/LAN play only — do not point
  this at live services.
- No game assets or third-party SDK binaries are included. The shims are independent
  replacements written against the interfaces the game imports.
- Modifying a game can violate its EULA/ToS. Use at your own risk; the authors accept no
  responsibility for bans, damage, or data loss.

## License

Not chosen yet. Add a `LICENSE` file before publishing if you want to grant usage terms.

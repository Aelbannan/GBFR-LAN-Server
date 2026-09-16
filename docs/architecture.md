# Architecture

Relink's shipped co-op uses three internet services: **Cygames HTTP/WebSocket** (login
gate, invites/presence), **PlayFab** (Steam login, entity token, Multiplayer Lobby), and
**PlayFab Party** (the session mesh that actually carries quest bytes). This repo replaces
all three with local implementations so host/join/quest works on a LAN or entirely offline.

- Plain HTTP/WebSocket/UDP only: no TLS, no hosts-file edits, no Administrator.
- The broker (`gbfr-lan-server.exe`) only brokers **lobbies and peer discovery**. Quest
  bytes go directly between clients over UDP.
- Everything the client sends is redirected by the shims (`gbfr_http.dll`,
  `PlayFabMultiplayerWin.dll`, `PartyWin.dll`); the game exe is never patched.

## Stock service flow (live play)

The exe has no LAN/offline path for online co-op: the flow below is what a live session does,
and it is what this stack impersonates end to end. The exe-side state machines (lobby →
mesh → quest) are identical in both cases; only the responses differ. Steam is auth + HTTP
+ overlay/DLC in this build — the old `SteamNetworkingSockets`/ISteamMatchmaking session
path is gone, replaced by PlayFab Party.

**Boot.** The exe fetches its boot config from `conf-f3btg-relink.granbluefantasy.jp`
(path `dat/config`, extension `blob`, config key `config-relink`) over Steam HTTP. The file
is a base64 ChaCha20-Poly1305 (IETF) blob with key `kdfg8kojildksuie23jsdfg8fg7klsdx`; the
decrypted JSON supplies `PlayfabTitleId`, `GameapiUrl`, `WebsocketUrl` and the matching
timers — the Cygames/PlayFab endpoints the exe uses come from here (the Party/Azure
endpoints live inside `PartyWin.dll`). Serving your own blob is therefore enough to redirect
the HTTP service layer.

**Steam identity.** `NetworkCheckerSteam` waits for `SteamServersConnected_t`;
`HttpAccessorSteam` sends Cygames HTTP over ISteamHTTP. The auth identity is the string
`server_auth_key_001`, fed by the Steamworks `GetTicketForWebApiResponse_t` callback.
PlayFab's `LoginWithSteam` consumes a second Steam ticket.

**Cygames HTTP.** Requests carry `App-Info` and `Signature` (timestamp + HMAC-SHA256 with
key `9xcleSD8efksogsz`); the live service rejects a missing/expired signature with 403.
Auth is `POST sys/user_auth` with `token`, `entity_id`, `save_slot_hash`, `save_slot_num`,
`playfab_id`, `playfab_title_id`, `enable_wing`; success is `meta.error_code == 0` plus
`data.auth_token` / `ws_api_key` / `ws_url` / `cygames_id_linked`. Other endpoints in the
surface: `sys/get_terms`, `sys/get_news`, `activity/get_invite_list`,
`activity/get_presence_list`, `cygames_id/*`, `playlog`.

**Cygames WebSocket.** The exe upgrades the `ws_url` from `user_auth` and sends **binary**
frames (`WinHttpWebSocketSend` opcode 2) whose payload is JSON text — e.g. the guest's
`{"command":"join_lobby","params":{"lobby_search_id":"…"}}`. Replies are correlated by
command-name echo, not by a sequence number; the receive parser requires a root `command`
string and a root `params` object.

**PlayFab login.** PC login is only `PFAuthenticationLoginWithSteamAsync`
(`createAccount=1`). The real endpoint is
`POST https://{PlayfabTitleId}.playfabapi.com/Client/LoginWithSteam`; the live service
validates the Steam ticket with Steam and returns `PlayFabId`, `SessionTicket` and an entity
token of type `title_player_account`. The exe then calls `PFEntityGetEntityToken` and
`PFMultiplayerSetEntityToken`.

**PlayFab lobby.** The genuine `PlayFabMultiplayerWin.dll` speaks the Lobby service
(`/Lobby/*`, `X-EntityToken` header). Lobby objects are **server-owned**: create/join/post
are applied by the service, and every member — including the author of a change — learns
about it through SignalR / Web PubSub notifications on `<titleId>.playfabapi.com/pubsub`,
not by polling. `FindLobbies` filters, sorts and paginates server-side using the
`string_keyN` / `number_keyN` convention; `OwnerMigrationPolicy`, `MembershipLock`,
`AccessPolicy` and the member connection states are service semantics. `network_descriptor`
and `invitation_identifier` are ordinary lobby properties carrying the Party hand-off.

**PlayFab Party.** The genuine DLL is Azure PlayFab Party 1.10. `PartyCreateNewNetwork`
gets `maxUserCount=4`, `maxDeviceCount=4`, `maxUsersPerDevice=1`, `maxDevicesPerUser=1`;
`directPeerConnectivityOptions=0xF` is `AnyPlatformType | AnyEntityLoginProvider`
(best-effort direct P2P with an Azure relay fallback) — there is **no LAN flag**. The
network is allocated through `/Party/RequestParty`, QoS via
`/MultiplayerServer/ListPartyQosServers`, speech tokens via
`/MultiplayerServer/GetCognitiveServicesToken`. The mesh itself is UDP (`WS2_32`) wrapped in
DTLS (`SspiCli`) with self-signed certs, falling back to Azure relays. The state-change enum
is the same one the shim pumps (the shim emits the subset the exe switches on).

**Why hosts-only redirection cannot work.** Live PlayFab validates Steam tickets, so
Goldberg tickets fail at login; the Cygames HTTP path expects TLS and the HMAC signature;
and Party needs Azure relay allocation unless a direct path is negotiated, which only the
live service can arrange. That is why this stack replaces the DLLs and serves the whole
surface locally instead of redirecting to the live endpoints.

## Components

| Folder | Output | Role |
|---|---|---|
| `steam-http-shim` | `gbfr_http.dll` | ISteamHTTP + WinHTTP shim. Loaded through Goldberg's `steam_settings\load_dlls`. Redirects Steam HTTP and WinHTTP to the broker, completes Steam call results. |
| `playfab-mp-shim` | `PlayFabMultiplayerWin.dll` | Drop-in replacement. The 20 lobby APIs the exe imports; lobbies live on the broker over HTTP. |
| `party-shim` | `PartyWin.dll` | Drop-in replacement. The 21 Party APIs the exe imports; replaces Azure Party with direct UDP + a small reliable-delivery layer. |
| `lan-server` | `gbfr-lan-server.exe` | The broker: Cygames endpoints, PlayFab lobby REST, Party peer registry, WebSocket. |

Shared config parsing (`lan.ini` / `GBFR_LAN_STUB`) lives in `common/lan_cfg.rs`.

## Local request flows (this stack)

### 1. Boot config

The game reads `dat/config/<hmac>.blob` through Steam HTTP / WinHTTP, which the HTTP shim
rewrites to `http://<host>:<port>/…`. The broker answers `GET /dat/config/*.blob` (any path
matching `.json`/`.dat`/`.blob`/`config` is answered with the same config) with a
base64 ChaCha20-Poly1305 (IETF) blob:

- key `kdfg8kojildksuie23jsdfg8fg7klsdx`
- 12-byte nonce = unix-seconds LE (8 bytes) followed by its first 4 bytes
- output = base64(nonce ‖ ciphertext ‖ 16-byte tag)

The decrypted JSON advertises `PlayfabTitleId` (default `1AC1AD`), `GameapiUrl`,
`WebsocketUrl` and the matching timers (`MatchingSearchWaitTimeMinSec = 0`,
`MatchingSearchWaitTimeMaxSec = 3600`). The exe's join work-item pump has its own 30 s
budget (`work+0xb8 = 30000 ms`) — see `game-integration.md`.

### 2. Login

1. `POST /Client/LoginWithSteam` → local `PlayFabId`, `SessionTicket`, entity token.
   Identity is derived from the Steam ticket (SHA-1), not from PlayFab.
2. `POST /sys/user_auth` (Cygames) → `auth_token`, `ws_url` (`ws://<host>:8081/`),
   `ws_api_key`, `cygames_id_linked: false`.
3. The Cygames WebSocket upgrade is accepted on **8081**. Binary JSON frames are parsed and
   echoed back as `{"command": …, "params": {…}}` — enough for the exe's `join_lobby` ACK.

### 3. Lobby

`CreateAndJoinLobby` / `JoinLobby` / `FindLobbies` / `GetLobby` / `UpdateLobby` /
`LeaveLobby` all hit `POST /Lobby/*` on the broker. The lobby list lives only in the broker
process memory; connection strings are `lan.<title_id>.lan-<12-hex-id>`.

While the lobby is live, `PFMultiplayerStartProcessingLobbyStateChanges` polls
`GetLobby` every 250 ms and queues the state changes the exe consumes as the roster changes
(`MemberAdded`, `Updated`, `MemberRemoved`, `Disconnected`, …).

### 4. Party mesh

The Party shim builds a descriptor UUID per network and publishes it through the lobby
property `network_descriptor` as `LAN1.<uuid>.<8-hex-ipv4>`. The guest deserializes it,
connects, and learns its peers from the broker (`/party/join`, `/party/peers`), then
confirms them directly over UDP (`GBFR` HELLO datagrams). All details: `party-transport.md`.

### 5. Quest traffic

`PartyEndpointSendMessage` payloads become UDP datagrams between the players' endpoints.
The broker never sees them. Reliability (retransmit/ack/ordering) is implemented in
`party-shim/src/reliable.rs` because the exe relies on `GuaranteedDelivery` for its
one-shot control RPCs.

## Configuration

`lan.ini` sits next to the game exe (and next to `gbfr-lan-server.exe`; the server also
honors `--ini` / `GBFR_LAN_INI`):

```ini
[server]
host = 127.0.0.1   ; host PC's LAN IP for other PCs
port = 8080        ; HTTP
ws_port = 8081     ; Cygames WebSocket

[lobby]
max_players = 8          ; 2..32; also the /party/join cap
                         ; (the Party shim itself tracks at most 8 remotes)
override_game_max = true ; ignore the game's MaxPlayers (shipping build sends 4)

[party]
udp_port = 27015         ; 0 = ephemeral
advertise_ip =           ; blank = auto-detect; set the LAN IPv4 if needed
```

Environment variables: `GBFR_LAN_STUB=host:port` (client override of `[server]`),
`GBFR_LAN_INI=<path>`, `GBFR_PARTY_LOG_HEX=1` (hex-dump Party payloads),
`GBFR_PARTY_FORCE_INLINE=1` (disable the shim's worker threads for debugging).

Server flags: `--http-port N`, `--ws-port N`, `--max-players N`, `--title-id X`,
`--ini <path>`, `--decode-blob`.

Ports: **TCP 8080** HTTP, **TCP 8081** Cygames WebSocket, **UDP 27015** Party mesh.

## Broker endpoint reference

All HTTP requests are `Connection: close`; the broker adds `Access-Control-Allow-Origin: *`.
Errors carry a real HTTP status where possible and a PlayFab-style body
(`{"code":404,"status":"NotFound","error":"LobbyNotFound",…}`).

### Cygames (also matched without the `sys/` prefix)

| Path | Response `data` |
|---|---|
| `sys/get_terms` | `{"pp_version":1,"playlog":1,"sc":0}` |
| `sys/get_news` | `{"list_size":0,"list":[]}` |
| `sys/user_auth`, `sys/external_user_auth` | `auth_token`, `ws_url`, `ws_api_key`, `cygames_id_linked:false` |
| `activity/get_invite_list`, `activity/get_presence_list` | `{}` (empty lists are skip-safe) |
| `cygames_id/link_status`, `cygames_id/link`, `cygames_id/unlink` | `{"is_pending":false,"link_status":0}` |
| `cygames_id/check_reward` | `{"has_link_reward":false,"has_cross_reward":false}` |
| `playlog*` | `{}` |
| `GET /dat/config/*.blob` (and any `.json`/`.dat`/`config` GET) | boot config (blob when the path ends `.blob`) |

Envelope: `{"meta":{"error_code":0},"common":[],"data":{…},"responseCode":0}`.

### PlayFab lobby REST (local subset)

| Path | Notes |
|---|---|
| `POST /Client/LoginWithSteam` | returns `SessionTicket`, `PlayFabId`, entity token |
| `*/GetEntityToken` | returns the session's entity token |
| `POST /Lobby/CreateAndJoinLobby` | owner row created here; `MaxPlayers` passed through `[lobby]` policy |
| `POST /Lobby/JoinLobby` | by `ConnectionString`; `LobbyNotFound` / `LobbyMemberLimitExceeded` (409) on failure |
| `POST /Lobby/FindLobbies` | evaluates the exe's filter/sort (see below) |
| `POST /Lobby/GetLobby` | full lobby: `LobbyId`, `ConnectionString`, `Owner`, `MaxPlayers`, `CurrentPlayers`, `MembershipLock`, `AccessPolicy`, `LobbyData`, `SearchData`, `Members[]` |
| `POST /Lobby/UpdateLobby` | merges `LobbyData`/`SearchData`/`MemberData`; `*ToDelete` arrays remove keys |
| `POST /Lobby/LeaveLobby` | the lobby closes when its last member leaves. An owner leaving applies the create-time `OwnerMigrationPolicy`: `Automatic` (1) hands ownership to another member, `None`/`Manual` clears the owner (the lobby stays ownerless) |
| any other `Client/*`, `Lobby/*` | empty PlayFab OK |

`Members[].MemberData` is normalized with `member_platform` (`"Steam"`),
`member_platform_account_id` (decimal, derived from the entity id) and
`member_platform_user_name` (`"LAN"`) when the game did not supply them. The exe's platform
check accepts the string `Steam`, not the enum value `1` (`fill_member_data`).

### Party peer registry

| Path | Notes |
|---|---|
| `POST /party/join` (alias `/party/create`) | upsert by `(network_id, entity_id)`; refreshes `seen`; refuse at cap with `PartyMemberLimitExceeded`; a stale `member_seq` is ignored |
| `GET /party/peers?network_id=…[&entity_id=…]` | member list `{entity_id, ip, udp_port, seen, member_seq}`. With `entity_id`, refreshes that member (a polling peer is alive even if its 10 s keep-alive is late). Retains members for a 30 s `seen` TTL |
| `POST /party/leave` | idempotent; a stale `member_seq` cannot delete a fresher join |
| `POST /party/msg` | accepted and ignored (dead endpoint) |

The party registry is what the shims use for peer discovery; it is keyed by the network id
embedded in the `LAN1.<uuid>.<ip>` descriptor.

### WebSocket (port 8081)

The broker accepts the upgrade, reads binary frames, and replies with
`{"command": <same>, "params": <same object>}`, hoisting a root-level `lobby_search_id` into
`params` if needed. A frame containing `join_lobby` gets that reply even if it does not parse
as JSON. There is no authentication and no server-initiated push.

## Lobby data model

- **Custom property keys (16)**: `leader_name`, `leader_platform`, `lobby_password` (empty
  means the `5555555555` sentinel), `allow_spoiler`, `strength`, `quest_rank`, `grade`,
  `purpose`, `play_style`, `chara_id`, `costume_number`, `comment1`, `comment2`, `region`,
  `join_limit_grade`, `network_version`.
- **Extra property keys** the exe reads: `network_descriptor` (the Party descriptor) and
  `invitation_identifier`. The broker stores all values verbatim.
- **Member properties**: `member_platform`, `member_platform_account_id`,
  `member_platform_user_name`.
- **Search-data convention**: the PlayFab service mandates `string_keyN`/`number_keyN`
  (`N` = 1..30); this convention is a *service* rule, so the game uses it directly. The
  exe's create path maps its named properties onto them (inferred parallel-array mapping:
  `number_key1..10` = strength/quest_rank/…/region, `number_key21` = join_limit_grade).
- **`id_container`**: a serialized lobby-property blob the peers use to exchange member
  slots/ordinals. The broker treats it as opaque and never allocates slots.
- **`network_descriptor`**: `LAN1.<36-char uuid>.<8-hex ipv4>`, opaque to the broker except
  that it parses the `LAN1.` prefix to key the party registry.

## FindLobbies filter and sort

The exe builds a real OData-like `filterString` (and passes an empty `sortString`). The
shim forwards `Filter`, `Sort`, `FriendsFilter` and `ClientSearchResultCount` verbatim; the
broker evaluates the documented subset:

- clauses joined by `and`; operators `eq ne le lt ge gt`;
- `string_keyN`/`number_keyN` (numeric values are floats);
- predefined keys `lobby/memberCount`, `lobby/maxMemberCount`,
  `lobby/memberCountRemaining`, `lobby/membershipLock`, `lobby/amOwner`, `lobby/amMember`,
  `lobby/amServer`, plus `lobbyId` as a broker extension;
- sort by any of those keys, ascending/descending, plus `distance{number_keyN = V}`.

Anything outside the subset is **reported**, never silently dropped: the broker logs it and
adds a `Warnings` array to the response, and an unsupported filter fails closed (no rows).
`FriendsFilter` is likewise reported and fails closed (the broker has no friend graph).

## Identity

`player_from_ticket` derives `PlayFabId = SHA1(ticket)[0..16]` (upper-case) and
`entity_id = SHA1("tpa:<playfab_id>")[0..16]`. Consecutive logins with the same ticket are
stable per machine, but an empty/unknown ticket falls back to one shared identity, and two
PCs with the same ticket/account produce the same identity. For LAN use this is normally
fine (each Goldberg instance has its own Steam ID); a collision would make members dedupe
and the mesh collapse to one peer.

## Server internals and limits

- One `Arc<Mutex<App>>` state; one thread per HTTP/WS connection; no background thread.
- Locking is poison-tolerant (`unwrap_or_else(into_inner)`); the whole state lock is never
  held across I/O.
- Lobbies have no TTL; they live until leave/owner-leave or process exit. Party rows expire
  after 30 s without a `/party/join` refresh unless refreshed by `/party/peers`.
- Logs: `gbfr-lan-server.log` next to the server exe (`steam_http_shim.log`,
  `playfab_mp_shim.log`, `party_shim.log` for the clients). The broker log includes a
  normalised request path for every call and a `catch-all POST …` line for unknown POSTs.

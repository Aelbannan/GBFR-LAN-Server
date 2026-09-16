# Diagnostics

What to read when a session fails, and what the log lines mean. All four logs are plain text
and append-only for the life of the process.

| File | Written by | Format |
|---|---|---|
| `steam_http_shim.log` | `gbfr_http.dll` (game dir) | `[gbfr-http] …` |
| `playfab_mp_shim.log` | `PlayFabMultiplayerWin.dll` (game dir) | `[epoch] …` |
| `party_shim.log` | `PartyWin.dll` (game dir) | `[epoch] …` |
| `gbfr-lan-server.log` | `gbfr-lan-server.exe` (next to the server) | `[HH:MM:SS] …` (UTC) |

Nucleus: each instance is a separate game folder, so each writes its own three logs; the
broker log is in player 1's instance folder.

## Healthy run, stage by stage

1. **Load / hooks** (`steam_http_shim.log`):
   `[gbfr-http] loaded version.dll proxy`, `IAT …`, `hooked WinHttp* in …`,
   `waiting for SteamAPI_Init`, `patched ISteamHTTP at …`.
2. **Steam** — `dispatched SteamServersConnected_t`, `GetAuthTicketForWebApi '…' stub=1
   ticket=LANSTUB|<steamid>|<pc>`.
3. **Boot config** — `CreateHTTPRequest … /dat/config/<hash>.blob` followed by
   `GET … -> 200 (848 bytes)`; then `PFServiceConfigCreateHandle ep=https://<title>.playfabapi.com`;
   `WinHttpConnect <realhost>:443 -> <host>:8080`.
4. **Cygames** — `GET …/sys/get_terms -> 200`, `…/sys/user_auth`, `…/activity/get_invite_list`;
   broker log shows the same paths.
5. **WebSocket** — server `websocket connected`, `ws command=join_lobby`, `ws reply command=join_lobby`.
6. **PlayFab login** — `PFMultiplayerInitialize title=1AC1AD`, `SetEntityToken len=…`,
   `pfqueue hb …` heartbeats.
7. **Lobby** — `CreateAndJoinLobby max=4 …` (host) or `FindLobbies …` / `JoinLobby …` (guest),
   `MemberAdded entity=…`, `GetLobbyProperty …`, `GetMembershipLock value=…`.
8. **Mesh** — `PartyInitialize`, `PartyCreateNewNetwork id=… udp=… advert=…` (host) or
   `Deserialize …` + `PartyConnectToNetwork` (guest), `AuthenticateLocalUser`,
   `CreateEndpoint uid=…`, `EndpointCreated remote entity=… uid=… ip=…:…`, then
   `type21 finished opcode=… sub=…`.
9. **Ready / quest** — `rows=[id=… +70=1 … +a0=4]` on both peers, `queue=[…:3:t1:s0]`,
   `msg_stats … reliable guar-seq(…)`, and (after quest start) `asyncload … v=loading/completed`.

## PlayFab shim lines

| Pattern | Meaning |
|---|---|
| `pfqueue queue type=N pending=M` | a state change was queued (type = PlayFab change type) |
| `pfqueue start n=… pending=… inflight=…` | `StartProcessing` handed out a batch |
| `pfqueue finish reclaimed=… pending=…` | `FinishProcessing` returned a batch |
| `pfqueue hb pending=… inflight=… outstanding=… starts=… finishes=… batches_nonzero=… queued=… reclaimed=… max_pending=… max_inflight=… latch/stray/mismatch=…` | 1 Hz health: `latch/stray/mismatch` non-zero means bookkeeping broke |
| `CreateAndJoinLobby max=… owner_policy=… lobby_props=… search=… member_props=…` | create request contents |
| `FindLobbies filter="…" sort="…" count=… friends=…` | the exe's search configuration as forwarded |
| `FindLobbies n=…` + `FindLobbies row …` | result rows (and their search keys) |
| `FindLobbies drop id=… no LAN1 descriptor yet` | a row is hidden until the host publishes the descriptor |
| `MemberAdded entity=…` / `MemberRemoved entity=…` | roster changes |
| `LobbyDisconnected id=… (broker returned non-200)` | terminal; type 10 queued |
| `GetLobbyProperty k=v` / `GetSearchProperty k=v` / `GetMemberProperty …` | each key the exe reads (the best census of what the exe consumes) |
| `GetMembershipLock value=… (reached via Updated membershipLockUpdated)` | proof the type-7 lock flag was consumed |
| `CreateAndJoinLobby failed code=0x…`, `JoinLobby failed … code=0x…` | genuine `E_PF_*` returned to the exe |

## Party shim lines

**Lifecycle**

- `PartyInitialize title_id=… handle_out=…`
- `PartyCreateNewNetwork id=<uuid> udp=<port> advert=<ip>`; `Serialize LAN1.<uuid>.<ip>`;
  `Deserialize raw=LAN1.…` (or `Deserialize ignore (not LAN1) raw=… (out descriptor cleared)`).
- `PartyConnectToNetwork id=… udp=… (state_change type=3 follows)`
- `WATCH_4 AuthenticateLocalUser entity=…` (the exe still passes `maxUserCount=4`),
  `CreateEndpoint uid=… entity=…`
- `EndpointCreated remote entity=… uid=… ip=…:…`; holds appear as
  `ensure_remote hold type12 entity=… until type10 delivered` or
  `… until native FUN_140260b30 insert`.
- `PartyNetworkLeaveNetwork` + `LeaveNetwork stack rva=…` (who in the exe tore down).

**Transport / reliability**

- `transport thread started tid=… poll_ms=2 …` / `broker thread started tid=… owns=/party/join,/party/leave,/party/peers`
  / `solo sampler thread started … read_only=1`.
- `transport[heartbeat] tid=… up=… wakeups=… jobs=… datagrams_rx=… datagrams_tx=… outbox_depth=… outbox_hwm=…`
  — `up=false` means the tick fallback is in use.
- `broker[jobs=… up=… queue=…]` — broker-thread queue depth and liveness.
- `msg_stats send=… recv=… send_ops=<op>:<n>,… recv_ops=… reliable guar-seq(sent/retransmitted/acked/dup-dropped/out-of-order-dropped/out-of-order-queued/evicted) best-seq(…) probes_run/skipped`
  — the primary traffic/tuning view.
- `recv gap …`, `first retransmit …`, `retry_gaveup …`, `reliable pending evict …`, `send options word …`,
  `remote endpoint moved entity=… old=… new=…`.
- `delivery ledger[heartbeat] pending=… in_flight=… batches=… queued=[types] out=[types] back=[types] deferred21=… cap_hits=… dropped=… type12_hold=<net> remotes=… held_ms=…`
  — state-change queue view. `deferred21` = inbound type-21s waiting for the remote type 12;
  `type12_hold` non-`none` = the exe's member row/type-10 has not landed yet.

**Game-memory probes** (read-only; safe to ignore unless debugging a stall)

- `rows=[id=<ordinal> +8=<serial> +70=<0|1> +a0=<work state>; …]` — the exe's member work
  items. `+70=1` means the item is connectable (only the game's matchmaking writes it);
  `+a0` is the connect state (`1` pending, `2` skipped, `3` waiting, `4` success, `6` fail).
- `queue=0x… fl=… [<key>:<state>:t<t#>:s<s#>; …]` — the exe's 0x180-stride ready/queue rows
  (key = work serial; state; `t` = `+0x10` latch; `s` = `+0x40`).
- `asyncload h=0xa596af73 mgr=… m1=… q=<hits>/<total>[/m<mode>] qmode=… va=… va2=… vatask=…
  v=queued|registered|loading|completed|waited|none` — the quest loader's async request.
  A permanent `v=queued` with `qmode=3` is the known quest-start stall.
- `mode3gate rm=… arm3=… done3=… skip3=… jobs=… qm=… sel=… obs=<o0>/<o1> exp=<e0>/<e1>
  canmatch=… online=…` — mode-3 resource arming and the `CanMatchingSetting` ordinal pair.
- `solo t=… upd_phase=… jl_gate=… qmsel=… qm_obs=… qm_exp=… canmatch=… arm3=… done3=… skip3=…
  m1=… q=… qmode=… v=… mat=… m50=… quest fields …` — the standalone sampler. It runs without a
  shim network and is the only probe for a solo/offline session; see `game-integration.md`
  for the token meanings.
- `quest_guard …`, `quest_sync …`, `quest_array …`, `quest_action …` — transitional probes;
  the array probe reads the wrong object (documented in `ORDINAL_CHAIN_AUDIT`) and prints noise.

## Broker log lines

Every request logs its method, `Host` header and **normalised** path, e.g.
`POST host=192.168.1.10:8080 path=/Lobby/JoinLobby`. Interesting ones:

- `party join network=… entity=… udp=… members=…` — peer registration (every 10 s per client).
- `party join refused network=… members=… max=…` — `[lobby] max_players` reached.
- `party leave …` / `party join stale ignored …` — leave and the stale-`member_seq` guard.
- `FindLobbies n=… ids=[…] filter="…" warnings=[…] sk5=[…]` — the actual evaluated search.
- `find_owner_stale:…` — informational: the owner's party row lapsed but the row is kept so
  `JoinLobby` can decide.
- `CreateLobby id=… search_keys=[…] lobby_keys=[…]`, `JoinLobby id=… members=…`,
  `LeaveLobby … closed=… owner_left=…`.
- `catch-all POST … — no handler, empty PlayFab OK` — a POST hit no route; if it was a real
  endpoint, the routing changed.
- `websocket connected`, `ws command=…`, `ws reply command=…`, `websocket closed`.
- `boot .blob <n> json -> <m> b64`.

## Symptom → first checks

| Symptom | Likely cause | Look for |
|---|---|---|
| Stuck connecting at boot | client not reaching the broker / Steam emulation off | `steam_http_shim.log`: no `CreateHTTPRequest` or connection refused; Goldberg `Offline=0`/`BlockConnection=0`; `lan.ini [server] host`; server running (`/health`) |
| Host creates a lobby, guests see none | filter dropped/unsupported, host stale, firewall | broker `FindLobbies … warnings=[]`; `party join` cadence; `[server] host` + firewall; `max_players` |
| Guest shows its own lobby / wrong lobby | search configuration not evaluated | `FindLobbies filter=…` in `playfab_mp_shim.log`; broker `warnings=[…]` |
| Join fails with −1C/−14C | exe connect gate on a `+0x70=0` work item, or WS `join_lobby` not ACKed | `rows=`/`mode0` in `party_shim.log`; server `ws command=join_lobby` + `ws reply`; `game-integration.md` |
| Remote peer never appears | type 12 held for the exe's member row / local type 10 | `ensure_remote hold type12 …`; ledger `type12_hold=…`; member-list insert |
| Quest starts then hangs | mode-3 `LoadAsset` arming never ran; loader request stuck | `asyncload … v=queued` with `qmode=3`, `mode3gate arm3=0`; open thread in `game-integration.md` |
| Quest traffic stutters/loss | retransmits, evictions, deferred type 21 | `recv gap`, `first retransmit`, `retry_gaveup`, `evicted=`, `deferred21=`, `cap_hits=` |
| No Party throughput at all | worker thread dead, tick fallback broken | `transport thread spawn FAILED` / `broker thread spawn FAILED` / `PANICKED`; `transport[heartbeat] up=false` |
| Two players collapse into one | identity collision (same Steam ticket / empty ticket) | broker `LoginWithSteam PlayFabId=…` twice; member dedupe |
| Broker answers but joins fail | lobby full/not found, stale connection string | broker `JoinLobby unknown connection=…`, `LobbyNotFound`, `party join refused` |

## Environment overrides

- `GBFR_LAN_STUB=host:port` — per-process broker override (all shims; overrides `[server]`).
- `GBFR_LAN_INI=<path>` / server `--ini` — alternate `lan.ini`.
- `GBFR_PARTY_LOG_HEX=1` — hex-dump every Party payload (`send_hex`/`recv_hex`).
- `GBFR_PARTY_FORCE_INLINE=1` — disable the transport/broker threads (everything runs on the
  game tick); use only to isolate a threading problem, it reintroduces tick stalls.

# Game integration

How `granblue_fantasy_relink.exe` v2.0.5 uses the replaced services, and what the shims must
do to satisfy it. RVAs are `VA − 0x140000000`. This is the condensed, verified version of the
`ghidra-notes` investigation; keep it updated when the game is patched.

## Loading

- `gbfr_http.dll` is loaded via Goldberg's `steam_settings\load_dlls`. It patches the
  ISteamHTTP vtable after `SteamAPI_Init`, the WinHTTP imports
  (`WinHttpConnect`, `WinHttpOpenRequest`, `WinHttpSendRequest`) and
  `PlayFabCore.PFServiceConfigCreateHandle`.
- WinHTTP rewrite rules: port 80/443 → the broker port; port 8081 is kept (the Cygames
  websocket); `WINHTTP_FLAG_SECURE` is stripped so the plaintext upgrade succeeds.
  `PFServiceConfigCreateHandle` is called with the broker origin instead of the real
  PlayFab endpoint.
- The exe statically imports **21** Party and **20** PlayFab Multiplayer functions — the
  shims export exactly those sets (plus `DllMain`). No missing import can be the cause of a
  failure; any remaining bug is behavioural.

## Boot and login

The boot blob (`dat/config/<hmac>.blob`) is base64 ChaCha20-Poly1305 (IETF) with key
`kdfg8kojildksuie23jsdfg8fg7klsdx`; the decrypted JSON is parsed by `ParseBootConfig`
(17-key table at `0x145AA8A38`). Keys the stack serves and why:

| Key | Served | Why |
|---|---|---|
| `PlayfabTitleId` | `1AC1AD` | not in the exe; used for the lobby connection string |
| `GameapiUrl`, `WebsocketUrl` | broker `http://…:8080`, `ws://…:8081` | plaintext endpoints |
| `MatchingSearchWaitTimeMinSec` / `MaxSec` | `0` / `3600` | matching wait window; the exe's join pump has its own 30 s budget, so serve the real service values rather than a short one |
| `MatchingRemainingMaxCounter`, `MatchingDefaultSearchLimit`, `MatchingEnableFindListSort` | `10`, `20`, `1` | matching UI |
| `PlaylogUrl`, `NetworkStatsUrl` | broker `/playlog*` | telemetry, answered empty |

Login: `LoginWithSteam` (PlayFab) → entity token → `sys/user_auth` (Cygames) returns
`ws_url`; the game opens the websocket and sends `join_lobby`. The broker echoes
`{"command","params"}`; the exe's parser (`FUN_14290b160`) requires exactly a root
`command` string and a `params` object, with `lobby_search_id` nested under `params`.
If the guest's `join_lobby` is not ACKed, the connect gate below never sees its work item.

## Lobby → mesh call order

Observed order in a working run (the exact interleaving of HTTP and WS calls varies):

**Host**: `CreateAndJoinLobby` → `PartySerializeNetworkDescriptor` → `PFLobbyPostUpdate`
(`network_descriptor`, `id_container`, …) → `PartyCreateNewNetwork` → connect-gate →
`PartyConnectToNetwork` (same network) → `PartyNetworkAuthenticateLocalUser` →
`PartyNetworkCreateEndpoint` → post-updates as members join.

**Guest**: `FindLobbies` → `JoinLobby` → `GetLobby` → `PartyDeserializeNetworkDescriptor`
→ `join_lobby` ACK → connect gate → `PartyConnectToNetwork` → authenticate → create
endpoint → the `op 3` chara-publish handshake.

The mesh work item (`+0x70` below) is created by the game's **own** network-boot /
`InitNetworkState` machines, not by our traffic — see "Mesh start".

## PlayFab state-change contract

The shim emits the types the exe's dispatch table consumes and writes only fields the exe
reads. The layouts are the real 1.8 ABI.

| Type | Layout (offsets) | Exe consumer |
|---|---|---|
| 0 CreateAndJoinCompleted | `result@4`, `asyncContext@8`, `lobby@0x10` | seeds `ctx+0x1d0` |
| 1 JoinLobbyCompleted | `result@4`, `newMember{id@8,type@0x10}`, `asyncContext@0x18`, `lobby@0x20` | seeds `ctx+0x1d0` |
| 2 MemberAdded | `lobby@8`, `member{id@0x10,type@0x18}` | seeds `ctx+0x1d0` |
| 4 MemberRemoved | `lobby@8`, `member@0x10`, `reason@0x20` | clears the peer row; must be emitted when a member disappears |
| 6 LeaveLobbyCompleted | `lobby@8`, `localUser@0x10`, `asyncContext@0x18` | body ignored |
| 7 Updated | `lobby@8`; flags `+0x10..0x13` (`owner`, `maxMembers`, `accessPolicy`, `membershipLock`); search count/keys `+0x14/+0x18`; lobby count/keys `+0x20/+0x28`; `memberUpdateCount@+0x30`, `memberUpdates@+0x38` stride `0x20`; size `0x58` | the state fan-out handler `FUN_143B48800` |
| 8 PostUpdateCompleted | `result@4`, `lobby@8`, `localUser@0x10`, `asyncContext@0x20` | reads only `result` |
| 10 Disconnected | `lobby@8` | clears cached handle `ctx+0x1d0`; terminal lobby loss |
| 12 FindLobbiesCompleted | `result@4`, `searchingEntity@8`, `asyncContext@0x18`, `count@0x20`, `results@0x28` | search results |

Type-7 is the one that must be *truthful*:

- `membershipLockUpdated` gate `0x143B488B1`: until a type-7 sets it, the exe never calls
  `PFLobbyGetMembershipLock` and `lobby+0xD8` stays unset. The shim sets it on the first
  Updated for a lobby and forces it on the join-completion Updated (the SDK guarantees the
  lock is populated before `JoinLobbyCompleted`).
- `memberUpdateCount` loop `0x143B4894B`: entries with a changed/new member property make
  the exe call `PFLobbyGetMemberConnectionStatus`. A count of 0 skips status refresh.
- Search/lobby key lists are read literally (loops at `0x143B4ADBE` / `0x143B48F4D`); a
  property that changed must be listed or the exe never re-reads it. The `id_container`
  branch (`0x3B49037`) is why the list must not be fabricated.
- Emit order: create → `MemberAdded` then `CreateAndJoinCompleted`; join →
  `MemberAdded(s)` → Updated → `JoinLobbyCompleted`; a local `PostUpdate` echoes
  `PostUpdateCompleted` then an Updated diff (the SDK's "sometime afterwards" contract).
  Diffs must be computed from real before/after snapshots, or the host never receives a
  type-7 for its own writes.

`PFLobbyGetLobbyProperty` / `GetSearchProperty` missing-key behavior is real-ABI: `S_OK` with
`*out = null`. The exe treats that as "absent", not an error.

## Mesh start (game-owned)

The exe starts its own mesh from its network-boot / `InitNetworkState` state machines:

- Leaf `FUN_14025fd50` (RVA `0x25FD50`): guards `session+0 != 3` and `session+0x28 == 0`,
  sets `session+0 = 1`, installs a task whose step 1 is `FUN_140253b50` (publishes member
  work, and is the **only** writer of `work+0x70 = 1`, at `0x1402554E0`), step 2 checks
  readiness, step 3 fires `session+0 = 2/3`.
- Callers: `FUN_1428E1180` (step 7 of the re-init machine built by `FUN_1428E4D40`) and
  `FUN_143B2C8E0` (step 5 of the boot machine built by `FUN_1428E3FA0`, gated on the Steam
  context). Neither is reached from a Party export or state change.
- Connect gate `FUN_142901540` (`0x142901549`): `cmp byte [work+0x70],0`; when zero it skips
  the connect (`+0xa0 = 2`) — the exe never calls `PartyConnectToNetwork`. When non-zero it
  picks `PartyConnectToNetwork` if `party+0x218` (invitation size) is set, else
  `PartyCreateNewNetwork` (`0x14290156D` / `0x1429015DB`).
- The pump `FUN_1429010F0` fails a `+0x70 == 0` item: `+0xa0 = 6` immediately or after the
  30 s budget in `work+0xb8`; `ProcessParty` case 2 then reports `0xffffffeb` (the −1C/−14C
  family) and tears the session down.

Consequences for the stack:

- **Do not** call the leaf, write `+0x70`, or NOP the gate. The 2026-09-14 "mesh start"
  hacks were removed; a hack-free run proves the game's own leaf ran (`+70=1` work row,
  `mode0=3`, `native=true`, and `PartyCreateNewNetwork`/`PartyConnectToNetwork` with no shim
  trigger line).
- The shim's job is only to be correct *around* the gate: have the descriptor present at
  `party+0xa0` (via the lobby property → `PartyDeserializeNetworkDescriptor`), let the
  invitation copy land at `party+0x208/+0x218`, and deliver types 3/4/10/12/21 within the
  30 s window.
- If `+0x70` never becomes 1, the fault is upstream (session latch/boot machine), not in the
  Party shim; probe `rows=[id=… +70=… +a0=…]`, `mode0`, and the work/ready queue pairing.

## Ordinals and `CanMatchingSetting`

- `work+0x68` is the member's `id_container` slot (ordinal); `work+0x6c` is a per-member
  sequence number assigned locally as `MemberAdded` arrives (`++[lobby+0x198]`). The broker
  never allocates slots.
- Each peer posts its own `id_container` via `PFLobbyPostUpdate`; agreement is emergent, not
  guaranteed. A remove-then-rejoin can leave a stale work/ready row resolved by
  `FUN_1408B9E10` (ordinal → first row with `+0x68 == ordinal`, no entity check).
- `CanMatchingSetting` (`0x1D7BB10`) compares the cached ordinal pair
  `[questmgr+0x6c828] == [questmgr+0x6c820]` (selector `+0x6cce8 == 0`). Both are written
  by one call (`FUN_1403A13C0`): expected = the roster record's slot, observed =
  `FUN_140265250(session)`. A stale observed value can leave the guest's gate false even
  though its live work row is correct. This gates a quest-counter action but **not** the
  mode-3 arming path; do not try to "fix" the guest's ordinal from the shim.

## Entity identity

The exe identifies a peer by the 20-byte **entity id** on the wire and in type 12/21.
`PartyGetUniqueIdentifier`'s per-process uid is not a cross-peer key: the remote uid read on
type 12 is dead code, and the local uid (`session+0x228`) is read only by the type-13
handler. The shim's datagrams carry no uid.

## Error codes

- The −1C/guest −14C/host family is the exe's own matching/connect failure
  (`0xffffffeb`, −21), not a PlayFab HRESULT; it is surfaced with role-dependent UI codes.
- The PlayFab shim now returns the genuine `E_PF_*` codes (`0x8923xxxx`) for the conditions
  it can detect — `E_PF_NOT_INITIALIZED`, `E_PF_INSTANCE_ALREADY_EXISTS`,
  `E_PF_ENTITY_KEY_MALFORMED`, `E_PF_ENTITY_TOKEN_MALFORMED`, `E_PF_OBJECT_STILL_PENDING`,
  `E_PF_LOBBY_NOT_FOUND`, `E_PF_LOBBY_NOT_JOINABLE`, `E_PF_LOBBY_ALREADY_MEMBER`,
  `E_PF_LOBBY_FULL`, `E_PF_LOBBY_MEMBER_NOT_IN_LOBBY`, `E_PF_LOBBY_EMPTY_UPDATE`,
  `E_PF_LOBBY_UPDATE_AFTER_DISCONNECT`, `E_PF_SERVICE_*`. The broker's HTTP status is mapped
  to those codes (404 → `LOBBY_NOT_FOUND`, 409 → full/unexpected, 5xx → service 5xx).
- Party-side errors are a single `E_FAIL` (`0x80004005`); the exe does not inspect them.

## Fragile spots (verified the hard way)

- **One-shot handshakes.** `op 3` subs 5/6/7/8 (chara publish) and quest sync are sent once,
  with no app-level retry; `conn+0x5C == 3` requires both halves. GuaranteedDelivery in the
  transport is what makes this survivable; a lost datagram is otherwise permanent.
- **Rank gate.** On `op 3 sub 6` status 0, the host sends the `sub 7` character blob only if
  `my_rank <= peer_rank` or a quest-manager predicate says so; otherwise the connection is
  marked −20 and no `sub 7` follows. The ranks are the per-peer ordinals above, so a
  divergence surfaces here.
- **Type-7 fidelity.** A host must receive a type-7 for its own posts; a canned key list
  (or no diff) silently starves the exe's lobby-state consumers.
- **FindLobbies.** The exe builds a real filter (including `number_key7 eq <version>` and
  `string_key1/2` for join-by-code) and passes an empty sort. If the filter is dropped or
  applied sloppily, the wrong lobby list is presented (the historical bug was a substring
  test where `locked` matched `unlocked`; the broker now evaluates the filter properly).
- **Lobby close semantics.** The lobby closes when its last member leaves. An owner
  leaving applies the create-time `OwnerMigrationPolicy` (`Automatic` hands ownership to
  another member; `None`/`Manual` clears the owner, and `PFLobbyGetOwner` then returns
  `S_OK` + NULL). A remaining peer can still get `LobbyNotFound` if the lobby actually
  closes; the shim emits type 10 so the exe clears its cached handle.
- **Membership-lock population order.** The lock getter errors until an Updated sets
  `membershipLockUpdated`; the join-completion path forces it, or the exe's `lobby+0xD8`
  stays unset.

## Open threads (state at the time of these docs)

The session/mesh path works: create/join, ready rows (`[k:3:t1:s0]` on both peers), and
UDP type-21 traffic. What was still under investigation after the last recorded run:

1. **Quest-start loader stall.** The ready-check node `ChangePartyJoinEnable` parks on async
   op `0xA596AF73` (`network_error_check_steam`) because its boot-queue entry
   `{mode=3, hash=0xA596AF73}` is still `v=queued` — the mode-3 `LoadAsset` arming pass never
   ran (`arm3=0`, probe `asyncload … v=queued`). `ui::fsm::action::LoadAsset` arms a screen's
   resource modes on activation; the ready-check screen has no `LoadAsset` component, so the
   arming must come from an earlier lobby/quest-counter screen that the LAN flow may not be
   entering. The recorded `done3=1` (queue pass completed) contradicts `arm3=0` and is not
   settled. Next steps: compare against a native flow, probe `RM+3*4+0x4e4` and the quest
   FSM screen transitions, and verify whether the quest-counter screens are reached.
2. **`CanMatchingSetting` false on the guest.** The pair can be stale while the live work
   row is correct. It gates a quest-counter action, not the arming; whether it matters for
   the stall is still open.
3. **`id_container` ownership.** The broker treats it as opaque and peers post their own
   versions (last-writer-wins). Making the broker the slot authority would remove the
   remove-then-rejoin hazard; not implemented.

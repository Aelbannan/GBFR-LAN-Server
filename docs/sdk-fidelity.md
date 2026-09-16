# SDK fidelity

What the genuine Microsoft binaries do versus what this stack does, and which differences
are deliberate. Use this when a game patch, a crash, or a new feature raises the question
"is the shim wrong, or is this the SDK contract?"

## Genuine binaries

| | Party | PlayFab Multiplayer |
|---|---|---|
| File | `PartyWin.dll.ms` | `PlayFabMultiplayerWin.dll.ms` |
| Version | **1.10.12** (`1.10.2509.24002`) | **1.8.0** (`1.8.2506.05002`) |
| Size | 4,027,432 B | 1,860,152 B |
| PE timestamp | 2025-09-24 | 2025-06-05 |
| Exports | **157** | **77** |
| Transport | DTLS over UDP + relay (`/Party/RequestParty`), QoS, speech tokens | libHttpClient → WinHTTP + SignalR pub/sub for lobby notifications |
| PDB | unavailable — public symbol servers return 404 | unavailable |

The layout and behaviour facts below come from disassembling those binaries and the exe's
consumers, not from a PDB or private headers. The authoritative public references are the
PlayFab Party 1.10 C API and the PlayFab Multiplayer Lobby REST docs (API version 260814);
the 1.8 binary matches the live docs for every field it has.

## Export surface

The exe statically imports exactly **21** Party + **20** PlayFab Multiplayer functions (IAT
re-parsed; no delay imports, no `GetProcAddress` path for these DLLs). The shims export those
sets verbatim. Everything else in the real DLLs is untouched:

- Party's 136 extra exports: voice chat/audio manipulation/TTS/transcription (67),
  invitations (7), roster/device/endpoint/statistics queries, shared properties, custom
  contexts, with-entity-handle variants, allocator/diagnostic hooks. None is imported.
- PlayFab Multiplayer's 57 extra exports: server/multiplayer-server APIs, matchmaking,
  invite listeners, handle variants, property-key getters, diagnostics.
- `PFMultiplayerGetErrorMessage` / `PartyGetErrorMessage` are the only error helpers imported;
  the exe checks neither for a missing export.

**No remaining bug can be "a missing function."** A failure is semantic: layout, ordering,
error code, or a field value.

## Deliberate service replacements

| Real stack | This stack | Why acceptable |
|---|---|---|
| Azure relay + DTLS, QoS probes, region selection | plain UDP between peer-advertised `ip:port` | the network is a private LAN; no NAT/relay needed |
| SignalR/PubSub push for lobby changes | 250 ms `GetLobby` poll per member | LAN latency; the exe consumes the same state changes |
| PlayFab service validation (Steam tickets, lobby constraints) | broker-side memory state | offline/LAN play; no live service is contacted |
| `network_descriptor` is an opaque Azure descriptor | `LAN1.<uuid>.<ip>` ASCII string | the exe treats it opaquely; only the shim parses it |
| `number_keyN`/`string_keyN` are a service-enforced convention | broker stores/evaluates the same names | the convention is a service rule, not an SDK rule |

## PlayFab shim fidelity

**Faithful (verified against the real 1.8 ABI and the exe's reads):**

- State-change numbering and layouts for the types the exe dispatches (0, 1, 2, 4, 6, 7, 8,
  10, 12), including the `PFLobbyDataUpdate`, `PFLobbyCreateConfiguration` and
  `PFLobbyJoinConfiguration` field offsets.
- `PFLobbyGetLobbyProperty` / `GetSearchProperty` / `GetMemberProperty` miss behavior:
  `S_OK` with `*out = null`.
- Asynchronous API shape: create/join/find/post/leave return `S_OK` and report the outcome
  in a completion state change; `FindLobbies` reports service failure in
  `FindLobbiesCompleted.result`.
- All broker I/O runs on a worker thread; `PFMultiplayerStartProcessingLobbyStateChanges`
  carries no network work and never blocks (the real `Start` only drains its queue).
- Ordering rules that matter: `MemberAdded` before `CreateAndJoinCompleted`; `MemberAdded(s)`
  → Updated (with `membershipLockUpdated`) → `JoinLobbyCompleted`; a local post echoes
  `PostUpdateCompleted` then an Updated diff.
- `PFLobbyGetMembershipLock` returns the broker's real value and `E_PF_OBJECT_STILL_PENDING`
  while a join/create is pending.
- `PFLobbyGetOwner` returns `S_OK` + NULL once an ownerless (post-leave, migration policy
  `None`/`Manual`) lobby is observed; the exe null-checks the output.
- Genuine `E_PF_*` error codes for detectable conditions (bad handle/entity key/token,
  re-init, lobby not found/full/not joinable/already member/member not in lobby, pending
  object, post after disconnect, HTTP status → service 4xx/5xx).
- `PFMultiplayerFindLobbies` now reads the real `PFLobbySearchConfiguration`
  (`friendsFilter`, `filterString`, `sortString`, `clientSearchResultCount`) and forwards it.

**Deviations (known and accepted, or bounded):**

| Area | Deviation |
|---|---|
| Notification channel | no SignalR push; changes arrive on the poll (≤250 ms) and on local posts |
| `PFLobbyForceRemoveMember` | validates the target entity key then returns `S_OK` without kicking (no LAN kick path; a fabricated removal would desync the roster). No type-5 completion is emitted |
| `PFLobbyGetMemberConnectionStatus` | returns `0` while pending, then `1` (Connected) for any member in the lobby; the real DLL tracks per-member state |
| `PFMultiplayerGetErrorMessage` | returns one constant string (the real DLL maps 122 `E_PF` codes); the exe only displays it |
| FindLobbies paging | the shim caps the state-change result array at **16** rows (the real SDK passes the service page through; `clientSearchResultCount` is forwarded to the broker). Search-row `membership_lock` is hardcoded `0` |
| Filter coverage | the broker evaluates the documented subset (`eq/ne/le/lt/ge/gt`, `and`, the predefined keys). Anything else is logged, returned in `Warnings`, and fails closed (no rows) |
| Property key arrays | `kv_list` / `intern_kv_arrays` truncate to 32 keys with a log (the real SDK has no such cap) |
| Allocation lifetime | state-change buffers are not freed in `Finish` (accepted leak, bounded by session length) |
| Handle validation | most exports do not validate that the `PFMultiplayerHandle` belongs to the shim (the real DLL returns `0x89236400`); getters validate their own pointers |
| `PFLobbyLeave` | the local lobby is torn down before the broker replies; type 6 is emitted asynchronously with the broker's result in `result` (+4) |
| `PFLobbyMemberDataUpdate` | a dead parse branch (the exe always passes NULL for member data updates) |

## Party shim fidelity

**Faithful:** the 21 exports; `PartyNetworkDescriptor` = 357 bytes (`37/20/300`) and the
449-byte serialization cap; descriptor zeroing on a failed deserialize; state-change types
2/3/4/10/12/19/21 and the fields the exe reads (`party+0x30/0x88/0x90/0x98/0xa0/0x208/0x218/0x220`);
`PartySetWorkMode` no-op semantics; per-message send options.
`PartyEndpointSendMessage` honors `GuaranteedDelivery`/`SequentialDelivery` per call.

**Deviations:**

| Area | Deviation |
|---|---|
| Descriptor contents | `LAN1.<uuid>.<ip>` instead of an Azure descriptor; the exe never parses it |
| Local type 12 | not emitted for the local endpoint (the exe ignores local `EndpointCreated`; the shim's single `type12_delivered` bit is reserved for the remote) |
| Unique identifiers | minted immediately instead of after the local type 12; the exe does not check |
| Endpoint destruction | no type 13/20; remote endpoints are never removed until leave/cleanup (cap 8) |
| Send targeting | `targetEndpoints` and multi-buffer sends are ignored; one datagram per call, broadcast to all remotes |
| Network configuration | `PartyNetworkConfiguration` (exe: maxUserCount=4, maxDeviceCount=4) is logged, not enforced; the shim caps remotes at 8 |
| Error mapping | `PartyGetErrorMessage` returns a static string; API failures are `E_FAIL` |
| Allocation lifetime | Party state-change buffers are not freed in `Finish` |
| Wire security | no DTLS, no authentication, no nonce/MAC; anything on the UDP port can inject |
| Missing flow | no relay fallback, no QoS/region probing, no speech token service |

## When the game is patched

The version-sensitive surfaces, in rough order of likelihood to break:

1. **Exe RVAs** in `game-integration.md` (mesh-start leaf/callers, connect gate, type-7
   gates, ordinal writers). A patch that moves code invalidates the probes but not
   necessarily the shim.
2. **The 21 + 20 import sets** — a new import is a hard failure the shim cannot mask.
3. **State-change layouts** if Microsoft ships a newer SDK in the game patch.
4. **Lobby/filter semantics** if the exe changes its search configuration.
5. **The `number_key` mapping** (create-time parallel array) if the lobby UI changes.

`steam_http_shim.log`, `playfab_mp_shim.log` and `party_shim.log` are designed to show the
first step that changed; the shims log unknown paths, unsupported filters, and refused calls
rather than silently succeeding.

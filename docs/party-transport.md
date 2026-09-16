# Party transport

`PartyWin.dll` is a drop-in replacement for the Microsoft PlayFab Party client. It does not
speak Azure Party: it provides the same C API, keeps the same state-change model, and moves
`PartyEndpointSendMessage` payloads over direct UDP between the peers the broker introduces.

The exe imports exactly **21** Party functions and the shim exports those 21 plus `DllMain`.
The game's send path is a single call site, so the shim sees every Party byte the game emits.

## API surface the game uses

```
PartyInitialize                     PartyCreateLocalUser
PartyLocalUserGetEntityId           PartyCreateNewNetwork
PartyConnectToNetwork               PartyDeserializeNetworkDescriptor
PartySerializeNetworkDescriptor     PartyNetworkAuthenticateLocalUser
PartyNetworkCreateEndpoint          PartyEndpointGetEntityId
PartyEndpointGetUniqueIdentifier    PartyEndpointSendMessage
PartyNetworkGetNetworkDescriptor    PartyNetworkLeaveNetwork
PartyStartProcessingStateChanges    PartyFinishProcessingStateChanges
PartyGetNetworks                    PartyDestroyLocalUser
PartyCleanup                        PartySetWorkMode
PartyGetErrorMessage
```

Behavior notes:

- `PartySetWorkMode` is accepted and ignored (the exe only uses Audio/Manual work modes;
  networking is automatic in the real DLL too).
- State changes are queued by the shim and returned by `PartyStartProcessingStateChanges`;
  the game returns them through `PartyFinishProcessingStateChanges`. The shim emits types
  **2, 3, 4, 10, 12, 19, 21** only. A type is handed out once; `Finish` marks type 10/12
  as delivered so the next pump can proceed.
- `PartyNetworkAuthenticateLocalUser` completes immediately (`type 4`, size `0x30`) once the
  network handle is the shim's.
- `PartyNetworkCreateEndpoint` mints a per-process uid and queues `type 10` (size `0x30`).
  The uid is **not** a cross-peer key (see `game-integration.md`).
- `PartyGetNetworks` returns the shim's `Vec<*mut Network>`; the array is owned by the shim.

## Lifecycle

**Host** — `PartyCreateNewNetwork`:

1. Bind UDP (`[party] udp_port`, fallback to an ephemeral port when busy).
2. Mint a UUID descriptor, store the UDP port in `descriptor.opaque[0..2]` and the
   advertised IPv4 in `opaque[2..6]`.
3. Register `(network_id, entity_id, udp_port, ip)` with the broker (`POST /party/join`).
4. Queue `type 2` (`CreateNewNetworkCompleted`) carrying the descriptor and the invitation
   string.

**Guest** — `PartyDeserializeNetworkDescriptor` reads the lobby property
`network_descriptor` (`LAN1.<uuid>.<8-hex-ipv4>`), clears and fills a 357-byte descriptor;
then `PartyConnectToNetwork` binds UDP, registers with the broker, queues `type 3`
(`ConnectToNetworkCompleted`), and sends a HELLO.

**Both** — `PartyNetworkAuthenticateLocalUser` (type 4) and `PartyNetworkCreateEndpoint`
(type 10) follow; remote peers become visible as type 12s (see below).

`PartyNetworkLeaveNetwork` queues `type 19` and, for one of the shim's networks, posts
`/party/leave`, flushes already-queued sends, closes the UDP socket and drops the
retransmit state. The network and endpoint boxes are intentionally leaked (the exe still
holds raw pointers), so an in-flight send can never use freed memory. `PartyCleanup`
releases every socket and the handle.

## Descriptor

`PartyNetworkDescriptor` is `37 + 20 + 300 = 357` bytes (`identifier`, `region`, `opaque`).
The serialized form is a C string, at most 449 bytes:

```
LAN1.<36-char uuid>.<8-hex ipv4>       e.g. LAN1.0f0d…-…-….c0a86470
```

- Non-`LAN1.` input is rejected and the output is zero-filled first (the real DLL also
  zeroes on failure, and the exe ignores the return value but still reads the field).
- Loopback IPs in a broker response are rewritten to the descriptor's advertised IP, so a
  guest on another PC can reach a host that registered as `127.0.0.1`.
- `[party] advertise_ip` overrides auto-detection; otherwise the shim picks the local IPv4
  used to reach the broker, or a routable address from a UDP connect probe.

## Mesh discovery

Peer discovery has two independent paths:

1. **Broker poll** — `PartyStartProcessingStateChanges` requests `GET
   /party/peers?network_id=…` at most once per 200 ms and on HELLO; the broker thread runs
   the HTTP and the next tick applies the results. Each member becomes a remote endpoint
   (`type 12`) once its `(ip, udp_port)` is known.
2. **UDP HELLO** — hosts/guests send a `GBFR` HELLO to every known remote and on first
   sight of an unknown sender; HELLOs also piggyback the current ack. This keeps a mesh
   alive when the broker is briefly unreachable.

Membership rules enforced on the broker side: join refreshes a 30 s `seen` TTL; the shim
re-registers every 10 s; `/party/peers?entity_id=…` also refreshes the caller. A
`member_seq` makes reordered joins/leaves harmless.

A remote endpoint is only created after the exe has inserted the peer into its live member
list (`FUN_140260b30` on a native connect). Type 12 is held until the local type 10 has been
delivered, and buffered type-21 messages are replayed only after the remote's type 12 has
been handed back — this ordering is required by the exe's connect/queue state machine.

## Wire format

One message per datagram. Header **v3** is 60 bytes (the ack field is present):

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `GBFR` |
| 4 | 1 | kind: `1` HELLO, `2` message, `3` standalone ack |
| 5 | 1 | header version (3) |
| 8 | 16 | network id (ASCII, zero-padded/truncated) |
| 24 | 20 | sender entity id (ASCII, zero-padded/truncated) |
| 44 | 4 | payload length (LE u32) |
| 48 | 4 | `PartySendMessageOptions` bits (LE u32) |
| 52 | 4 | sequence (LE u32; 0 for HELLO/ACK) |
| 56 | 4 | cumulative guaranteed-space ack (LE u32) |
| 60 | n | payload |

With `PARTY_RELIABLE = false` the shim falls back to the old **v2** 56-byte header (no ack,
one global sequence counter, no retransmits) — a debug switch, not a supported mode.

Sends are currently broadcast to every known remote (the target-endpoint list is ignored;
the game passes one target, so in a 2-player session this is identical). Messages are sent
as one datagram each; oversized payloads are refused with an error rather than fragmented.

## Reliability

The game requests reliability **per message**, not per opcode:
`PartyEndpointSendMessage(..., options = packet[0xC] | 2, ...)`. The exe uses
`GuaranteedDelivery | SequentialDelivery` (`0x3`) for control RPCs and
`SequentialDelivery` best-effort (`0x2`) for high-rate streams, and it has **no**
application-level loss recovery — the handshake (`op 3` subs 5→6→7→8, quest sync
`0xC/0xD/0xE`) is one-shot. The reliable layer is therefore load-bearing, not cosmetic.

Implemented in `party-shim/src/reliable.rs` (pure logic, compiled into both the DLL and
`reliable_test.rs`):

| Option | Behaviour |
|---|---|
| `GuaranteedDelivery` (0x1) | retransmit until acked or endpoint destroyed; cumulative acks per (sender→target) pairing |
| `SequentialDelivery` (0x2) | per-pairing FIFO; guaranteed+sequential buffers out-of-order arrivals up to `OOO_CAP` |
| Sequential + best-effort | older-than-newest arrivals are dropped; nothing is buffered or delivered late |
| `AllowLazyAcknowledgement` (0x10) | suppresses the forced standalone ack |

Tuning constants: RTO starts at 60 ms and doubles to 500 ms; 8 retries then a loud give-up
(the message stays pending until acked/evicted); pending map capped at 4096 (oldest evicted
with a log); out-of-order/duplicate set capped at 256; a forced ack is produced 30 ms after
a guaranteed arrival that was not piggybacked.

Per-pairing sequence spaces keep a best-effort loss from stalling guaranteed
acknowledgement. The v3 cumulative ack is in every datagram, including HELLOs, so a
unidirectional sender still gets acked by return traffic.

## Threading

The shim runs three process-lifetime worker threads (started lazily on the first Party
export, never from `DllMain`, never joined):

| Thread | Role |
|---|---|
| `party-transport` | owns UDP send/recv, retransmits, forced acks; runs every ≤2 ms or when the outbox is signalled |
| `party-broker` | owns all broker HTTP (`/party/join`, `/party/leave`, `/party/peers`); broker requests never run on the tick. (Descriptor advertising can still do a short connect probe to detect the local IP.) |
| `party-solo-sample` | read-only game-memory sampler (250 ms; change + 5 s heartbeat lines). Writes nothing, takes no shim lock |

Concurrency rules:

- One global `G` mutex protects every `Network`/`Endpoint` box, the UDP socket and the
  queues. `PartyStartProcessingStateChanges` drains the state-change queue and services
  timers under `G`; the transport thread takes `G` for bounded batches only
  (`OUTBOX_BATCH = 128`, `RECV_BATCH = 256`), so a burst cannot starve the tick.
- Sends are enqueued to an unbounded outbox (condvar-woken); the transport thread performs
  the datagram I/O. If a worker thread fails to spawn or panics, the shim sets
  `TRANSPORT_UP = false` and falls back to tick-driven inline transport instead of queueing
  into a dead consumer.
- The broker queue is capped at 512 with coalesced register tasks; a lost poll is re-armed
  after `PEER_POLL_TIMEOUT_MS = 10 s`, so broker-thread death cannot wedge peer discovery.

## State changes emitted by the shim

| Type | Name | Trigger |
|---|---|---|
| 2 | CreateNewNetworkCompleted | `PartyCreateNewNetwork` |
| 3 | ConnectToNetworkCompleted | `PartyConnectToNetwork` / same-id reconnect |
| 4 | AuthenticateLocalUserCompleted | `PartyNetworkAuthenticateLocalUser` |
| 10 | CreateEndpointCompleted | `PartyNetworkCreateEndpoint` |
| 12 | EndpointCreated (remote) | broker poll / HELLO creates a remote endpoint |
| 19 | LeaveNetworkCompleted | `PartyNetworkLeaveNetwork` |
| 21 | EndpointMessageReceived | an inbound message is delivered |

Layouts follow the real 1.10 C ABI; the offsets the exe reads were verified against its
handlers. A local type 12 is deliberately not emitted: the exe ignores it, and the shim's
single `type12_delivered` bit must stay reserved for the remote endpoint.

## Debugging

- Environment: `GBFR_PARTY_LOG_HEX=1` (hex-dump every payload),
  `GBFR_PARTY_FORCE_INLINE=1` (disable both worker threads; everything runs on the tick).
- `party_shim.log` carries per-type ledgers, `EndpointCreated remote …`, `recv gap …`,
  `first retransmit …`, `retry_gaveup`, `transport[heartbeat]` and `broker[…]`/`msg_stats`
  counters. `solo …` lines come from the sampler and are the only probe that runs without a
  shim network.
- Tests: `reliable_test.rs` (pure loss/dup/reorder simulation) and `broker_http_test.rs`
  (drives the real DLL against an in-process mock broker). See the root README.

## Known limitations

- Remote endpoints are capped at **8** (hardcoded); the broker can be configured higher
  (`[lobby] max_players`), but the shim will not track more peers.
- `PartyEndpointSendMessage` ignores `targetEndpoints` and `PartyDataBuffer` count > 1.
- State-change allocations are never freed in `PartyFinishProcessingStateChanges`
  (accepted leak; bounded by session length).
- No endpoint-destroyed (13) / network-destroyed (20) changes, and the shim mints uids
  immediately instead of after a local type 12 (the exe never checks either).
- The UDP wire is unauthenticated: anything that can reach the port can HELLO and inject
  messages. This matches the broker's own unauthenticated model and is accepted for a
  private LAN.
- `PartyGetErrorMessage` returns a fixed string; `PartyCreateNewNetwork` logs but does not
  enforce the exe's `maxUserCount=4` config.

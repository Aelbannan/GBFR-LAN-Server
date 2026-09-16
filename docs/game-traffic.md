# Game traffic (Party RPC layer)

The shim treats `PartyEndpointSendMessage` payloads as opaque, but debugging a stalled quest
means reading them. This is what the game's own protocol looks like as far as it has been
recovered (v2.0.5 disassembly + `party_shim.log` wire logs). It is **partial**: opcodes are
identified only where a handler or gate was disassembled. Treat the tables as a map, not a
complete spec.

## Where it enters the shim

There is exactly **one** game call site to `PartyEndpointSendMessage`
(`0x142513CA7`); the options argument is the packet's reliability byte OR'd with
`SequentialDelivery`:

```
options = packet[0xC] | 0x2        ; 0x142513C50 -> 0x142513C65
```

So the game never asks for a bare best-effort send; it asks for sequential, plus guaranteed
for the messages that carry control state.

## Packet header

The game's network packet starts with:

```
+0x00  u64   (sub << 32) | op        ; op in the low dword, sub in the high dword
+0x08  u32   size
+0x0C  u8    reliability             ; 0x00 best-effort, 0x01 guaranteed
+0x0D  u8/u16 pad
```

`size`, `reliability` and the payload travel into Party; the `op`/`sub` stay visible in
`party_shim.log`'s `type21 finished opcode=… sub=…` lines and in `msg_stats`'s
`send_ops`/`recv_ops` histograms.

## Reliability classes observed

| Options byte | Party options | Used by |
|---|---|---|
| `0x01` → `0x3` | Guaranteed + Sequential | `op 2` subs `0x28,0x29,0x35,0x38,0x3A,0x3E,0x3F,0x40,0x4F,0x58,0x5A,0x5F`; `op 3` subs `1,5,6,7,8,0xA,0xF,0x10`; `op 5` subs `5,6,7,0xC`; `op 6` subs `0x16,0x28,0x29` |
| `0x00` → `0x2` | Sequential, best-effort | `op 5` sub `3`, `op 6` sub `1` (high-rate streams) |

Reliability is **per message**, not per opcode: `op 5 sub 3` has variants that take the byte
as a parameter (callers pass `dl=1`). Do not table-drive it; always read the byte.

## Opcode / sub-op map (recovered so far)

| Op | Role (recovered so far) |
|---|---|
| `2` | member/roster RPCs. `sub 0x5A` carries a member key/ordinal (e.g. the guest's own ordinal at payload `+0x10`); `sub 0x28/0x29` are connect-adjacent |
| `3` | character publish / join handshake. Dispatch table at `0x14619DFF8`, indexed `sub−1`; subs `4` and `9` have **no handler**, and the subs with handlers are `1,2,3,5,6,7,8,0xA,0xB,0xC,0xD,0xE,0xF,0x10`. `sub 5/6/7/8` are the chara-publish handshake below |
| `5` | gameplay RPCs. `sub 3` is the high-rate stream (best-effort); `sub 5/6/7/0xC` are guaranteed control |
| `6` | gameplay RPCs. `sub 1` is the high-rate stream (best-effort); `sub 0x16/0x28/0x29` are guaranteed control |
| `0xC/0xD/0xE` | quest sync (one-shot, guaranteed) |

The Party RPC layer also exposes named classes — `CBUChat` (behavior), `CBUUserInfo`,
`CBUUserPlayerName`, `CBUJoinUserInfo` (user, `NetworkRecvManager`), `CBUPlayerCard`,
`CBUCharaDataSync`, `CBUMultiCPUCharaData` (system, `NetworkSystemRpcManager`),
`SyncMultiQuestPartyCharaDataSync` (`MultiQuestPartyEntityContainer`). These are game RPC
classes inside Party messages, **not** Cygames WebSocket commands. A member's identity in
these messages is the 20-byte entity id, never the Party uid.

## `op 3` sub 5→6→7→8 (chara publish)

This is the load-screen handshake and it is **one-shot in both directions** — no app-level
retry, no timer anywhere. The transport's GuaranteedDelivery is the only thing that keeps it
whole.

1. **sub 5** — sent once per endpoint when `[ep+0x5C] == 1`, then `[ep+0x5C] = 2`
   (`FUN_142512AE0`, send `0x142512E82`). Reply is **sub 6**.
2. **sub 6** — status reply (`0x140B440E0`).
3. **sub 7** — the character blob (`movabs rax, 0x700000003`, size `0xC8`), sent once only
   on a `sub 6` with status 0 (`0x140B462A0`).
4. **sub 8** — completion (`movabs rax, 0x800000003`, size `0x10`, `0x140B48B6E`).

Both halves must land for `conn+0x5C == 3` (`0x140B48BD1`, `0x140B444D5`). That state is what
rescues a failed connect item: a connect failure is only marked failed when
`conn+0x5C != 3`.

**Rank gate on sub 6 status 0.** The host sends `sub 7` only if `my_rank <= peer_rank`
(`cmp eax,[rcx+0x6c]`, `0x140B46066–0x140B46079`), or if the quest-manager predicate
`FUN_140B48E20` returns 0. Otherwise it marks the connection `−20` (`0xffffffec`) via
`FUN_14250FA70` and **never sends sub 7**. The `sub 5` responder has the mirror gate and
answers status 6. The ranks here are the per-peer ordinals (`work+0x68`/`+0x6c`); a
divergence surfaces as this gate.

**Silent no-retry drop sites** (any one turns the handshake into a permanent stall *without*
losing a datagram):

- `sub 5` sender's connection lookup misses → return (`0x140B43FF1`).
- `sub 6` connection lookup misses → return (`0x140B43D58`).
- `sub 7` gate byte `[msg+0x30C] == 0` → return.
- `sub 7` apply (`FUN_140B48640`) bails on any of `session+4 != 3`, `[0x1471B43D8] == 0`,
  overlay null, overlay `vtbl+0x108` false, or no ready row for `[msg+0x10]`; it writes
  `conn+0x78 = −13` (`0xfffffff3`, `0x140B48A18`) and sends **no sub 8**.

## Ready/queue rows and the load gate

`FUN_140267450` (the "all members ready" predicate) returns `0` only when:

- the roster chain is non-null and the roster count matches `[roster+0x100]`,
- no roster row has ordinal `-1`,
- **every** ready/queue row (`0x180` stride, `DAT_1471AFB30`) has byte `+0x10 != 0`.

`+0x10 = 1` is written in exactly one place: `0x14025E55C`, when a connect/queue item reaches
`work+0xa0 == 4` (success). The character-blob apply does **not** touch it. A row at
`+0x10 == 0` with state `5` returns `1` (hard fail → the −21 UI); any other `+0x10 == 0`
returns `2` (keep waiting). The load screen polls this with **no timeout** when
`task+0xa8 == 0` (`FUN_1428F2910`), so a never-completing row is an indefinite,
UI-interactive wait. This is why deferred/held type-21s show up as a load stall rather than a
disconnect.

## Recovery

There is no application-level retransmit. The only recovery path is the received-message
dispatcher's NACK-driven state re-arm (`0x140B460CF`), which a **lost** packet never triggers
(and the transport layer should make loss irrelevant). If a one-shot message is ever dropped
above the transport, no later traffic repairs the exchange.

## Practical debugging

- `msg_stats` gives the op histogram and the reliability counters; a `sub 5/6/7/8` missing on
  one side is terminal. Log every message (`GBFR_PARTY_LOG_HEX=1` dumps payload hex).
- `party_shim.log`'s `type21 finished opcode=… sub=… len=…` lines are the cheapest way to see
  whether the handshake happened, and `SUB7_APPLIED` is latched after a sub-7 delivery.
- If the connect never happens, the problem is upstream of this layer — see the `+0x70` gate
  in `game-integration.md`. The quest-loader stall is a different layer again
  (`asyncload … v=queued`, mode-3 arming).

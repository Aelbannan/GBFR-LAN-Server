# gbfr-lan reference documentation

These docs consolidate the reverse-engineering and design investigation that produced this
repo. They describe the **final implementation** and the game-side contract it satisfies —
exact endpoints, offsets and semantics, without the day-by-day investigation narrative.

| Doc | Contents |
|---|---|
| [architecture.md](architecture.md) | The stock (live) service flow, what the stack replaces, boot/lobby/party flows, broker endpoint reference, lobby model, configuration |
| [party-transport.md](party-transport.md) | `PartyWin.dll` shim: API surface, mesh discovery, UDP wire format, reliability layer, threading |
| [game-integration.md](game-integration.md) | What the exe expects: import set, call order, state-change layouts, mesh start, −1C/−14C, ordinals, open threads |
| [game-traffic.md](game-traffic.md) | The game's own Party RPC layer: packet header, op/sub map, reliability classes, `op 3` handshake, load gate |
| [diagnostics.md](diagnostics.md) | Log files, line catalogue, probe fields, symptom → first checks, debug environment variables |
| [nucleus.md](nucleus.md) | Multi-instance / Nucleus Co-op: handler layout, per-instance ports and IDs, troubleshooting |
| [sdk-fidelity.md](sdk-fidelity.md) | Genuine Microsoft DLLs vs the shims: versions, exports, error codes, deliberate deviations |
| [reverse-engineering.md](reverse-engineering.md) | Patch-recovery runbook: assets, tooling, method, what to re-derive and how to verify it |
| [provenance.md](provenance.md) | Which `ghidra-notes` report each section came from, and what is safe to prune |

**Version basis.** The game-side facts (RVAs, offsets, struct layouts) are for
`granblue_fantasy_relink.exe` **v2.0.5** (image base `0x140000000`, `RVA = VA − 0x140000000`).
The SDK comparison uses the shipping `PartyWin.dll.ms` **1.10.12** (`1.10.2509.24002`) and
`PlayFabMultiplayerWin.dll.ms` **1.8.0** (`1.8.2506.05002`). A game patch can move any of these;
[start here](reverse-engineering.md#re-derive-after-a-patch) when it does.

**Sources.** Cleaned up from `dev/ghidra-notes/*.md` and `dev/paseo-prompts/*.md`
(2026-09-06 .. 2026-09-16), cross-checked against the source in this repo. See
[provenance.md](provenance.md) for the file-by-file mapping.

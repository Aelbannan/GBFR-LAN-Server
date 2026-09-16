# gbfr-lan reference documentation

These docs consolidate the reverse-engineering and design investigation that produced this
repo. They describe the **final implementation** and the game-side contract it satisfies —
exact endpoints, offsets and semantics, without the day-by-day investigation narrative.

| Doc | Contents |
|---|---|
| [architecture.md](architecture.md) | The stock (live) service flow, what the stack replaces, boot/lobby/party flows, broker endpoint reference, lobby model, configuration |
| [party-transport.md](party-transport.md) | `PartyWin.dll` shim: API surface, mesh discovery, UDP wire format, reliability layer, threading |
| [game-integration.md](game-integration.md) | What the exe expects: import set, call order, state-change layouts, mesh start, −1C/−14C, ordinals, open threads |
| [sdk-fidelity.md](sdk-fidelity.md) | Genuine Microsoft DLLs vs the shims: versions, exports, error codes, deliberate deviations |

**Version basis.** The game-side facts (RVAs, offsets, struct layouts) are for
`granblue_fantasy_relink.exe` **v2.0.5** (image base `0x140000000`, `RVA = VA − 0x140000000`).
The SDK comparison uses the shipping `PartyWin.dll.ms` **1.10.12** (`1.10.2509.24002`) and
`PlayFabMultiplayerWin.dll.ms` **1.8.0** (`1.8.2506.05002`). A game patch can move any of these.

**Sources.** Cleaned up from `dev/ghidra-notes/*.md` and `dev/paseo-prompts/*.md`
(2026-09-06 .. 2026-09-16), cross-checked against the source in this repo. The originals are
kept in the `dev` tree for provenance; where a detail here matters, the original file name is
named in the doc.

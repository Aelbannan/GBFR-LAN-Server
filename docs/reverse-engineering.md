# Reverse engineering (patch recovery)

How the game-side facts in these docs were derived, and how to re-derive them after a game
patch. The game ships no symbols: everything here comes from the binaries themselves.

## Assets

| Asset | Notes |
|---|---|
| `granblue_fantasy_relink.exe` | image base `0x140000000`; `RVA = VA − 0x140000000`; raw file offset = `VA − 0x140000C00` (v2.0.5) |
| `PartyWin.dll.ms` | the original Microsoft DLL, 1.10.12 (`1.10.2509.24002`), base `0x180000000`. `install.ps1` backs it up here as `<name>.ms`; `.rdata`: `RVA = file offset + 0x1000` |
| `PlayFabMultiplayerWin.dll.ms` | original 1.8.0 (`1.8.2506.05002`), base `0x180000000`. `.rdata`: `RVA = file offset + 0x1800` |
| `.pdata` | use it for real function bounds; Ghidra often has no function where pdata does |

**No PDBs.** Both Microsoft DLLs carry debug directories; the public symbol server returns
404 (`msdl.microsoft.com/download/symbols/<name>.pdb/<GUID><age>/<name>.pdb`) and
`symbols.nuget.org` 403. GUIDs: Party `BFA1666B-2F74-4888-A39F-B38D4C790482` age 1;
PlayFab Multiplayer `30E8A15E0F0A2D4EA6255B76C0437C66` age 1. Layouts come from the public
headers/docs + the binaries' own code + the exe's consumers.

## Tooling

`dev/ghidra-notes/` (kept for provenance) contains the working set:

- **`an_tool.py`** — the primary helper (Capstone 5; raw PE parsing):
  `python an_tool.py fn <va>` (pdata bounds + disassembly), `range <va> <len>`, `xrefs <va>`,
  `str <va>`.
- `disasm_*.py`, `scan_*.py`, `find_iat_calls.py`, `pdata_lookup.py`, `decode_config_url.py`,
  `dump_*.py`, `func_info.py` — one-off scans kept as evidence. `scan_*.py` names map to the
  features they audited.
- `granblue_fantasy_relink_exe_pe.txt` and `*_dll_pe.txt` — parsed PE section/import/export
  tables (read these instead of re-parsing).
- Ghidra project `Granblue Fantasy Relink` + `mcp-1c14c/` decompiles for structural work;
  the MCP extension is a Ghidra-version-specific install.

## Method

1. **Authority order:** exe bytes > genuine DLL bytes > wire logs > SDK prose/docs.
2. **Cite evidence** for every claim: RVA + raw bytes, or `file:line`. Label each finding
   `verified` / `probable` / `speculative`.
3. **Find real bounds** with `.pdata`; function-entry scans use `E8 rel32`; import use goes
   through the compiler thunks (`jmp qword ptr [rip+…]`, `FF 25`) — each import has exactly
   one thunk.
4. **Read the consumer** (the exe handlers) before trusting a struct layout; write only the
   fields it reads.

### Gotchas

- `an_tool.py xrefs()` walks the dword array by byte offsets and only tests 4-byte-aligned
  RIP-relative displacements — it can miss unaligned references. Do a byte-shifted scan of
  the whole image when a reference count matters.
- Ghidra has gaps where CodeView/pdata says a function exists (e.g. the WS receive range
  `0x142910040`, the `work+0x70 = 1` store near `0x14025C420`); `create_function` can fail
  because the previous function's linear sweep overlaps. Disassemble from the pdata start
  with `an_tool.py` instead.
- Capstone can fail to disassemble arbitrary offsets; fall back to raw byte parsing and say
  so rather than guessing.
- The v2.0.5 exe is not the v2.0.1 one the oldest notes were written against; re-check every
  address before reusing it.

## Re-derive after a patch

In rough order (stop at the first break; each item backs a documented behavior):

| Surface | What to find | Where the current value lives |
|---|---|---|
| exe IAT | 21 Party + 20 PlayFab Multiplayer imports, no delay imports | docs/sdk-fidelity.md |
| Party dispatch | 13-entry jump table for Party state-change types | table `0x146199C28`, dispatch `0x14025D392` |
| PlayFab dispatch | 13-entry jump table for lobby state-change types | table `0x146199D04`, dispatch `0x14025D392` |
| Type-7 gates | lock flag, member loop, key-list loops, `id_container` branch | `0x143B488B1`, `0x143B4894B`, `0x143B4ADBE`/`0x143B48F4D`, `0x3B49037` |
| Connect gate | `cmp byte [work+0x70],0` → skip/Create/Connect | `0x142901549`..`0x1429015DB`; pump `FUN_1429010F0` |
| Work ctor / `+0x70` | `+0x70=0`, `+0xb8=30000`; the only `+0x70=1` store | `FUN_140AE4C80`; writer `0x1402554E0` in `FUN_140253B50` |
| Mesh leaf | guards + the two callers | `FUN_14025FD50`; callers `FUN_1428E1180`, `FUN_143B2C8E0` |
| Ordinals | slot/sequence writers; `CanMatchingSetting` compare | `FUN_1403A13C0` (`0x3A1987`/`0x3A199A`), `0x1D7BB10`, `+0x6cce8` selector |
| Party send | single call site and options trace | `0x142513CA7`, `0x142513C50`/`0x142513C65` |
| Party lifecycle | host create, guest connect, completion | `0x142517E70`, `0x142517FD0`, `0x1425147A0` |
| PlayFab login | `createAccount=1`, entity token hand-off | `0x143A1CB20`, `0x143A1DC10` |
| Cygames | boot host/key, `user_auth` parser, WS parser/send | host strings + `0x143B29E00`, `0x14290B160`, `0x14290E350` |
| Boot config | 17-key parse table | `0x145AA8A38` |
| Lobby keys | custom/search/member key pointer tables | `0x145F3D9D0` / `0x145F3DA50` / `0x145F3DB00` |

## Verification recipes

- **Dispatch table:** read 13 dwords at the table VA; each is a 4-byte offset applied to the
  table base in the exe's jump (`mov eax,[rdx]`); each target is a handler. Confirm type
  numbers against the layout table in `game-integration.md`.
- **Import census:** parse the IAT with the PE helper; confirm the two shim export sets still
  match exactly. A new import is a hard failure the shim cannot mask.
- **Struct size sanity:** `PartyNetworkDescriptor` is `0x165` bytes in the genuine DLL
  (`memset` before parse); the exe's next field after `party+0xa0` is at `+0x208`. If a patch
  changes this, the shim's 357-byte struct no longer lands in the ABI padding.
- **Stores/reads:** before writing a new emulation, disassemble the handler and list the
  offsets it reads; write exactly those (extra fields are usually harmless, wrong ones are
  not).
- **Log-backed cross-check:** after any change, run and read `diagnostics.md`'s healthy-run
  checklist; the shims log the first step that changed instead of failing silently.

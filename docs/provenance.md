# Provenance

Where the consolidated docs came from. The originals live in `dev/ghidra-notes/` (plus
`dev/paseo-prompts/` for the task definitions) and are the raw evidence record: RVAs, byte
dumps, per-run logs and full finding tables. These reference docs carry the distilled,
verified conclusions; the mapping below is what allows the originals to be pruned.

Status: **active** = content distilled into the reference docs; **superseded** = kept for
history only (removed hacks, duplicate audits, intermediate states); **partial** = open
threads remain in the reference doc.

## Primary mapping

| Original note(s) | Consolidated into | Status |
|---|---|---|
| `LAN_ARCHITECTURE`, `LAN_FINDINGS_VERIFIED`, `PHASE_REVIEW` | architecture (stock + local flows) | active |
| `LOBBY_AND_WS`, `NUMBER_KEY9`, `LOBBY_SHAPE_SPEC`, `LOBBY_STRUCTURE` | architecture (lobby model), game-integration (type-7) | active |
| `AUDIT_LAN_SERVER`, `FIND_LOBBY_FILTER` | architecture (broker endpoints, filter grammar) | active |
| `CYGAMES_VS_STUB`, `PLAYFAB_VS_STUB` | architecture (stock flow), sdk-fidelity | active |
| `E2E_WS_ACK`, `E2E_PLAYFAB`, `E2E_MATCHING_TIMER` | architecture, diagnostics | active |
| `PARTY`, `PARTY_RELIABILITY_CONTRACT` | party-transport (transport, reliability), game-traffic | active |
| `AUDIT_PARTY`, `_recovered_dup_party_audit`, `_recovered_dup2_party_audit` | party-transport, sdk-fidelity | active (dupes superseded) |
| `TRANSPORT_REVIEW` | party-transport (threading/limits) | active (findings since fixed) |
| `E2E_PARTY`, `SOLO_INSTRUMENTS` | party-transport, diagnostics (probe tokens) | active |
| `ROOT_CAUSE_1C_14C`, `1C14C_EVIDENCE_2026-09-15`, `E2E_1C14C`, `FABLE_1C_INVESTIGATE` | game-integration (mesh start, −1C/−14C) | active |
| `MESH_START_TRIGGER`, `MESH_START_LEAF_TRIGGER_2026-09-15` | game-integration (mesh start) | active / superseded (leaf-call hack removed) |
| `MODE_BYTE_1479F6958`, `HACKS_SESSION_MODE` | game-integration (pump), historical | active / superseded |
| `GUEST_ORDINAL_MISMATCH`, `ORDINAL_CHAIN_AUDIT` | game-integration (ordinals) | partial (open thread) |
| `ENDPOINT_IDENTITY` | game-integration (entity identity) | active |
| `AUDIT_HOST_TEARDOWN`, `PIPELINE_AUDIT_2026-09-15`, `BROKER_QUEST_GAP`, `NATIVE_STACK`, `TRAFFIC_DIFF_JOIN` | game-integration, diagnostics | active |
| `SERVICE_SURFACE_DIFF` | game-integration (service gates) | partial |
| `CHANGE_EMITTER` | game-integration, sdk-fidelity (type-7/ordering) | active |
| `QUEST_*` (`CONNECTING_WAIT`, `FLOW_MAP`, `LOAD_STATE_6C814`, `MATCHING_BRANCH`, `REGRESSION_HUNT`, `SEQUENCE_GAP`, `SESSION_GUARD`, `START_FLOW`) | game-integration (quest pipeline), diagnostics | partial |
| `LOAD_*` (`LATCH_ROOT`, `SCREEN_PIPELINE`, `STALL_FLOW`), `SESSION_UPDATE_STALL` | game-integration, game-traffic (load gate) | partial |
| `ASYNC_OP_A596AF73`, `ASYNC_QUEUE_DISPATCH`, `MODE3_ARMING` | game-integration (open thread), diagnostics | partial (open thread) |
| `TEARDOWN_FOLLOWUP_IDCONTAINER`, `TEARDOWN_FOLLOWUP_READYLIST`, `STATIC_PASS_ADD_SEND_2026-09-15` | game-integration / internal, historical | superseded |
| `APPLY_STUB_FIX`, `STEAM_RESOURCE_LOAD`, `JOB_WORKER_SHIM_CALLEES`, `INSTRUMENTATION_SEARCHPROP` | reverse-engineering / diagnostics | active (reference value) |
| `MS_DLL_GROUND_TRUTH`, `ORIGINAL_MS_DLLS_VS_SHIMS`, `ERROR_CODE_FIDELITY` | sdk-fidelity | active |
| `AUDIT_PLAYFAB` | sdk-fidelity, game-integration | active |
| `HACKS_CRASH_1C`, `HACKS_HAPPY_PATH`, `HACKS_MISS_PATH` | game-integration (what not to do), historical | superseded |
| `FIX_BACKLOG_2026-09-15` | superseded by `TODO.md` + the fixes themselves | superseded |
| `README.md` (lan-stub) | gbfr-lan README + architecture/diagnostics | active |
| `paseo-prompts/*` | method + provenance for reverse-engineering | active (tasking side) |
| `an_tool.py`, `disasm_*.py`, `scan_*.py`, `*_pe.txt`, `mcp-1c14c/`, `_decompile_compare/` | reverse-engineering tooling | active |

## Out of scope

These `ghidra-notes` files belong to other mods/experiments and were deliberately not
consolidated here: `FIFTH_*` (fifth-player/`more_players`), `SLOT_GATES`, `ENDLESS_MODE`
(Conflux investigation), `CRASH_8E8` (`more_players.dll` crash). `NUMBER_KEY9` is in scope
(it is the lobby search key, not the keyboard).

## If the originals are pruned

The new docs distill conclusions and cite the addresses that matter, but they do **not**
reproduce every raw byte dump, per-run log excerpt, or superseded finding. Before deleting
`ghidra-notes/`:

1. Keep `reverse-engineering.md` and the tooling it names, or the next game patch starts from
   zero.
2. Keep the genuine DLLs (`*.ms`) — they are the ground truth for future fidelity work.
3. If a claim here ever needs to be re-litigated, the mapping above points at the original
   report; a pruned note cannot be recovered from these docs alone.

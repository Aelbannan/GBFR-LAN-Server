# TODO / known debt

Notes from a code review, roughly ordered by value. None of these are needed for the mod to
work as-is.

## Done (kept for the record)

- [x] **Tests.** `run_tests.ps1` builds all four components and runs every suite. New coverage:
  `common/json_test`, `common/http_test`, `lan-server` integration/framing tests, `party-shim/wire_test`,
  `steam-http-shim/pe_test`. CI runs the runner plus `cargo fmt --check` and `cargo clippy -D warnings`.
- [x] **lan-server: cap request bodies.** `read_request` refuses a declared body over
  `MAX_BODY_BYTES` (1 MB) with a 413 before reading a byte of it; the per-read header timeout
  remains a slowloris bound rather than a hard one.
- [x] **lan-server: cap concurrent connections.** `MAX_CONNECTIONS` (256) with an RAII slot per
  accepted socket; over the cap the connection gets a 503 instead of a thread.
- [x] **lan-server: WebSocket idle timeout.** `pump_websocket` uses a 900 s read timeout
  (`ServerOpts::ws_idle`, 300 ms in tests) instead of `None`.
- [x] **shims: cap HTTP response reads.** Both shims use `common/http.rs`: 1 s connect cap and a
  4 MB response cap, with the status/body parsing in one tested place.
- [x] **Shared JSON accessors.** `common/json.rs` replaces both hand-rolled copies (the
  `json_str` drift is gone); lookups handle escapes, nesting and malformed input.
- [x] **Panic strategy decided.** Keep unwind. The Party shim's transport and broker threads use
  `catch_unwind` and fall back to the inline path; `panic = "abort"` would turn a recoverable
  worker panic into a game crash. (An earlier review suggested `abort`; that was wrong.)
- [x] **Poisoned-mutex recovery.** `g_lock` / `mp_lock` / `state_lock` recover instead of
  `.lock().unwrap()`, so a panic while a lock was held cannot turn every later export into an
  abort across the FFI boundary.
- [x] **Single build path.** Root Cargo workspace; each `build.ps1` builds its member and copies
  the artifact. The raw-`rustc` build was removed (it produced a different binary from the same
  source, and only one path can be the shipping one).
- [x] **Formatting and lints.** Tree is `cargo fmt` clean; `cargo clippy --workspace --release`
  is warning-free; rustc warnings are gone (private-interface lints, dead fields, unused imports).
- [x] **install.ps1: stale backups.** A shipping DLL that differs from the existing `.ms` backup
  is re-backed-up, so a game update no longer loses the new original.
- [x] **`async_broker_test` in CI** (via the runner), plus `pfqueue_smoke` and `wire_test`.

## Known leak (measure before fixing)

- [ ] **State-change allocations are never freed.** Both shims `alloc_zeroed` every state change
  (and playfab's interned key/value arrays) in `alloc_sc` / `intern_cstr_list` /
  `member_update_entries`, and there is no free path at all — FinishProcessing reclaims the batch
  counters but not the memory. Verify whether the title retains any of these after Finish;
  if not, free at Finish with the layout they were allocated with. Party type-21 changes copy up
  to 64 KiB of payload, so this grows with quest traffic. At minimum, measure RSS over a long
  session and decide.

## Refactors (maintainability, not bugs)

- [ ] **Split the large files.** `party-shim/src/lib.rs` (~5.6k lines), `playfab-mp-shim/src/lib.rs`
  (~3.6k), `lan-server/src/main.rs` (~2.2k). Concretely:
  - about a third of `party-shim/src/lib.rs` is diagnostics (`solo_sample_note` 165 lines,
    `probe_session` 141, `mode3_gate_note` 104, `log_payload` 98, `asyncload_note` 63,
    `quest_guard_note` 55, `note_eventbus` 52, ...): move the debug-gated probes into
    `src/diagnostics.rs`, or delete the one-off RE instruments;
  - `lan-server`: split `handle_playfab` (496 lines) and `handle_party` (172) by route, and fold
    `load_lobby_ini` into `common/lan_cfg.rs` (also drops the second `parse_bool`);
  - `steam-http`: split `winhttp_execute` (153 lines) into URL build / send / response parse.
- [ ] **`lan-server`: fold `[lobby]` into `LanCfg`.** `load_lobby_ini` is a second ini parser next
  to `common/lan_cfg.rs`; the server pre-scans `--ini` and sets `GBFR_LAN_INI` so the shared loader
  sees it, which is easy to break.
- [ ] **Pre-upgrade WebSocket connections have no header timeout** (`header_timeout = None` on the
  WS port, deliberately: WinHTTP connects at `user_auth` and sends the upgrade much later). The
  connection cap bounds the damage; a generous timeout would bound it better, but it must be long
  enough not to cut a player who is idling at the multiplayer counter.
- [ ] **Delete or archive `dev/lan-stub`.** It is a stale pre-git copy of the same stack (different
  sources, old install scripts, scratch dirs). Editing the wrong tree is the failure mode.

## Legal / packaging

- [ ] **LICENSE.** Not chosen yet.
- [ ] **Decide what ships.** Built DLLs/EXEs are gitignored; `lan.ini` is tracked.

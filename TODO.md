# TODO / known debt

Notes from a code review, roughly ordered by value. None of these are needed for the mod to
work as-is.

## Hardening (trusted-LAN threat model)

The design assumes the network is trusted (plain HTTP, no auth, no TLS), so these improve
failure modes — they do not create a security boundary.

- [ ] **lan-server: cap request bodies.** `read_request` trusts `Content-Length` and grows the
  body unbounded (`main.rs:1846`); the 1 MB guard only covers headers. Reject/close oversize
  bodies (the game never posts anything close to 1 MB).
- [ ] **lan-server: cap concurrent connections.** `listen()` spawns a thread per accepted socket
  with no limit; combined with the next item a peer can pile up threads without sending a byte.
- [ ] **lan-server: WebSocket idle timeout.** Connections on the WS port get
  `header_timeout = None` (`main.rs:1874`) and `set_read_timeout(None)` in `pump_websocket`, so an
  idle socket holds a thread forever. Add a generous idle timeout (the game reconnects).
- [ ] **shims: cap HTTP response reads.** `http_json_status` in `party-shim` and
  `playfab-mp-shim` uses `read_to_end` with no size limit; the 3 s read timeout is per read, not
  overall.

## Known leak (measure before fixing)

- [ ] **State-change allocations are never freed.** Both shims `alloc_zeroed` every state change
  (and playfab's interned key/value arrays) in `alloc_sc` / `intern_cstr_list` /
  `member_update_entries`, and there is no free path at all — FinishProcessing reclaims the batch
  counters but not the memory. Verify whether the title retains any of these after Finish;
  if not, free at Finish with the layout they were allocated with. Party type-21 changes copy up
  to 64 KiB of payload, so this grows with quest traffic. At minimum, measure RSS over a long
  session and decide.

## Refactors (maintainability, not bugs)

- [ ] **party-shim JSON scrapers.** `json_str` / `json_num` / `json_arr_objects` (~line 992) are
  only used for `/party/peers` (entity_id/ip/udp_port). If the broker contract grows, share the
  escape-aware parser from `playfab-mp-shim` via `common/` instead of extending the scrapers.
- [ ] **Split the large files.** `party-shim/src/lib.rs` (5.6k lines),
  `playfab-mp-shim/src/lib.rs` (3.1k), `lan-server/src/main.rs` (2.0k). Start with the
  game-memory probes and broker threads in party, and service routing in the server.
- [ ] **Build hygiene.** Clean or gate the build warnings (function casts, private interfaces,
  unused assignments); decide `cargo` vs raw `rustc` for the shims — the `Cargo.toml` files exist
  but the documented build bypasses them, and `panic=abort` would be appropriate for a cdylib.
- [ ] **LICENSE.** Not chosen yet.

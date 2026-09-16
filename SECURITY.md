# Security

## Threat model: trusted LAN

gbfr-lan replaces the game's online services with plain HTTP/WebSocket and plain UDP. There is
**no authentication, no encryption, and no authorization**. The design assumes every host on
the network is trusted; it is a convenience boundary for friends on the same LAN or VPN, not a
security boundary.

Do not expose it. In particular:

- Do **not** port-forward TCP 8080/8081 or UDP 27015.
- Keep the firewall rules from `open_firewall.ps1` scoped to the **Private** profile.
- For play across the internet, use a VPN that provides a virtual LAN (Tailscale, ZeroTier,
  Radmin, WireGuard, …) — the traffic is still unauthenticated, but it is not reachable from
  the public internet.

## What is unauthenticated

| Surface | Exposure |
|---|---|
| Broker HTTP (TCP 8080) | any endpoint: Cygames REST, PlayFab lobby REST, party registry, boot config. Anyone who can reach the port can create/join/leave lobbies, register peers and read the lobby list |
| Cygames WebSocket (TCP 8081) | accepted without credentials; attempts are echoed |
| Party UDP mesh | `GBFR` datagrams carry no MAC/nonce/signature; any process that can reach the port can HELLO, be treated as a remote endpoint, and inject messages into the game session |
| `lan.ini`, `GBFR_LAN_STUB` | plain configuration, no secrets to steal |

An attacker on the same segment can therefore impersonate a player, inject or drop game
messages, fill the 8-remote cap with bogus entities, and deny service. The hardening backlog
for resource limits (unbounded request bodies/connections, WS idle timeout, response reads) is
tracked in `TODO.md`.

## What is not at risk

- No real PlayFab/Cygames session is used or proxied: Steam tickets are stubbed
  (`LANSTUB|…`), tokens are generated locally, and no live service is contacted.
- No game assets or SDK binaries are distributed; the genuine Microsoft DLLs are only backed
  up (`*.ms`) on the player's own disk.
- The stack does not elevate: `install.ps1` copies files into the game folder;
  `open_firewall.ps1` is the only Administrator step and only edits the Private profile.
- Personal data is limited to what the game itself puts on the wire (player name, platform
  account id). It stays between the players; nothing is uploaded.

## Reporting

There is no security boundary to bypass, so run-of-the-mill spoofing/injection on your own LAN
is expected behavior, not a vulnerability. Please open a GitHub issue for:

- any way the stack ends up reachable from a public network (e.g. a default that binds or
  advertises beyond the LAN),
- resource exhaustion reachable by an unauthenticated peer (see `TODO.md` for the known ones),
- anything that writes outside the game folder or the repo.

Vulnerabilities in the game, the Steam emulator, or the Microsoft SDKs are not ours — report
those upstream.

//! Tests for `party-shim/src/wire.rs` — the UDP header the shim writes and reads.
//!
//! Build with plain rustc (the shim's documented build):
//!     rustc --edition 2021 -O -o wire_test.exe tests/wire_test.rs
//!     .\wire_test.exe
//!
//! The offsets here are the contract between `send_udp_all` and `recv_udp`; a change to one side
//! without the other is what makes peers silently ignore each other, so every field is asserted
//! byte-for-byte in both protocol versions, and every truncation of a valid datagram is fed back
//! through `parse` to prove it cannot index out of bounds.

use std::sync::atomic::{AtomicU32, Ordering};

#[path = "../src/wire.rs"]
mod wire;

use wire::*;

static FAILURES: AtomicU32 = AtomicU32::new(0);

fn check(name: &str, ok: bool, detail: &str) {
    if ok {
        println!("PASS  {name}  {detail}");
    } else {
        println!("FAIL  {name}  {detail}");
        FAILURES.fetch_add(1, Ordering::SeqCst);
    }
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn main() {
    // ── v3 layout ────────────────────────────────────────────────────────────────────────
    const ID: &str = "net-abcdef";
    const ENT: &[u8] = b"entity-1";
    let payload = [0xAAu8; 7];
    let pkt = wire::packet(ID, ENT, KIND_MSG, 0x1 | 0x2, 42, 41, &payload, true);
    check(
        "reliable datagram length is 60 + payload",
        pkt.len() == 60 + payload.len(),
        &format!("len={}", pkt.len()),
    );
    check(
        "magic at 0..4",
        &pkt[0..4] == b"GBFR",
        &format!("{:?}", &pkt[0..4]),
    );
    check("kind at 4", pkt[4] == KIND_MSG, &format!("{}", pkt[4]));
    check("version at 5 is 3", pkt[5] == 3, &format!("{}", pkt[5]));
    check(
        "network id at 8, NUL padded",
        &pkt[8..18] == ID.as_bytes() && pkt[18..24].iter().all(|b| *b == 0),
        &format!("{:?}", &pkt[8..24]),
    );
    check(
        "entity id at 24, NUL padded",
        &pkt[24..32] == ENT && pkt[32..44].iter().all(|b| *b == 0),
        &format!("{:?}", &pkt[24..44]),
    );
    check(
        "payload length at 44",
        le32(&pkt, 44) == payload.len() as u32,
        "",
    );
    check("options at 48", le32(&pkt, 48) == 0x3, "");
    check("sequence at 52", le32(&pkt, 52) == 42, "");
    check("ack at 56", le32(&pkt, 56) == 41, "");
    check(
        "payload starts at 60",
        pkt[60..] == payload[..],
        &format!("{:?}", &pkt[60..]),
    );

    // ── v2 (pre-reliability) layout ──────────────────────────────────────────────────────
    let legacy = wire::packet(ID, ENT, KIND_MSG, 0, 5, 99, &payload, false);
    check(
        "legacy datagram length is 56 + payload",
        legacy.len() == 56 + payload.len(),
        &format!("len={}", legacy.len()),
    );
    check("legacy version at 5 is 2", legacy[5] == 2, "");
    check(
        "legacy payload starts at 56 (no ack field)",
        legacy[56..] == payload[..],
        &format!("{:?}", &legacy[56..]),
    );
    check(
        "legacy datagram carries no ack",
        le32(&legacy, 44) == payload.len() as u32,
        "",
    );

    // ── round trip through parse ─────────────────────────────────────────────────────────
    for reliable in [true, false] {
        for (kind, opts, seq, ack, plen) in [
            (KIND_HELLO, 0u32, 0u32, 0u32, 0usize),
            (KIND_MSG, 0x1, 1, 0, 1),
            (KIND_MSG, 0x1 | 0x2, 7, 6, 1249),
            (KIND_MSG, 0x10, u32::MAX, 3, MAX_PAYLOAD),
            (KIND_ACK, 0, 0, 9, 0),
        ] {
            let body = vec![0x5Au8; plen];
            let pkt = wire::packet(ID, ENT, kind, opts, seq, ack, &body, reliable);
            match wire::parse(&pkt, reliable) {
                Ok(p) => check(
                    &format!("round trip reliable={reliable} kind={kind} plen={plen}"),
                    p.kind == kind
                        && p.options == opts
                        && p.seq == seq
                        && p.payload_len == plen
                        && p.ack == (if reliable { ack } else { 0 })
                        && wire::entity(&pkt) == "entity-1"
                        && wire::network_id(&pkt) == ID,
                    &format!("{p:?}"),
                ),
                Err(e) => check(
                    &format!("round trip reliable={reliable} kind={kind} plen={plen}"),
                    false,
                    &format!("{e:?}"),
                ),
            }
        }
    }

    // ── ids ──────────────────────────────────────────────────────────────────────────────
    let long_id = "0123456789ABCDEFGHIJ"; // 20 chars: must truncate to 16
    let pkt = wire::packet(long_id, &[0xFFu8; 30], KIND_MSG, 0, 0, 0, &[], true);
    check(
        "an over-long network id truncates to 16 bytes",
        wire::network_id(&pkt) == "0123456789ABCDEF",
        &wire::network_id(&pkt),
    );
    check(
        "an over-long entity id truncates to 20 bytes",
        wire::entity(&pkt).chars().count() == ENT_LEN - 1,
        &format!("chars={}", wire::entity(&pkt).chars().count()),
    );
    let pkt = wire::packet("", &[], KIND_HELLO, 0, 0, 0, &[], true);
    check(
        "empty ids read back empty",
        wire::network_id(&pkt).is_empty() && wire::entity(&pkt).is_empty(),
        "",
    );
    check(
        "entity/network_id tolerate a truncated buffer",
        wire::entity(&[0u8; 4]).is_empty() && wire::network_id(&[0u8; 4]).is_empty(),
        "",
    );

    // ── parse rejections ─────────────────────────────────────────────────────────────────
    let good = wire::packet(ID, ENT, KIND_MSG, 0, 1, 0, &[1, 2, 3], true);
    check(
        "one byte short of the header is TooShort",
        wire::parse(&good[..59], true) == Err(ParseError::TooShort),
        "",
    );
    check(
        "empty is TooShort",
        wire::parse(&[], true) == Err(ParseError::TooShort),
        "",
    );
    {
        let mut bad = good.clone();
        bad[0] = b'X';
        check(
            "bad magic",
            wire::parse(&bad, true) == Err(ParseError::BadMagic),
            "",
        );
    }
    {
        let mut bad = good.clone();
        bad[5] = 2;
        check(
            "v2 header into a v3 build is BadVersion",
            wire::parse(&bad, true) == Err(ParseError::BadVersion),
            "",
        );
    }
    check(
        "a v3 header into a v2 build is BadVersion",
        wire::parse(&good, false) == Err(ParseError::BadVersion),
        "",
    );
    {
        let mut bad = good.clone();
        bad[44..48].copy_from_slice(&(MAX_PAYLOAD as u32 + 1).to_le_bytes());
        check(
            "payload beyond MAX_PAYLOAD",
            wire::parse(&bad, true) == Err(ParseError::BadPayloadLen),
            "",
        );
    }
    {
        let mut bad = good.clone();
        bad[44..48].copy_from_slice(&1000u32.to_le_bytes());
        check(
            "payload longer than the datagram",
            wire::parse(&bad, true) == Err(ParseError::BadPayloadLen),
            "",
        );
    }
    check(
        "legacy parse of a legacy datagram",
        wire::parse(&legacy, false).is_ok() && wire::parse(&legacy, false).unwrap().ack == 0,
        "",
    );

    // ── every truncation must be an Err, never a panic or an out-of-bounds read ──────────
    let mut survived = 0usize;
    for n in 0..=good.len() {
        let part = &good[..n];
        let _ = wire::parse(part, true);
        let _ = wire::entity(part);
        let _ = wire::network_id(part);
        survived += 1;
    }
    check(
        "all 64 truncations of a datagram parse without panicking",
        survived == good.len() + 1,
        &format!("{survived} cases"),
    );
    // Garbage with a valid magic and a huge declared length.
    let mut hostile = vec![b'G', b'B', b'F', b'R', 2, 3];
    hostile.extend_from_slice(&[0u8; 60]);
    hostile[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
    check(
        "declared payload length u32::MAX is rejected",
        wire::parse(&hostile, true) == Err(ParseError::BadPayloadLen),
        "",
    );

    // ── pure helpers ─────────────────────────────────────────────────────────────────────
    check(
        "u32le in bounds",
        wire::u32le(&[1, 2, 3, 4], 0) == Some(0x04030201),
        "",
    );
    check(
        "u32le out of bounds is None",
        wire::u32le(&[1, 2, 3], 0).is_none(),
        "",
    );
    check(
        "hex_preview caps and formats",
        wire::hex_preview(&[0x0A, 0xFF, 0x01], 2) == "0a ff",
        &wire::hex_preview(&[0x0A, 0xFF, 0x01], 2),
    );
    check(
        "hex_preview of nothing",
        wire::hex_preview(&[], 4).is_empty(),
        "",
    );

    {
        let mut p = vec![0u8; 0x20];
        p[0..4].copy_from_slice(&2u32.to_le_bytes());
        p[4..8].copy_from_slice(&9u32.to_le_bytes());
        p[16..20].copy_from_slice(&1u32.to_le_bytes());
        let h = wire::payload_hints(&p);
        check(
            "payload_hints reads op/sub/dword",
            h.contains("op=2")
                && h.contains("dispatch")
                && h.contains("sub=9")
                && h.contains("dword@+10=1"),
            &h,
        );
    }
    check(
        "payload_hints flags the chara blob size",
        wire::payload_hints(&vec![0u8; 0x2FC]).contains("sz~0x2FC_chara"),
        &wire::payload_hints(&vec![0u8; 0x2FC]),
    );
    check(
        "payload_hints flags the CPU chara size",
        wire::payload_hints(&vec![0u8; 0x14 + 0x2FC]).contains("sz~cpu_chara"),
        &wire::payload_hints(&vec![0u8; 0x14 + 0x2FC]),
    );
    check(
        "payload_hints on an empty payload is empty",
        wire::payload_hints(&[]).is_empty(),
        "",
    );

    check(
        "is_chara_snapshot: op2 >= 600",
        wire::is_chara_snapshot(2, 600),
        "",
    );
    check(
        "is_chara_snapshot: op2 < 600",
        !wire::is_chara_snapshot(2, 599),
        "",
    );
    check(
        "is_chara_snapshot: op3 is never one",
        !wire::is_chara_snapshot(3, 4000),
        "",
    );

    check(
        "ip_string",
        wire::ip_string([10, 0, 0, 5]) == "10.0.0.5",
        "",
    );
    check(
        "is_loopback_ip: 127.0.0.1",
        wire::is_loopback_ip("127.0.0.1"),
        "",
    );
    check(
        "is_loopback_ip: 127.1.2.3",
        wire::is_loopback_ip("127.1.2.3"),
        "",
    );
    check(
        "is_loopback_ip: 0.0.0.0",
        wire::is_loopback_ip("0.0.0.0"),
        "",
    );
    check(
        "is_loopback_ip: 10.0.0.5 is not",
        !wire::is_loopback_ip("10.0.0.5"),
        "",
    );
    check(
        "is_loopback_ip trims",
        wire::is_loopback_ip("  127.0.0.1 "),
        "",
    );

    {
        let (id, ip) = wire::parse_lan1("LAN1.abc123.0a000005");
        check(
            "parse_lan1 reads the packed ip",
            id == "abc123" && ip == Some([10, 0, 0, 5]),
            &format!("{id} {ip:?}"),
        );
        let (id, ip) = wire::parse_lan1("LAN1.abc123");
        check(
            "parse_lan1 without an address",
            id == "abc123" && ip.is_none(),
            &format!("{id} {ip:?}"),
        );
        let (id, ip) = wire::parse_lan1("plain-id");
        check(
            "parse_lan1 accepts a bare id",
            id == "plain-id" && ip.is_none(),
            &format!("{id} {ip:?}"),
        );
        let (_, ip) = wire::parse_lan1("LAN1.abc.ZZZZZZZZ");
        check(
            "parse_lan1 rejects a non-hex address",
            ip.is_none(),
            &format!("{ip:?}"),
        );
        let (_, ip) = wire::parse_lan1("LAN1.abc.0a00");
        check(
            "parse_lan1 rejects a short address",
            ip.is_none(),
            &format!("{ip:?}"),
        );
        let (_, ip) = wire::parse_lan1("LAN1.abc.ffffffff");
        check(
            "parse_lan1 handles 255.255.255.255",
            ip == Some([255, 255, 255, 255]),
            "",
        );
    }

    // ── multibyte-safe truncation (byte slicing would panic here) ────────────────────────
    {
        let s = "グranblueファンタジー";
        let mut ok = true;
        for n in 0..=s.chars().count() + 2 {
            let t = wire::truncate_log(s, n);
            if t.chars().count() > n {
                ok = false;
            }
        }
        check(
            "truncate_log never exceeds the char budget and never panics",
            ok,
            "",
        );
        check(
            "truncate_log of a short string is identity",
            wire::truncate_log("abc", 5) == "abc",
            "",
        );
    }

    let failures = FAILURES.load(Ordering::SeqCst);
    if failures > 0 {
        println!("{failures} FAILURE(S)");
        std::process::exit(1);
    }
    println!("all party wire checks passed");
}

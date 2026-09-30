//! Pure UDP wire-format helpers for the Party transport (no Win32 calls, no logging).
//!
//! Compiled twice: into `PartyWin.dll` (`mod wire;` in `lib.rs`) and into `tests/wire_test.rs`, so
//! the layout tests exercise exactly the packing and parsing the shim runs.
//!
//! Header layout (offsets are stable between v2 and v3; v3 adds the trailing cumulative ack):
//!
//! ```text
//!   0..4    magic "GBFR"
//!   4       kind        1 = HELLO, 2 = message, 3 = standalone ack
//!   5       version     3 = reliable build, 2 = pre-reliability build
//!   6..8    reserved    zero
//!   8..24   network id  16 bytes, NUL padded
//!   24..44  local entity id, NUL padded
//!   44..48  payload length (LE)
//!   48..52  PartySendMessageOptions (LE)
//!   52..56  sequence (LE)
//!   56..60  cumulative ack (LE, reliable build only)
//!   60..    payload (reliable) / 56.. (legacy)
//! ```

/// Datagram magic. A peer that does not send this has no compatible version; the shim logs one
/// mismatch per peer and drops the datagram (there is deliberately no legacy fallback).
pub const MAGIC: [u8; 4] = *b"GBFR";
pub const KIND_HELLO: u8 = 1;
pub const KIND_MSG: u8 = 2;
pub const KIND_ACK: u8 = 3;

pub const HDR_LEN: usize = 60;
pub const HDR_LEN_LEGACY: usize = 56;
pub const HDR_VERSION: u8 = 3;
pub const HDR_VERSION_LEGACY: u8 = 2;
pub const HDR_ACK_OFF: usize = 56;

pub const NET_ID_OFF: usize = 8;
pub const NET_ID_LEN: usize = 16;
pub const ENT_OFF: usize = 24;
/// 20 bytes of entity id plus the terminating NUL the game's ids carry.
pub const ENT_LEN: usize = 21;
pub const PAYLOAD_LEN_OFF: usize = 44;
pub const OPTIONS_OFF: usize = 48;
pub const SEQ_OFF: usize = 52;

/// Hard cap on a single datagram's payload (matches the receive path's guard).
pub const MAX_PAYLOAD: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Shorter than the header for this protocol version.
    TooShort,
    /// Missing `GBFR`.
    BadMagic,
    /// Header version is not this build's (v2 vs v3).
    BadVersion,
    /// Payload length is beyond `MAX_PAYLOAD` or beyond the datagram.
    BadPayloadLen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub kind: u8,
    pub payload_len: usize,
    pub options: u32,
    pub seq: u32,
    /// Zero in the legacy build; the ack field is not part of a v2 datagram.
    pub ack: u32,
}

pub const fn header_len(reliable: bool) -> usize {
    if reliable {
        HDR_LEN
    } else {
        HDR_LEN_LEGACY
    }
}

pub const fn version(reliable: bool) -> u8 {
    if reliable {
        HDR_VERSION
    } else {
        HDR_VERSION_LEGACY
    }
}

/// Build one datagram. One argument per header field, mirroring the wire layout: the ids are
/// truncated to their fixed fields and stay NUL padded, so a shorter id is indistinguishable
/// from one padded with zeros (as before).
#[allow(clippy::too_many_arguments)]
pub fn packet(
    net_id: &str,
    local_ent: &[u8],
    kind: u8,
    options: u32,
    seq: u32,
    ack: u32,
    payload: &[u8],
    reliable: bool,
) -> Vec<u8> {
    let hdr = header_len(reliable);
    let mut pkt = vec![0u8; hdr + payload.len()];
    pkt[..4].copy_from_slice(&MAGIC);
    pkt[4] = kind;
    pkt[5] = version(reliable);
    let ib = net_id.as_bytes();
    let n = ib.len().min(NET_ID_LEN);
    pkt[NET_ID_OFF..NET_ID_OFF + n].copy_from_slice(&ib[..n]);
    let e = local_ent.len().min(ENT_LEN - 1);
    pkt[ENT_OFF..ENT_OFF + e].copy_from_slice(&local_ent[..e]);
    pkt[PAYLOAD_LEN_OFF..PAYLOAD_LEN_OFF + 4]
        .copy_from_slice(&(payload.len() as u32).to_le_bytes());
    pkt[OPTIONS_OFF..OPTIONS_OFF + 4].copy_from_slice(&options.to_le_bytes());
    pkt[SEQ_OFF..SEQ_OFF + 4].copy_from_slice(&seq.to_le_bytes());
    if reliable {
        pkt[HDR_ACK_OFF..HDR_ACK_OFF + 4].copy_from_slice(&ack.to_le_bytes());
    }
    pkt[hdr..].copy_from_slice(payload);
    pkt
}

pub fn u32le(data: &[u8], off: usize) -> Option<u32> {
    data.get(off..off + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
}

/// Decode a datagram header. Every length is checked before it is used, so a truncated or hostile
/// datagram can only return `Err` — the receive path never indexes past the buffer.
pub fn parse(buf: &[u8], reliable: bool) -> Result<Parsed, ParseError> {
    let hdr = header_len(reliable);
    if buf.len() < hdr {
        return Err(ParseError::TooShort);
    }
    if buf[..4] != MAGIC {
        return Err(ParseError::BadMagic);
    }
    if buf[5] != version(reliable) {
        return Err(ParseError::BadVersion);
    }
    let payload_len = u32le(buf, PAYLOAD_LEN_OFF).ok_or(ParseError::TooShort)? as usize;
    if payload_len > MAX_PAYLOAD || hdr + payload_len > buf.len() {
        return Err(ParseError::BadPayloadLen);
    }
    Ok(Parsed {
        kind: buf[4],
        payload_len,
        options: u32le(buf, OPTIONS_OFF).unwrap_or(0),
        seq: u32le(buf, SEQ_OFF).unwrap_or(0),
        ack: if reliable {
            u32le(buf, HDR_ACK_OFF).unwrap_or(0)
        } else {
            0
        },
    })
}

/// Entity id of a parsed datagram: the NUL-padded 20-byte field, lossily decoded.
pub fn entity(buf: &[u8]) -> String {
    let raw = buf.get(ENT_OFF..ENT_OFF + ENT_LEN - 1).unwrap_or(&[]);
    let n = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..n]).into_owned()
}

/// Network id of a parsed datagram. Part of the wire API the tests pin, even though the
/// receive path already has the id from the `Network` it is reading for.
#[allow(dead_code)]
pub fn network_id(buf: &[u8]) -> String {
    let raw = buf.get(NET_ID_OFF..NET_ID_OFF + NET_ID_LEN).unwrap_or(&[]);
    let n = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..n]).into_owned()
}

pub fn hex_preview(data: &[u8], n: usize) -> String {
    data.iter()
        .take(n)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Char-boundary-safe truncation for log lines (byte slicing panics on multibyte input).
pub fn truncate_log(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

/// Human hints for one game payload: opcode/sub-opcode and the sizes the RE notes identified.
pub fn payload_hints(data: &[u8]) -> String {
    let len = data.len();
    let op = u32le(data, 0);
    let sub = u32le(data, 4);
    let slot = u32le(data, 0x10);
    let mut tags = Vec::new();
    if let Some(op) = op {
        tags.push(format!("op={op}"));
        if op == 2 {
            tags.push("dispatch".into());
        }
    }
    if let Some(sub) = sub {
        tags.push(format!("sub={sub}"));
    }
    if let Some(slot) = slot {
        if slot <= 3 {
            tags.push(format!("dword@+10={slot}"));
        }
        if slot == 4 {
            tags.push("WATCH_4 dword@+10=4".into());
        }
    }
    // Human apply memcpy is 0x3578 in-memory; wire blob is 0x2FC.
    if (0x2FC..0x2FC + 48).contains(&len) {
        tags.push("sz~0x2FC_chara".into());
    }
    // CPU: u32 slot @ +0x10, blob 0x2FC @ +0x14, name[40].
    if (0x14 + 0x2FC..0x14 + 0x2FC + 48).contains(&len) {
        tags.push("sz~cpu_chara".into());
    }
    tags.join(" ")
}

/// The host's chara snapshot: op 2 with a payload large enough to carry the 0x2FC blob.
pub fn is_chara_snapshot(op: u32, len: usize) -> bool {
    op == 2 && len >= 600
}

pub fn ip_string(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

pub fn is_loopback_ip(s: &str) -> bool {
    let s = s.trim();
    s == "127.0.0.1" || s == "::1" || s == "0.0.0.0" || s.starts_with("127.")
}

/// `LAN1.<id>.<8 hex digits>`, the opaque descriptor the lobby carries. The broker also accepts a
/// bare id, and a malformed tail yields no address rather than a bogus one.
pub fn parse_lan1(raw: &str) -> (String, Option<[u8; 4]>) {
    let rest = raw.strip_prefix("LAN1.").unwrap_or(raw);
    let mut parts = rest.split('.');
    let id = parts.next().unwrap_or(rest).to_string();
    let ip = parts.next().and_then(|h| {
        if h.len() != 8 {
            return None;
        }
        let n = u32::from_str_radix(h, 16).ok()?;
        Some([(n >> 24) as u8, (n >> 16) as u8, (n >> 8) as u8, n as u8])
    });
    (id, ip)
}

//! ABI layout and string helpers for `gbfr_http.dll`.
//!
//! These are the pieces with no Win32 calls and no global state: the exact byte layout of the
//! Steam callback structs the shim hands back to the game, and the narrow/wide string bridges.
//! They live here so `tests/pe_test.rs` can assert the offsets against the documented SDK
//! layouts instead of trusting the packing code by inspection.

use std::ffi::c_char;

/// Wide (UTF-16, NUL-terminated) form of a Rust string, for `…W` Win32 entry points.
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Narrow C string to `String`. Null yields empty; the scan is capped at 4096 bytes, so a missing
/// terminator truncates instead of walking off the end (a panic here would be a game crash).
///
/// # Safety
/// `p` must be null or point to a readable NUL-terminated byte string.
pub unsafe fn from_c(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0usize;
    while n < 4096 && *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, n)).into_owned()
}

/// Wide C string to `String`. Null yields empty; the scan is capped at 512 code units.
///
/// # Safety
/// `p` must be null or point to a readable NUL-terminated UTF-16 string.
pub unsafe fn from_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0usize;
    while n < 512 && *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

/// `HTTPRequestCompleted_t` packing used to complete an `ISteamHTTP` call.
///
/// MSVC x64 layout (steam_api.h, `HTTPRequestCompleted_t`): `uint32 m_hRequest` at +0, pad to
/// +8, `uint64 m_ulContextValue` at +8, `bool m_bRequestSuccessful` at +16, pad to +20,
/// `EHTTPStatusCode m_eStatusCode` at +20, `uint32 m_unBodySize` at +24. The struct is 32 bytes
/// with tail padding. The game reads every one of these fields, so the padding must be zero.
pub fn pack_http_completed(
    handle: u32,
    context: u64,
    ok: bool,
    status: u32,
    body_len: u32,
) -> [u8; 32] {
    let mut p = [0u8; 32];
    p[0..4].copy_from_slice(&handle.to_le_bytes());
    p[8..16].copy_from_slice(&context.to_le_bytes());
    p[16] = if ok { 1 } else { 0 };
    p[20..24].copy_from_slice(&status.to_le_bytes());
    p[24..28].copy_from_slice(&body_len.to_le_bytes());
    p
}

/// `GetTicketForWebApiResponse_t` packing: `HAuthTicket m_hAuthTicket` at +0,
/// `EResult m_eResult` at +4, `int m_cubTicket` at +8, `uint8 m_rgubTicket[2560]` at +12.
/// `ticket` is truncated to the array length and the rest stays zero.
pub fn pack_webapi_ticket(handle: u32, ticket: &[u8], max: usize) -> Vec<u8> {
    let n = ticket.len().min(max);
    let mut p = vec![0u8; 12 + max];
    p[0..4].copy_from_slice(&handle.to_le_bytes());
    p[4..8].copy_from_slice(&1u32.to_le_bytes()); // k_EResultOK
    p[8..12].copy_from_slice(&(n as u32).to_le_bytes());
    p[12..12 + n].copy_from_slice(&ticket[..n]);
    p
}

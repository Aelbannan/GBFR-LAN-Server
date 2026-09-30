//! Tests for `steam-http-shim/src/pe.rs` and `src/abi.rs`.
//!
//! Build with plain rustc (the shim's documented build has no cargo/deps):
//!     rustc -O -o pe_test.exe tests/pe_test.rs
//!     .\pe_test.exe
//!
//! The PE tests build a synthetic module image in memory — DOS header, PE32+ optional header,
//! import descriptors, ILT, IAT and hint/name table — and call the real `pe::replace` against it.
//! The image is allocated exactly `SizeOfImage` bytes with an unreadable guard region after it,
//! so a regression that walks past the image end raises an access violation and kills the test
//! (that is the boot crash this code exists to prevent, `0xC0000005` at offset `0x33F2`), while a
//! bounds-checked walk returns `NotFound`.

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicU32, Ordering};

#[path = "../src/abi.rs"]
mod abi;
#[path = "../src/pe.rs"]
mod pe;

static FAILURES: AtomicU32 = AtomicU32::new(0);

fn check(name: &str, ok: bool, detail: &str) {
    if ok {
        println!("PASS  {name}  {detail}");
    } else {
        println!("FAIL  {name}  {detail}");
        FAILURES.fetch_add(1, Ordering::SeqCst);
    }
}

// ── synthetic PE image ───────────────────────────────────────────────────────

const PAGE_READWRITE: u32 = 0x04;
const PAGE_NOACCESS: u32 = 0x01;
const SIZE_OF_IMAGE: usize = 0x3000;

const E_LFANEW: usize = 0x40;
const OPT: usize = E_LFANEW + 24; // optional header start
const OFF_MAGIC: usize = OPT;
const OFF_SIZE_OF_IMAGE: usize = OPT + 56;
const OFF_IMPORT_RVA: usize = OPT + 120;

const IMPORT_RVA: usize = 0x1000;
const DLL_NAME_RVA: usize = 0x1100;
const ILT_RVA: usize = 0x1200;
const HINT1_RVA: usize = 0x1300;
const HINT2_RVA: usize = 0x1340;
const IAT_RVA: usize = 0x1400;

const IAT0_BEFORE: u64 = 0x1122_3344_5566_7788;
const IAT1_BEFORE: u64 = 0xABCD_EF01_2345_6789;

extern "system" {
    fn VirtualAlloc(addr: *mut c_void, size: usize, alloc_type: u32, protect: u32) -> *mut c_void;
    fn VirtualFree(addr: *mut c_void, size: usize, free_type: u32) -> i32;
    fn VirtualProtect(addr: *mut c_void, size: usize, new: u32, old: *mut u32) -> i32;
}

/// A synthetic PE32+ image: `SizeOfImage` readable bytes followed by an unreadable guard page.
struct Image {
    base: *mut u8,
    total: usize,
}

impl Image {
    fn new() -> Self {
        let total = 0x2000; // guard region
        let size = SIZE_OF_IMAGE + total;
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                size,
                0x3000, /* COMMIT|RESERVE */
                PAGE_READWRITE,
            )
        } as *mut u8;
        assert!(!base.is_null(), "VirtualAlloc failed");
        unsafe {
            std::ptr::write_bytes(base, 0, size);
            let mut old = 0u32;
            assert_ne!(
                VirtualProtect(
                    base.add(SIZE_OF_IMAGE) as *mut c_void,
                    total,
                    PAGE_NOACCESS,
                    &mut old
                ),
                0,
                "VirtualProtect guard failed"
            );
        }
        Self { base, total }
    }

    fn u16(&mut self, off: usize, v: u16) {
        unsafe { (self.base.add(off) as *mut u16).write_unaligned(v) };
    }

    fn u32(&mut self, off: usize, v: u32) {
        unsafe { (self.base.add(off) as *mut u32).write_unaligned(v) };
    }

    fn u64(&mut self, off: usize, v: u64) {
        unsafe { (self.base.add(off) as *mut u64).write_unaligned(v) };
    }

    fn bytes(&mut self, off: usize, v: &[u8]) {
        unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), self.base.add(off), v.len()) };
    }

    /// A well-formed image importing `winhttp.dll!WinHttpOpenRequest` and `winhttp.dll!WinHttpSendRequest`.
    fn valid() -> Self {
        let mut i = Self::new();
        i.u32(0x3C, E_LFANEW as u32);
        i.u16(OFF_MAGIC, 0x20b);
        i.u32(OFF_SIZE_OF_IMAGE, SIZE_OF_IMAGE as u32);
        i.u32(OFF_IMPORT_RVA, IMPORT_RVA as u32);

        // descriptor 0: winhttp.dll -> ILT 0x1200, IAT 0x1400
        i.u32(IMPORT_RVA, ILT_RVA as u32); // OriginalFirstThunk
        i.u32(IMPORT_RVA + 12, DLL_NAME_RVA as u32); // Name
        i.u32(IMPORT_RVA + 16, IAT_RVA as u32); // FirstThunk
                                                // descriptor 1 stays zero: the null terminator

        i.bytes(DLL_NAME_RVA, b"winhttp.dll\0");
        i.u64(ILT_RVA, HINT1_RVA as u64);
        i.u64(ILT_RVA + 8, HINT2_RVA as u64);
        // thunk terminator at ILT_RVA + 16 stays zero
        i.u16(HINT1_RVA, 0x1234); // hint
        i.bytes(HINT1_RVA + 2, b"WinHttpOpenRequest\0");
        i.u16(HINT2_RVA, 0x5678);
        i.bytes(HINT2_RVA + 2, b"WinHttpSendRequest\0");
        i.u64(IAT_RVA, IAT0_BEFORE);
        i.u64(IAT_RVA + 8, IAT1_BEFORE);
        i
    }

    fn iat(&self, idx: usize) -> u64 {
        unsafe { (self.base.add(IAT_RVA + idx * 8) as *const u64).read_unaligned() }
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        unsafe {
            // The guard page was never committed, so MEM_RELEASE over the whole reservation is safe.
            VirtualFree(self.base as *mut c_void, 0, 0x8000);
        }
    }
}

fn fake_fn() {}

fn main() {
    // ── happy path ───────────────────────────────────────────────────────────────────────
    {
        let mut img = Image::valid();
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        check(
            "patches the matching import and returns the old pointer",
            r == Ok(IAT0_BEFORE as usize),
            &format!("{r:?}"),
        );
        check(
            "the IAT slot now holds the new pointer",
            img.iat(0) == fake_fn as u64,
            &format!("{:#x}", img.iat(0)),
        );
        check(
            "the other import is untouched",
            img.iat(1) == IAT1_BEFORE,
            "",
        );
    }
    {
        let mut img = Image::valid();
        let r = unsafe {
            pe::replace(
                img.base,
                "WINHTTP.DLL",
                "WinHttpSendRequest",
                fake_fn as usize,
            )
        };
        check(
            "the dll match is case-insensitive; the second thunk patches independently",
            r == Ok(IAT1_BEFORE as usize)
                && img.iat(0) == IAT0_BEFORE
                && img.iat(1) == fake_fn as u64,
            &format!("{r:?}"),
        );
    }
    {
        let img = Image::valid();
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "winhttp_sendrequest",
                fake_fn as usize,
            )
        };
        check(
            "the function name is case-sensitive (exports are)",
            r == Err(pe::IatError::NotFound),
            &format!("{r:?}"),
        );
    }
    {
        let img = Image::valid();
        let r = unsafe {
            pe::replace(
                img.base,
                "kernel32.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        check(
            "unknown dll is NotFound",
            r == Err(pe::IatError::NotFound),
            &format!("{r:?}"),
        );
    }
    {
        let img = Image::valid();
        let r = unsafe { pe::replace(img.base, "winhttp.dll", "NoSuchExport", fake_fn as usize) };
        check(
            "unknown function is NotFound",
            r == Err(pe::IatError::NotFound),
            &format!("{r:?}"),
        );
    }
    {
        let r = unsafe { pe::replace(std::ptr::null_mut(), "winhttp.dll", "x", fake_fn as usize) };
        check(
            "null module is rejected",
            r == Err(pe::IatError::NullModule),
            &format!("{r:?}"),
        );
    }

    // ── malformed headers ────────────────────────────────────────────────────────────────
    {
        let mut img = Image::valid();
        img.u32(0x3C, 0x2000);
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        check(
            "e_lfanew beyond the header window is rejected",
            r == Err(pe::IatError::BadDosHeader),
            &format!("{r:?}"),
        );
    }
    {
        let mut img = Image::valid();
        img.u32(0x3C, 0);
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        check(
            "e_lfanew inside the DOS header is rejected",
            r == Err(pe::IatError::BadDosHeader),
            &format!("{r:?}"),
        );
    }
    {
        let mut img = Image::valid();
        img.u16(OFF_MAGIC, 0x10b); // PE32, not PE32+
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        check(
            "non-PE32+ is rejected",
            r == Err(pe::IatError::NotPe64),
            &format!("{r:?}"),
        );
    }
    {
        let mut img = Image::valid();
        img.u32(OFF_SIZE_OF_IMAGE, 0x100);
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        check(
            "a bogus SizeOfImage is rejected before any walk",
            r == Err(pe::IatError::BadImageSize),
            &format!("{r:?}"),
        );
    }
    {
        let mut img = Image::valid();
        img.u32(OFF_IMPORT_RVA, 0x5000);
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        check(
            "an import directory outside the image is rejected",
            r == Err(pe::IatError::BadImportDirectory),
            &format!("{r:?}"),
        );
    }

    // ── the boot-crash regression cases: out-of-image RVAs must never be dereferenced ─────
    {
        let mut img = Image::valid();
        img.u32(IMPORT_RVA + 12, 0x9_9999); // descriptor 0 name RVA far outside the image
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        img.u32(0, 0); // no-op, keep the image alive past the call
        check(
            "a descriptor whose name RVA is outside the image is skipped, not dereferenced",
            r == Err(pe::IatError::NotFound),
            &format!("{r:?}"),
        );
    }
    {
        let mut img = Image::valid();
        img.u32(IMPORT_RVA, 0x9_9999); // OriginalFirstThunk outside the image
        img.u32(IMPORT_RVA + 16, IAT_RVA as u32); // FirstThunk still valid
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        check(
            "an out-of-image ILT RVA is skipped (the old walk dereferenced it)",
            r == Err(pe::IatError::NotFound),
            &format!("{r:?}"),
        );
    }
    {
        let mut img = Image::valid();
        img.u32(IMPORT_RVA, ILT_RVA as u32);
        img.u32(IMPORT_RVA + 16, (SIZE_OF_IMAGE - 4) as u32); // FirstThunk straddles the image end
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        check(
            "an IAT that straddles the image end is skipped",
            r == Err(pe::IatError::NotFound),
            &format!("{r:?}"),
        );
    }
    {
        let r = unsafe {
            pe::replace(
                std::ptr::null_mut::<u8>(),
                "winhttp.dll",
                "x",
                fake_fn as usize,
            )
        };
        check(
            "null module is NullModule, not a crash",
            r == Err(pe::IatError::NullModule),
            &format!("{r:?}"),
        );
    }

    // ── unterminated strings run into the guard page ─────────────────────────────────────
    {
        // The dll name starts near the image end with no NUL: a scan that is not clamped to
        // SizeOfImage reaches the unreadable page and kills the process.
        let mut img = Image::valid();
        img.u32(IMPORT_RVA + 12, (SIZE_OF_IMAGE - 11) as u32);
        // 11 bytes with no NUL, ending exactly at SizeOfImage: the next readable address is the
        // guard page.
        img.bytes(SIZE_OF_IMAGE - 11, b"winhttp.dlX");
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        check(
            "an unterminated dll name is skipped at the image end",
            r == Err(pe::IatError::NotFound),
            &format!("{r:?}"),
        );
    }
    {
        let mut img = Image::valid();
        img.u64(ILT_RVA, (SIZE_OF_IMAGE - 11) as u64);
        img.bytes(SIZE_OF_IMAGE - 11, b"WinHttpOprX"); // hint + name, no NUL before the guard
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        check(
            "an unterminated function name is skipped at the image end",
            r == Err(pe::IatError::NotFound),
            &format!("{r:?}"),
        );
    }
    {
        // A thunk entry with the high bit set is an ordinal import: it must be skipped without
        // being treated as a name pointer.
        let mut img = Image::valid();
        img.u64(ILT_RVA, 0x8000_0000_0000_0042);
        let r = unsafe {
            pe::replace(
                img.base,
                "winhttp.dll",
                "WinHttpOpenRequest",
                fake_fn as usize,
            )
        };
        check(
            "ordinal imports are skipped",
            r == Err(pe::IatError::NotFound),
            &format!("{r:?}"),
        );
    }

    // ── mov_eax_imm ──────────────────────────────────────────────────────────────────────
    check(
        "mov eax, imm32 at offset 0",
        pe::mov_eax_imm(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]) == Some(1),
        "",
    );
    check(
        "mov eax, imm32 after a prefix",
        pe::mov_eax_imm(&[0x55, 0x8B, 0xEC, 0xB8, 0x02, 0x00, 0x00, 0x00, 0xC3]) == Some(2),
        "",
    );
    check(
        "too short is None",
        pe::mov_eax_imm(&[0xB8, 0x01]).is_none(),
        "",
    );
    check(
        "no mov at all is None",
        pe::mov_eax_imm(&[0x90, 0x90, 0x90, 0x90, 0x90, 0xC3]).is_none(),
        "",
    );
    check("empty is None", pe::mov_eax_imm(&[]).is_none(), "");

    // ── struct packing (offsets from the SDK layout) ─────────────────────────────────────
    {
        let p = abi::pack_http_completed(0x11223344, 0x0102030405060708, true, 200, 4096);
        let rd32 = |o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
        let rd64 = |o: usize| u64::from_le_bytes(p[o..o + 8].try_into().unwrap());
        check(
            "HTTPRequestCompleted_t offsets",
            rd32(0) == 0x11223344
                && rd64(8) == 0x0102030405060708
                && p[16] == 1
                && rd32(20) == 200
                && rd32(24) == 4096,
            &format!("{p:02x?}"),
        );
        check(
            "HTTPRequestCompleted_t padding is zeroed",
            p[4..8].iter().all(|b| *b == 0)
                && p[17..20].iter().all(|b| *b == 0)
                && p[28..32].iter().all(|b| *b == 0),
            "",
        );
        let f = abi::pack_http_completed(1, 0, false, 0, 0);
        check(
            "ok=false packs a zero byte, not 1",
            f[16] == 0 && f.iter().filter(|b| **b != 0).count() == 1,
            &format!("{f:02x?}"),
        );
    }
    {
        let ticket = b"LANSTUB|76561198000000000|PC";
        let p = abi::pack_webapi_ticket(7, ticket, 2560);
        let rd32 = |o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
        check(
            "GetTicketForWebApiResponse_t offsets",
            p.len() == 12 + 2560
                && rd32(0) == 7
                && rd32(4) == 1
                && rd32(8) == ticket.len() as u32
                && &p[12..12 + ticket.len()] == ticket,
            &format!("len={}", p.len()),
        );
        check(
            "the ticket array tail stays zero",
            p[12 + ticket.len()..].iter().all(|b| *b == 0),
            "",
        );
        let long = vec![0x5A; 3000];
        let p = abi::pack_webapi_ticket(1, &long, 2560);
        check(
            "an oversized ticket is truncated to the array length",
            p.len() == 12 + 2560 && p[12..].iter().all(|b| *b == 0x5A),
            "",
        );
        let p = abi::pack_webapi_ticket(1, &[], 2560);
        check(
            "an empty ticket packs a zero count",
            u32::from_le_bytes(p[8..12].try_into().unwrap()) == 0,
            "",
        );
    }

    // ── string bridges ───────────────────────────────────────────────────────────────────
    {
        let w = abi::to_wide("abc");
        check(
            "to_wide appends NUL",
            w == vec![0x61, 0x62, 0x63, 0x00],
            &format!("{w:?}"),
        );
        let s = unsafe { abi::from_wide(w.as_ptr()) };
        check("from_wide round-trips", s == "abc", &s);
        check(
            "from_wide(null) is empty",
            unsafe { abi::from_wide(std::ptr::null()) }.is_empty(),
            "",
        );
        let wide: Vec<u16> = "héllo".encode_utf16().chain(std::iter::once(0)).collect();
        let s = unsafe { abi::from_wide(wide.as_ptr()) };
        check("from_wide keeps non-ASCII", s == "héllo", &s);
        check(
            "to_wide of an empty string is just NUL",
            abi::to_wide("") == vec![0u16],
            "",
        );
    }
    {
        let c = std::ffi::CString::new("hello").unwrap();
        let s = unsafe { abi::from_c(c.as_ptr()) };
        check("from_c round-trips", s == "hello", &s);
        check(
            "from_c(null) is empty",
            unsafe { abi::from_c(std::ptr::null()) }.is_empty(),
            "",
        );
        // Not NUL-terminated within the cap: must truncate, not run away.
        let raw = vec![b'a'; 6000];
        let s = unsafe { abi::from_c(raw.as_ptr() as *const c_char) };
        check(
            "from_c caps an unterminated string at 4096",
            s.len() == 4096,
            &format!("len={}", s.len()),
        );
        let bad = [0xFFu8, 0xFE, 0x00];
        let s = unsafe { abi::from_c(bad.as_ptr() as *const c_char) };
        check(
            "from_c lossily decodes invalid UTF-8 without panicking",
            !s.is_empty(),
            &s,
        );
    }

    let failures = FAILURES.load(Ordering::SeqCst);
    if failures > 0 {
        println!("{failures} FAILURE(S)");
        std::process::exit(1);
    }
    println!("all steam-http pe/abi checks passed");
}

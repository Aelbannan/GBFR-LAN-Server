//! Bounds-checked PE import-address-table patching for `gbfr_http.dll`.
//!
//! Split out of `lib.rs` so it can be exercised against a synthetic module image (see
//! `tests/pe_test.rs`). The unguarded predecessor of this walk crashed the game at boot
//! (`0xC0000005` at offset `0x33F2`, the `*iat = new` store) when another boot-time patcher was
//! mutating the import tables concurrently; the real fix is that *every* read and write here is
//! checked against the module's `SizeOfImage` and every string scan is bounded by it, so a
//! malformed, truncated or concurrently-edited table can only produce `Err`, never a bogus
//! pointer dereference.
//!
//! This module contains no logging and no game state: it returns why it failed so the embedder
//! can decide what to log.

use std::ffi::c_void;

/// `PAGE_EXECUTE_READWRITE`, the protection used for the one-slot write. Import tables sit in a
/// read-only page after the loader maps them.
pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;

/// Why `replace` did not patch a slot. `NotFound` is the normal outcome for an import the module
/// does not have (or a DLL that is not imported at all).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IatError {
    /// `module` was null.
    NullModule,
    /// `IMAGE_DOS_HEADER.e_lfanew` is outside the file-header window.
    BadDosHeader,
    /// The optional header is not PE32+ (mask `0x20b`).
    NotPe64,
    /// `SizeOfImage` is missing or unusable, so no bounds check would be meaningful.
    BadImageSize,
    /// The import directory RVA is outside the image.
    BadImportDirectory,
    /// The slot exists but `VirtualProtect` refused to make it writable.
    SlotProtected,
    /// No matching `dll!func` in the import tables.
    NotFound,
}

#[link(name = "kernel32")]
extern "system" {
    // Distinct Rust name: `lib.rs` links the same import under its own binding, and two bindings
    // for the same symbol are fine as long as the Rust identifiers differ.
    #[link_name = "VirtualProtect"]
    fn pe_virtual_protect(addr: *mut c_void, size: usize, new: u32, old: *mut u32) -> i32;
}

/// Replace the `dll!func` entry of `module`'s import address table with `new`, returning the
/// previous value. `new` must be an `extern "system"` function whose ABI matches the import.
///
/// # Safety
/// `module` must point at the base of a mapped PE image that stays mapped. `new` must be a valid
/// function pointer with the ABI of the original import.
pub unsafe fn replace(
    module: *mut u8,
    dll: &str,
    func: &str,
    new: usize,
) -> Result<usize, IatError> {
    if module.is_null() {
        return Err(IatError::NullModule);
    }
    let e_lfanew = *(module.add(0x3C) as *const u32) as usize;
    // The smallest valid PE places the file header right after IMAGE_DOS_HEADER (0x40).
    if !(0x40..=0x1000).contains(&e_lfanew) {
        return Err(IatError::BadDosHeader);
    }
    let opt = module.add(e_lfanew + 24);
    if *(opt as *const u16) != 0x20b {
        return Err(IatError::NotPe64);
    }
    let size_of_image = *(opt.add(56) as *const u32) as usize;
    if size_of_image < 0x1000 {
        return Err(IatError::BadImageSize);
    }
    let in_image = |rva: usize, len: usize| -> bool {
        rva.checked_add(len).is_some_and(|end| end <= size_of_image)
    };
    /// Read a NUL-terminated ASCII string inside the image. The scan is clamped to the image end
    /// so an unterminated name can never walk past the mapped module.
    unsafe fn image_cstr(
        module: *mut u8,
        rva: usize,
        size_of_image: usize,
        cap: usize,
    ) -> Option<String> {
        let mut n = 0usize;
        while n < cap {
            if rva.checked_add(n).is_none_or(|off| off >= size_of_image) {
                return None;
            }
            if *(module.add(rva + n)) == 0 {
                return Some(
                    String::from_utf8_lossy(std::slice::from_raw_parts(module.add(rva), n))
                        .into_owned(),
                );
            }
            n += 1;
        }
        None
    }

    let import_rva = *(opt.add(120) as *const u32) as usize;
    if !in_image(import_rva, 20) {
        return Err(IatError::BadImportDirectory);
    }
    let mut desc = module.add(import_rva);
    for desc_idx in 0..256 {
        if !in_image(import_rva + desc_idx * 20, 20) {
            break;
        }
        let orig_thunk = *(desc as *const u32) as usize;
        let name_rva = *(desc.add(12) as *const u32) as usize;
        let first_thunk = *(desc.add(16) as *const u32) as usize;
        if name_rva == 0 && first_thunk == 0 {
            break; // null terminator descriptor: end of the import directory
        }
        if name_rva == 0 || first_thunk == 0 || !in_image(name_rva, 2) {
            desc = desc.add(20);
            continue;
        }
        let Some(iname) = image_cstr(module, name_rva, size_of_image, 128) else {
            desc = desc.add(20);
            continue;
        };
        if !iname.eq_ignore_ascii_case(dll) {
            desc = desc.add(20);
            continue;
        }
        let ilt_rva = if orig_thunk != 0 {
            orig_thunk
        } else {
            first_thunk
        };
        if !in_image(ilt_rva, 8) || !in_image(first_thunk, 8) {
            desc = desc.add(20);
            continue;
        }
        let mut ilt = module.add(ilt_rva) as *mut u64;
        let mut iat = module.add(first_thunk) as *mut u64;
        for thunk_idx in 0..4096 {
            if !in_image(ilt_rva + thunk_idx * 8, 8) || !in_image(first_thunk + thunk_idx * 8, 8) {
                break;
            }
            let entry = *ilt;
            if entry == 0 {
                break;
            }
            if entry & (1u64 << 63) == 0 {
                if let Some(fname) = image_cstr(module, entry as usize + 2, size_of_image, 128) {
                    if fname == func {
                        let old = *iat as usize;
                        let mut prot = 0u32;
                        if pe_virtual_protect(
                            iat as *mut c_void,
                            8,
                            PAGE_EXECUTE_READWRITE,
                            &mut prot,
                        ) == 0
                        {
                            return Err(IatError::SlotProtected);
                        }
                        *iat = new as u64;
                        let mut tmp = 0u32;
                        pe_virtual_protect(iat as *mut c_void, 8, prot, &mut tmp);
                        return Ok(old);
                    }
                }
            }
            ilt = ilt.add(1);
            iat = iat.add(1);
        }
        desc = desc.add(20);
    }
    Err(IatError::NotFound)
}

/// Scan a small code stub for the first `mov eax, imm32` (`B8 xx xx xx xx`). Several Steam API
/// init functions are `mov eax, 1; ret` ("already initialised") stubs; the scanner must find the
/// constant without assuming it is the first instruction.
pub fn mov_eax_imm(code: &[u8]) -> Option<u32> {
    if code.len() >= 5 && code[0] == 0xB8 {
        return Some(u32::from_le_bytes(code[1..5].try_into().ok()?));
    }
    for i in 0..code.len().saturating_sub(5) {
        if code[i] == 0xB8 {
            return Some(u32::from_le_bytes(code[i + 1..i + 5].try_into().ok()?));
        }
    }
    None
}

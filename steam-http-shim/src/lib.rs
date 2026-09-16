//! ISteamHTTP / WinHTTP shim: hooks the Steam HTTP interface after SteamAPI_Init,
//! redirects requests to the LAN server, and completes Steam call results.
//! Loaded through Goldberg's steam_settings\load_dlls.

#![allow(non_snake_case)]

use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

include!("../../common/lan_cfg.rs");

const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const DLL_PROCESS_ATTACH: u32 = 1;
const WINHTTP_ACCESS_TYPE_DEFAULT_PROXY: u32 = 0;
const WINHTTP_FLAG_SECURE: u32 = 0x00800000;
const WINHTTP_ADDREQ_FLAG_ADD: u32 = 0x20000000;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x20000000;
const INTERNET_SCHEME_HTTPS: i32 = 2;
const WINHTTP_OPTION_SECURITY_FLAGS: u32 = 31;
const SECURITY_FLAG_IGNORE_UNKNOWN_CA: u32 = 0x00000100;
const SECURITY_FLAG_IGNORE_CERT_WRONG_USAGE: u32 = 0x00000200;
const SECURITY_FLAG_IGNORE_CERT_CN_INVALID: u32 = 0x00001000;
const SECURITY_FLAG_IGNORE_CERT_DATE_INVALID: u32 = 0x00002000;
const HTTP_CALLBACK_COMPLETED: i32 = 2101;
const UTILS_IS_CALL_COMPLETED: usize = 11;
const UTILS_GET_CALL_RESULT: usize = 13;
const USER_BLOGGED_ON: usize = 1;
const USER_GET_AUTH_TICKET_WEBAPI: usize = 14; // SteamUser023, after GetAuthSessionTicket
const CB_STEAM_SERVERS_CONNECTED: i32 = 101; // k_iSteamUserCallbacks + 1
const CB_TICKET_FOR_WEBAPI: i32 = 168; // k_iSteamUserCallbacks + 68
const WEBAPI_TICKET_MAX: usize = 2560;

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleA(name: *const u8) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn VirtualProtect(addr: *mut c_void, size: usize, new: u32, old: *mut u32) -> i32;
    fn VirtualAlloc(addr: *mut c_void, size: usize, ty: u32, prot: u32) -> *mut c_void;
    fn GetModuleFileNameW(module: *mut c_void, buf: *mut u16, size: u32) -> u32;
    fn GetSystemDirectoryW(buf: *mut u16, len: u32) -> u32;
    fn OutputDebugStringA(s: *const u8);
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
}

#[link(name = "winhttp")]
extern "system" {
    fn WinHttpOpen(
        agent: *const u16,
        access: u32,
        proxy: *const u16,
        bypass: *const u16,
        flags: u32,
    ) -> *mut c_void;
    fn WinHttpConnect(session: *mut c_void, host: *const u16, port: u16, reserved: u32) -> *mut c_void;
    fn WinHttpOpenRequest(
        connect: *mut c_void,
        verb: *const u16,
        path: *const u16,
        version: *const u16,
        referrer: *const u16,
        accept: *const *const u16,
        flags: u32,
    ) -> *mut c_void;
    fn WinHttpAddRequestHeaders(
        request: *mut c_void,
        headers: *const u16,
        len: u32,
        modifiers: u32,
    ) -> i32;
    fn WinHttpSendRequest(
        request: *mut c_void,
        headers: *const u16,
        hlen: u32,
        optional: *const u8,
        olen: u32,
        total: u32,
        context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(request: *mut c_void, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        request: *mut c_void,
        info: u32,
        name: *const u16,
        buffer: *mut c_void,
        size: *mut u32,
        index: *mut u32,
    ) -> i32;
    fn WinHttpReadData(request: *mut c_void, buf: *mut u8, size: u32, read: *mut u32) -> i32;
    fn WinHttpSetOption(h: *mut c_void, option: u32, buf: *mut c_void, len: u32) -> i32;
    fn WinHttpCloseHandle(h: *mut c_void) -> i32;
    fn WinHttpCrackUrl(url: *const u16, len: u32, flags: u32, comp: *mut WinHttpUrlComponents) -> i32;
}

#[repr(C)]
struct WinHttpUrlComponents {
    struct_size: u32,
    scheme: *mut u16,
    scheme_len: u32,
    nscheme: i32,
    host: *mut u16,
    host_len: u32,
    port: u16,
    user: *mut u16,
    user_len: u32,
    pass: *mut u16,
    pass_len: u32,
    path: *mut u16,
    path_len: u32,
    extra: *mut u16,
    extra_len: u32,
}

struct Request {
    method: i32,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    context: u64,
    status: u32,
    response: Vec<u8>,
    ok: bool,
}

struct Call {
    request: u32,
    failed: bool,
}

struct State {
    requests: HashMap<u32, Request>,
    calls: HashMap<u64, Call>,
    call_results: HashMap<u64, usize>,
    dispatched: HashSet<u64>,
    callbacks: Vec<(i32, usize)>,
    pending_webapi: Vec<u32>,
    fired_connected: HashSet<usize>,
}

static PATCHED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);
static NEXT_CALL: AtomicU64 = AtomicU64::new(0x0BEE_0001);
static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static ORIG_UTILS: OnceLock<UtilsOrig> = OnceLock::new();
static ORIG_RUN_CALLBACKS: OnceLock<unsafe extern "system" fn()> = OnceLock::new();
static ORIG_REG_CALL: OnceLock<unsafe extern "system" fn(*mut c_void, u64)> = OnceLock::new();
static ORIG_UNREG_CALL: OnceLock<unsafe extern "system" fn(*mut c_void, u64)> = OnceLock::new();
static ORIG_REG_CB: OnceLock<unsafe extern "system" fn(*mut c_void, i32)> = OnceLock::new();
static ORIG_UNREG_CB: OnceLock<unsafe extern "system" fn(*mut c_void)> = OnceLock::new();
static ORIG_WEBAPI: OnceLock<unsafe extern "system" fn(*mut c_void, *const c_char) -> u32> = OnceLock::new();
static ORIG_SVC_CFG: OnceLock<
    unsafe extern "C" fn(*const c_char, *const c_char, *mut *mut c_void) -> i32,
> = OnceLock::new();
type WinHttpConnectFn =
    unsafe extern "system" fn(*mut c_void, *const u16, u16, u32) -> *mut c_void;
type WinHttpSendFn = unsafe extern "system" fn(
    *mut c_void,
    *const u16,
    u32,
    *const u8,
    u32,
    u32,
    usize,
) -> i32;
static ORIG_WH_CONNECT: OnceLock<WinHttpConnectFn> = OnceLock::new();
static ORIG_WH_SEND: OnceLock<WinHttpSendFn> = OnceLock::new();
type WinHttpOpenRequestFn = unsafe extern "system" fn(
    *mut c_void,
    *const u16,
    *const u16,
    *const u16,
    *const u16,
    *const *const u16,
    u32,
) -> *mut c_void;
static ORIG_WH_OPEN: OnceLock<WinHttpOpenRequestFn> = OnceLock::new();
static NEXT_TICKET: AtomicU32 = AtomicU32::new(1);
static IAT_HOOKED: AtomicBool = AtomicBool::new(false);

struct UtilsOrig {
    is_completed: unsafe extern "system" fn(*mut c_void, u64, *mut u8) -> u8,
    get_result: unsafe extern "system" fn(*mut c_void, u64, *mut u8, i32, i32, *mut u8) -> u8,
}

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| {
        Mutex::new(State {
            requests: HashMap::new(),
            calls: HashMap::new(),
            call_results: HashMap::new(),
            dispatched: HashSet::new(),
            callbacks: Vec::new(),
            pending_webapi: Vec::new(),
            fired_connected: HashSet::new(),
        })
    })
}

fn log_file() -> std::path::PathBuf {
    unsafe {
        let mut buf = [0u16; 260];
        let steam = GetModuleHandleA(b"steam_api64.dll\0".as_ptr());
        let n = if steam.is_null() {
            GetModuleFileNameW(ptr::null_mut(), buf.as_mut_ptr(), 260)
        } else {
            GetModuleFileNameW(steam, buf.as_mut_ptr(), 260)
        } as usize;
        let path = String::from_utf16_lossy(&buf[..n.min(260)]);
        std::path::Path::new(&path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("steam_http_shim.log")
    }
}

static LOG_TRUNCATED: AtomicBool = AtomicBool::new(false);

fn log_msg(msg: &str) {
    let line = format!("[gbfr-http] {}\n", msg);
    let c = line.replace('\0', "");
    unsafe { OutputDebugStringA(c.as_ptr()) };
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true);
    if !LOG_TRUNCATED.swap(true, Ordering::SeqCst) {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    if let Ok(mut f) = opts.open(log_file()) {
        use std::io::Write;
        let _ = write!(f, "{}", line);
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_c(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe {
        let mut n = 0usize;
        while n < 4096 && *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, n)).into_owned()
    }
}

fn pack_http_completed(handle: u32, context: u64, ok: bool, status: u32, body_len: u32) -> [u8; 32] {
    // MSVC x64 HTTPRequestCompleted_t (uint32 + pad + uint64 + bool + pad + enum + uint32)
    let mut p = [0u8; 32];
    p[0..4].copy_from_slice(&handle.to_le_bytes());
    p[8..16].copy_from_slice(&context.to_le_bytes());
    p[16] = if ok { 1 } else { 0 };
    p[20..24].copy_from_slice(&status.to_le_bytes());
    p[24..28].copy_from_slice(&body_len.to_le_bytes());
    p
}

fn exe_dir() -> std::path::PathBuf {
    let mut buf = [0u16; 260];
    let n = unsafe { GetModuleFileNameW(ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) };
    let s = String::from_utf16_lossy(&buf[..n as usize]);
    std::path::Path::new(&s)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

fn read_account_steamid() -> String {
    let p = exe_dir().join("steam_settings").join("configs.user.ini");
    let Ok(text) = std::fs::read_to_string(p) else {
        return "0".into();
    };
    for line in text.lines() {
        let line = line.split([';', '#']).next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case("account_steamid") {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    "0".into()
}

fn lan_steam_ticket() -> Vec<u8> {
    // Must be unique per client. FindLobbies hides rooms the viewer already
    // belongs to; a shared ticket made every PC the same PlayFab entity.
    let steam = read_account_steamid();
    let pc = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "pc".into());
    format!("LANSTUB|{steam}|{pc}").into_bytes()
}

fn pack_webapi_ticket(handle: u32) -> Vec<u8> {
    // GetTicketForWebApiResponse_t: HAuthTicket, EResult, cubTicket, rgubTicket[2560]
    let ticket = lan_steam_ticket();
    let n = ticket.len().min(WEBAPI_TICKET_MAX);
    let mut p = vec![0u8; 12 + WEBAPI_TICKET_MAX];
    p[0..4].copy_from_slice(&handle.to_le_bytes());
    p[4..8].copy_from_slice(&1u32.to_le_bytes()); // k_EResultOK
    p[8..12].copy_from_slice(&(n as u32).to_le_bytes());
    p[12..12 + n].copy_from_slice(&ticket[..n]);
    p
}

fn mov_eax_imm(code: &[u8]) -> Option<u32> {
    // mov eax, imm32  (B8 xx xx xx xx) optionally followed by ret (C3)
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

/// # Safety
/// `obj` must be a live Steam callback object: its first field is a vtable pointer and slots
/// 0..6 are readable function pointers. Objects come from the emulator's RegisterCallback and
/// are never owned or freed here.
unsafe fn callback_run_index(obj: usize, default: usize) -> usize {
    let vptr = *(obj as *mut *mut c_void);
    if vptr.is_null() {
        return default;
    }
    let slots = vptr as *mut *mut c_void;
    for i in 0..6 {
        let f = *slots.add(i);
        if f.is_null() {
            continue;
        }
        let code = std::slice::from_raw_parts(f as *const u8, 16);
        if let Some(sz) = mov_eax_imm(code) {
            if (1..8192).contains(&sz) && i >= 2 {
                log_msg(&format!(
                    "callback {:#x} GetCallbackSizeBytes slot={} size={} -> Run slot={}",
                    obj,
                    i,
                    sz,
                    i - 2
                ));
                return i - 2;
            }
        }
    }
    default
}

/// # Safety
/// Same contract as `callback_run_index`, plus: the selected vtable slot is a valid
/// `void (*)(void*, void*)` and `data` points to a buffer with the callback's documented
/// layout. Callers pass Steam-owned callback structs and freshly packed payloads.
unsafe fn invoke_callback(obj: usize, data: *mut u8, default_run: usize) {
    if obj == 0 {
        return;
    }
    let vptr = *(obj as *mut *mut c_void);
    if vptr.is_null() {
        return;
    }
    let slots = vptr as *mut *mut c_void;
    let idx = callback_run_index(obj, default_run);
    let run: unsafe extern "system" fn(*mut c_void, *mut u8) =
        std::mem::transmute(*slots.add(idx));
    run(obj as *mut c_void, data);
}

unsafe extern "system" fn blogged_on(_this: *mut c_void) -> u8 {
    1
}

unsafe extern "system" fn get_auth_ticket_webapi(this: *mut c_void, identity: *const c_char) -> u32 {
    let id = from_c(identity);
    // Cygames sys/user_auth uses identity server_auth_key_001 (pointer table
    // 0x145F3AC10). PlayFab login uses playfab_services. Goldberg may return a
    // handle for one and not complete the other — always stub both.
    let lan_identity = id == "playfab_services" || id == "server_auth_key_001";
    if !lan_identity {
        if let Some(orig) = ORIG_WEBAPI.get() {
            let h = orig(this, identity);
            log_msg(&format!("GetAuthTicketForWebApi '{}' orig={}", id, h));
            if h != 0 {
                return h;
            }
        }
    }
    let handle = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
    let ticket = String::from_utf8_lossy(&lan_steam_ticket()).into_owned();
    log_msg(&format!(
        "GetAuthTicketForWebApi '{}' stub={} ticket={}",
        id, handle, ticket
    ));
    state().lock().unwrap().pending_webapi.push(handle);
    handle
}

unsafe extern "system" fn hook_reg_cb(cb: *mut c_void, i_callback: i32) {
    if !cb.is_null() {
        state()
            .lock()
            .unwrap()
            .callbacks
            .push((i_callback, cb as usize));
        log_msg(&format!("RegisterCallback id={} cb={:p}", i_callback, cb));
    }
    if let Some(orig) = ORIG_REG_CB.get() {
        orig(cb, i_callback);
    }
}

unsafe extern "system" fn hook_unreg_cb(cb: *mut c_void) {
    if !cb.is_null() {
        let mut st = state().lock().unwrap();
        st.callbacks.retain(|(_, p)| *p != cb as usize);
        st.fired_connected.remove(&(cb as usize));
    }
    if let Some(orig) = ORIG_UNREG_CB.get() {
        orig(cb);
    }
}

fn dispatch_steam_user_callbacks() {
    let (connected, tickets, cbs) = {
        let mut st = state().lock().unwrap();
        let tickets = std::mem::take(&mut st.pending_webapi);
        let cbs = st.callbacks.clone();
        let mut connected = Vec::new();
        for (id, ptr) in &cbs {
            if *id == CB_STEAM_SERVERS_CONNECTED && st.fired_connected.insert(*ptr) {
                connected.push(*ptr);
            }
        }
        (connected, tickets, cbs)
    };
    for ptr in connected {
        let mut dummy = [0u8; 8];
        unsafe { invoke_callback(ptr, dummy.as_mut_ptr(), 0) };
        log_msg(&format!("dispatched SteamServersConnected_t cb={:#x}", ptr));
    }
    for handle in tickets {
        let mut packed = pack_webapi_ticket(handle);
        let mut n = 0;
        for (id, ptr) in &cbs {
            if *id == CB_TICKET_FOR_WEBAPI {
                // PlayFabCore's CCallback often has a virtual dtor at slot 0.
                unsafe { invoke_callback(*ptr, packed.as_mut_ptr(), 1) };
                n += 1;
            }
        }
        log_msg(&format!(
            "dispatched GetTicketForWebApiResponse_t handle={} listeners={}",
            handle, n
        ));
    }
}

/// # Safety
/// `vtable` points at a live vtable whose `index`-th slot exists, and `new_fn` has the same ABI
/// as the function stored there. VirtualProtect is required because vtables sit in read-only
/// pages after the loader maps them; the original protection is restored before returning.
unsafe fn patch_slot(vtable: *mut *mut c_void, index: usize, new_fn: *mut c_void) {
    let slot = vtable.add(index);
    let mut old = 0u32;
    VirtualProtect(slot as *mut c_void, std::mem::size_of::<*mut c_void>(), PAGE_EXECUTE_READWRITE, &mut old);
    *slot = new_fn;
    VirtualProtect(slot as *mut c_void, std::mem::size_of::<*mut c_void>(), old, &mut old);
}

fn winhttp_execute(req: &mut Request) {
    // SAFETY: every handle below is created by the matching WinHttpOpen* call and closed on all
    // exit paths; pointers passed to WinHTTP come from NUL-terminated `to_wide` buffers that
    // outlive each call. No game memory is touched in this function.
    unsafe {
        let agent = to_wide("GBFR-lan-stub");
        let session = WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, ptr::null(), ptr::null(), 0);
        if session.is_null() {
            log_msg("WinHttpOpen failed");
            req.ok = false;
            req.status = 0;
            return;
        }
        let url_w = to_wide(&req.url);
        let mut host = [0u16; 256];
        let mut path = [0u16; 2048];
        let mut extra = [0u16; 1024];
        let mut comp = WinHttpUrlComponents {
            struct_size: std::mem::size_of::<WinHttpUrlComponents>() as u32,
            scheme: ptr::null_mut(),
            scheme_len: 0,
            nscheme: 0,
            host: host.as_mut_ptr(),
            host_len: host.len() as u32,
            port: 0,
            user: ptr::null_mut(),
            user_len: 0,
            pass: ptr::null_mut(),
            pass_len: 0,
            path: path.as_mut_ptr(),
            path_len: path.len() as u32,
            extra: extra.as_mut_ptr(),
            extra_len: extra.len() as u32,
        };
        if WinHttpCrackUrl(url_w.as_ptr(), 0, 0, &mut comp) == 0 {
            log_msg(&format!("WinHttpCrackUrl failed {}", req.url));
            WinHttpCloseHandle(session);
            req.ok = false;
            return;
        }
        let connect = WinHttpConnect(session, host.as_ptr(), comp.port, 0);
        if connect.is_null() {
            log_msg("WinHttpConnect failed");
            WinHttpCloseHandle(session);
            req.ok = false;
            return;
        }
        let verb = to_wide(if req.method == 3 { "POST" } else { "GET" });
        let mut full_path: Vec<u16> = path.iter().cloned().take_while(|&c| c != 0).collect();
        let extra_s: Vec<u16> = extra.iter().cloned().take_while(|&c| c != 0).collect();
        full_path.extend_from_slice(&extra_s);
        full_path.push(0);
        let flags = if comp.nscheme == INTERNET_SCHEME_HTTPS {
            WINHTTP_FLAG_SECURE
        } else {
            0
        };
        let hreq = WinHttpOpenRequest(
            connect,
            verb.as_ptr(),
            full_path.as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            flags,
        );
        if hreq.is_null() {
            log_msg("WinHttpOpenRequest failed");
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            req.ok = false;
            return;
        }
        if flags != 0 {
            let mut sec = SECURITY_FLAG_IGNORE_UNKNOWN_CA
                | SECURITY_FLAG_IGNORE_CERT_WRONG_USAGE
                | SECURITY_FLAG_IGNORE_CERT_CN_INVALID
                | SECURITY_FLAG_IGNORE_CERT_DATE_INVALID;
            WinHttpSetOption(
                hreq,
                WINHTTP_OPTION_SECURITY_FLAGS,
                &mut sec as *mut u32 as *mut c_void,
                4,
            );
        }
        for (k, v) in &req.headers {
            let line = to_wide(&format!("{}: {}", k, v));
            WinHttpAddRequestHeaders(
                hreq,
                line.as_ptr(),
                (line.len() - 1) as u32,
                WINHTTP_ADDREQ_FLAG_ADD,
            );
        }
        let send = WinHttpSendRequest(
            hreq,
            ptr::null(),
            0,
            if req.body.is_empty() {
                ptr::null()
            } else {
                req.body.as_ptr()
            },
            req.body.len() as u32,
            req.body.len() as u32,
            0,
        );
        if send == 0 || WinHttpReceiveResponse(hreq, ptr::null_mut()) == 0 {
            log_msg(&format!("WinHTTP send/recv failed {}", req.url));
            WinHttpCloseHandle(hreq);
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            req.ok = false;
            return;
        }
        let mut status = 0u32;
        let mut slen = 4u32;
        let mut idx = 0u32;
        WinHttpQueryHeaders(
            hreq,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            ptr::null(),
            &mut status as *mut u32 as *mut c_void,
            &mut slen,
            &mut idx,
        );
        req.status = status;
        let mut body = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            let mut n = 0u32;
            if WinHttpReadData(hreq, chunk.as_mut_ptr(), chunk.len() as u32, &mut n) == 0 {
                break;
            }
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n as usize]);
        }
        req.response = body;
        req.ok = (200..300).contains(&status);
        log_msg(&format!(
            "{} {} -> {} ({} bytes)",
            if req.method == 3 { "POST" } else { "GET" },
            req.url,
            status,
            req.response.len()
        ));
        WinHttpCloseHandle(hreq);
        WinHttpCloseHandle(connect);
        WinHttpCloseHandle(session);
    }
}

unsafe extern "system" fn create_http(_this: *mut c_void, method: i32, url: *const c_char) -> u32 {
    let url = rewrite_http_url_to_stub(&from_c(url));
    if url.is_empty() {
        return 0;
    }
    let id = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let mut st = state().lock().unwrap();
    st.requests.insert(
        id,
        Request {
            method,
            url: url.clone(),
            headers: Vec::new(),
            body: Vec::new(),
            context: 0,
            status: 0,
            response: Vec::new(),
            ok: false,
        },
    );
    log_msg(&format!("CreateHTTPRequest {} {}", id, url));
    id
}

unsafe extern "system" fn set_context(_this: *mut c_void, handle: u32, value: u64) -> u8 {
    if let Some(r) = state().lock().unwrap().requests.get_mut(&handle) {
        r.context = value;
        1
    } else {
        0
    }
}

unsafe extern "system" fn set_timeout(_this: *mut c_void, _handle: u32, _seconds: u32) -> u8 {
    1
}

unsafe extern "system" fn set_header(_this: *mut c_void, handle: u32, name: *const c_char, value: *const c_char) -> u8 {
    let mut st = state().lock().unwrap();
    if let Some(r) = st.requests.get_mut(&handle) {
        r.headers.push((from_c(name), from_c(value)));
        1
    } else {
        0
    }
}

unsafe extern "system" fn set_param(_this: *mut c_void, _h: u32, _n: *const c_char, _v: *const c_char) -> u8 {
    1
}

unsafe extern "system" fn send_http(_this: *mut c_void, handle: u32, call_out: *mut u64) -> u8 {
    let mut req = {
        let st = state().lock().unwrap();
        match st.requests.get(&handle) {
            Some(r) => Request {
                method: r.method,
                url: r.url.clone(),
                headers: r.headers.clone(),
                body: r.body.clone(),
                context: r.context,
                status: 0,
                response: Vec::new(),
                ok: false,
            },
            None => return 0,
        }
    };
    winhttp_execute(&mut req);
    let call = NEXT_CALL.fetch_add(1, Ordering::Relaxed);
    {
        let mut st = state().lock().unwrap();
        if let Some(stored) = st.requests.get_mut(&handle) {
            stored.status = req.status;
            stored.response = req.response;
            stored.ok = req.ok;
        }
        st.calls.insert(
            call,
            Call {
                request: handle,
                failed: !req.ok && req.status == 0,
            },
        );
    }
    if !call_out.is_null() {
        *call_out = call;
    }
    1
}

unsafe extern "system" fn send_stream(this: *mut c_void, handle: u32, call_out: *mut u64) -> u8 {
    send_http(this, handle, call_out)
}

unsafe extern "system" fn bool_true(_this: *mut c_void, _handle: u32) -> u8 {
    1
}

unsafe extern "system" fn create_cookie_container(_this: *mut c_void, _allow: u8) -> u32 {
    1
}

unsafe extern "system" fn release_cookie_container(_this: *mut c_void, _h: u32) -> u8 {
    1
}

unsafe extern "system" fn header_size(_this: *mut c_void, _h: u32, name: *const c_char, size: *mut u32) -> u8 {
    let n = from_c(name).to_ascii_lowercase();
    if n == "content-type" {
        if !size.is_null() {
            *size = 17; // "application/json" + NUL
        }
        1
    } else {
        0
    }
}

unsafe extern "system" fn header_value(
    _this: *mut c_void,
    _h: u32,
    name: *const c_char,
    buf: *mut u8,
    sz: u32,
) -> u8 {
    let n = from_c(name).to_ascii_lowercase();
    if n != "content-type" || buf.is_null() {
        return 0;
    }
    let v = b"application/json\0";
    if sz as usize != v.len() {
        return 0;
    }
    ptr::copy_nonoverlapping(v.as_ptr(), buf, v.len());
    1
}

unsafe extern "system" fn body_size(_this: *mut c_void, handle: u32, size: *mut u32) -> u8 {
    let st = state().lock().unwrap();
    if let Some(r) = st.requests.get(&handle) {
        if !size.is_null() {
            *size = r.response.len() as u32;
        }
        log_msg(&format!("GetHTTPResponseBodySize {} {}", handle, r.response.len()));
        1
    } else {
        0
    }
}

unsafe extern "system" fn body_data(_this: *mut c_void, handle: u32, buf: *mut u8, size: u32) -> u8 {
    let st = state().lock().unwrap();
    if let Some(r) = st.requests.get(&handle) {
        if size as usize != r.response.len() || buf.is_null() {
            return 0;
        }
        ptr::copy_nonoverlapping(r.response.as_ptr(), buf, r.response.len());
        log_msg(&format!("GetHTTPResponseBodyData {} {} bytes", handle, r.response.len()));
        1
    } else {
        0
    }
}

unsafe extern "system" fn stream_body(
    _this: *mut c_void,
    _h: u32,
    _off: u32,
    _buf: *mut u8,
    _sz: u32,
) -> u8 {
    0
}

unsafe extern "system" fn release(_this: *mut c_void, handle: u32) -> u8 {
    state().lock().unwrap().requests.remove(&handle);
    1
}

unsafe extern "system" fn progress(_this: *mut c_void, _h: u32, pct: *mut f32) -> u8 {
    if !pct.is_null() {
        *pct = 100.0;
    }
    1
}

unsafe extern "system" fn set_raw_body(
    _this: *mut c_void,
    handle: u32,
    data: *const u8,
    _content_type: *const c_char,
    len: u32,
) -> u8 {
    let mut st = state().lock().unwrap();
    if let Some(r) = st.requests.get_mut(&handle) {
        if !data.is_null() && len > 0 && len <= 2 * 1024 * 1024 {
            r.body = std::slice::from_raw_parts(data, len as usize).to_vec();
        }
        1
    } else {
        0
    }
}

unsafe extern "system" fn set_cookie(_this: *mut c_void, _c: u32, _h: *const c_char, _n: *const c_char, _v: *const c_char) -> u8 {
    1
}

unsafe extern "system" fn set_cookie_container(_this: *mut c_void, _h: u32, _c: u32) -> u8 {
    1
}

unsafe extern "system" fn set_ua(_this: *mut c_void, _h: u32, _ua: *const c_char) -> u8 {
    1
}

unsafe extern "system" fn set_verify(_this: *mut c_void, _h: u32, _req: u8) -> u8 {
    1
}

unsafe extern "system" fn set_abs_timeout(_this: *mut c_void, _h: u32, _ms: u32) -> u8 {
    1
}

unsafe extern "system" fn was_timeout(_this: *mut c_void, _h: u32, out: *mut u8) -> u8 {
    if !out.is_null() {
        *out = 0;
    }
    1
}

unsafe extern "system" fn utils_is_completed(this: *mut c_void, call: u64, failed: *mut u8) -> u8 {
    {
        let st = state().lock().unwrap();
        if let Some(c) = st.calls.get(&call) {
            if !failed.is_null() {
                *failed = if c.failed { 1 } else { 0 };
            }
            return 1;
        }
    }
    if let Some(orig) = ORIG_UTILS.get() {
        return (orig.is_completed)(this, call, failed);
    }
    0
}

unsafe extern "system" fn utils_get_result(
    this: *mut c_void,
    call: u64,
    callback: *mut u8,
    cub: i32,
    expected: i32,
    failed: *mut u8,
) -> u8 {
    let (handle, call_failed, context, status, body_len, ok) = {
        let st = state().lock().unwrap();
        match st.calls.get(&call) {
            Some(c) => {
                let r = st.requests.get(&c.request);
                (
                    c.request,
                    c.failed,
                    r.map(|x| x.context).unwrap_or(0),
                    r.map(|x| x.status).unwrap_or(0),
                    r.map(|x| x.response.len() as u32).unwrap_or(0),
                    r.map(|x| x.ok).unwrap_or(false),
                )
            }
            None => {
                drop(st);
                if let Some(orig) = ORIG_UTILS.get() {
                    return (orig.get_result)(this, call, callback, cub, expected, failed);
                }
                return 0;
            }
        }
    };
    if !failed.is_null() {
        *failed = if call_failed { 1 } else { 0 };
    }
    if expected != 0 && expected != HTTP_CALLBACK_COMPLETED {
        return 0;
    }
    if callback.is_null() || cub < 28 {
        log_msg(&format!("GetAPICallResult reject cub={} expected={}", cub, expected));
        return 0;
    }
    let packed = pack_http_completed(handle, context, ok, status, body_len);
    let n = std::cmp::min(cub as usize, packed.len());
    ptr::copy_nonoverlapping(packed.as_ptr(), callback, n);
    log_msg(&format!(
        "GetAPICallResult call={} handle={} status={} bytes={} cub={}",
        call, handle, status, body_len, cub
    ));
    1
}

unsafe extern "system" fn hook_run_callbacks() {
    if let Some(orig) = ORIG_RUN_CALLBACKS.get() {
        orig();
    }
    dispatch_steam_user_callbacks();
    dispatch_http_callresults();
}

unsafe extern "system" fn hook_reg_call(cb: *mut c_void, call: u64) {
    if !cb.is_null() && call != 0 {
        state().lock().unwrap().call_results.insert(call, cb as usize);
        log_msg(&format!("RegisterCallResult call={:#x} cb={:p}", call, cb));
    }
    if let Some(orig) = ORIG_REG_CALL.get() {
        orig(cb, call);
    }
}

unsafe extern "system" fn hook_unreg_call(cb: *mut c_void, call: u64) {
    state().lock().unwrap().call_results.remove(&call);
    if let Some(orig) = ORIG_UNREG_CALL.get() {
        orig(cb, call);
    }
}

fn dispatch_http_callresults() {
    let jobs: Vec<(u64, usize, [u8; 32], u8)> = {
        let mut st = state().lock().unwrap();
        let mut out = Vec::new();
        let calls: Vec<u64> = st.calls.keys().copied().collect();
        for call in calls {
            if st.dispatched.contains(&call) {
                continue;
            }
            let Some(cb) = st.call_results.get(&call).copied() else {
                continue;
            };
            let c = st.calls.get(&call).unwrap();
            let r = st.requests.get(&c.request);
            let packed = pack_http_completed(
                c.request,
                r.map(|x| x.context).unwrap_or(0),
                r.map(|x| x.ok).unwrap_or(false),
                r.map(|x| x.status).unwrap_or(0),
                r.map(|x| x.response.len() as u32).unwrap_or(0),
            );
            let failed = if c.failed { 1 } else { 0 };
            st.dispatched.insert(call);
            out.push((call, cb, packed, failed));
        }
        out
    };
    for (call, cb, packed, failed) in jobs {
        unsafe {
            let obj = cb as *mut c_void;
            if obj.is_null() {
                continue;
            }
            let vptr = *(obj as *mut *mut c_void);
            if vptr.is_null() {
                continue;
            }
            let slots = vptr as *mut *mut c_void;
            // SAFETY: `cb` is a CCallResult the game registered with RegisterCallResult; vtable
            // slot 1 is CCallResult::Run (this, void* result, u8 io_failed, u64 apicall). `packed`
            // is built to the layout the game's HTTPRequestCompleted_t handler reads.
            let run: unsafe extern "system" fn(*mut c_void, *mut u8, u8, u64) =
                std::mem::transmute(*slots.add(1));
            let mut buf = packed;
            run(obj, buf.as_mut_ptr(), failed, call);
            log_msg(&format!("dispatched HTTPRequestCompleted_t call={:#x}", call));
        }
    }
}

/// # Safety
/// `module` must be the base of a live, loaded PE image (or null); its headers and import
/// tables stay mapped for the process lifetime. `new` must be an `extern "system"` function
/// whose ABI matches `func`.
unsafe fn iat_replace(module: *mut u8, dll: &str, func: &str, new: usize) -> Option<usize> {
    if module.is_null() {
        return None;
    }
    let e_lfanew = *(module.add(0x3C) as *const u32) as usize;
    let opt = module.add(e_lfanew + 24);
    let magic = *(opt as *const u16);
    if magic != 0x20b {
        return None;
    }
    let import_rva = *(opt.add(120) as *const u32) as usize;
    if import_rva == 0 {
        return None;
    }
    let mut desc = module.add(import_rva);
    for _desc in 0..256 {
        let orig_thunk = *(desc as *const u32);
        let name_rva = *(desc.add(12) as *const u32);
        let first_thunk = *(desc.add(16) as *const u32);
        if name_rva == 0 && first_thunk == 0 {
            break;
        }
        let iname = {
            let p = module.add(name_rva as usize) as *const i8;
            let mut n = 0usize;
            while n < 128 && *p.add(n) != 0 {
                n += 1;
            }
            String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, n)).to_lowercase()
        };
        if iname == dll {
            let ilt_rva = if orig_thunk != 0 { orig_thunk } else { first_thunk };
            let mut ilt = module.add(ilt_rva as usize) as *mut u64;
            let mut iat = module.add(first_thunk as usize) as *mut u64;
            for _thunk in 0..4096 {
                if *ilt == 0 {
                    break;
                }
                if *ilt & (1u64 << 63) == 0 {
                    let fname = {
                        let p = module.add((*ilt as usize) + 2) as *const i8;
                        let mut n = 0usize;
                        while n < 128 && *p.add(n) != 0 {
                            n += 1;
                        }
                        String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, n)).into_owned()
                    };
                    if fname == func {
                        let old = *iat as usize;
                        let mut prot = 0u32;
                        VirtualProtect(iat as *mut c_void, 8, PAGE_EXECUTE_READWRITE, &mut prot);
                        *iat = new as u64;
                        VirtualProtect(iat as *mut c_void, 8, prot, &mut prot);
                        return Some(old);
                    }
                }
                ilt = ilt.add(1);
                iat = iat.add(1);
            }
        }
        desc = desc.add(20);
    }
    None
}

unsafe extern "C" fn hook_svc_cfg(
    endpoint: *const c_char,
    title: *const c_char,
    handle: *mut *mut c_void,
) -> i32 {
    let ep = from_c(endpoint);
    let t = from_c(title);
    log_msg(&format!("PFServiceConfigCreateHandle ep={} title={}", ep, t));
    let rewrite = CString::new(lan_cfg().origin()).unwrap();
    if let Some(orig) = ORIG_SVC_CFG.get() {
        return orig(rewrite.as_ptr(), title, handle);
    }
    0x8000_4005u32 as i32
}

fn hook_playfab_iat() {
    // SAFETY: `exe` is this process's main module (always a valid image base), and the
    // replacement has the exact signature of PFServiceConfigCreateHandle.
    unsafe {
        let exe = GetModuleHandleA(ptr::null()) as *mut u8;
        if ORIG_SVC_CFG.get().is_some() {
            return;
        }
        if let Some(old) = iat_replace(
            exe,
            "playfabcore.win32.dll",
            "PFServiceConfigCreateHandle",
            hook_svc_cfg as *const () as usize,
        ) {
            let _ = ORIG_SVC_CFG.set(std::mem::transmute(old));
            log_msg(&format!("IAT PFServiceConfigCreateHandle {:#x}", old));
        } else {
            log_msg("IAT PFServiceConfigCreateHandle not found");
        }
    }
}

fn from_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe {
        let mut n = 0usize;
        while *p.add(n) != 0 {
            n += 1;
            if n > 512 {
                break;
            }
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}

/// Hook replacement for winhttp!WinHttpConnect. `session` and `host` belong to the caller;
/// `host` is a NUL-terminated UTF-16 string that is only read (length-capped) here. The other
/// arguments pass through unchanged to the original WinHttpConnect.
unsafe extern "system" fn hook_wh_connect(
    session: *mut c_void,
    host: *const u16,
    port: u16,
    reserved: u32,
) -> *mut c_void {
    let cfg = lan_cfg();
    let host_s = from_wide(host);
    // 443/80 are Cygames/PlayFab TLS REST → LAN HTTP. Keep 8081 (and any other
    // already-rewritten LAN port) so WinHTTP does not pool the websocket onto the
    // HTTP socket. Old code always passed cfg.port, so ws://host:8081 never hit
    // the broker and guest join_lobby was never ACKed (1C).
    let dest_host = if host_s == cfg.host {
        host_s.clone()
    } else {
        cfg.host.clone()
    };
    let dest_port = match port {
        80 | 443 => cfg.port,
        p => p,
    };
    log_msg(&format!(
        "WinHttpConnect {host_s}:{port} -> {dest_host}:{dest_port}"
    ));
    let wh = to_wide(&dest_host);
    if let Some(orig) = ORIG_WH_CONNECT.get() {
        orig(session, wh.as_ptr(), dest_port, reserved)
    } else {
        ptr::null_mut()
    }
}

unsafe extern "system" fn hook_wh_open_request(
    connect: *mut c_void,
    verb: *const u16,
    object: *const u16,
    version: *const u16,
    referrer: *const u16,
    accept: *const *const u16,
    flags: u32,
) -> *mut c_void {
    // Broker WS/HTTP are plaintext. Relink still sets WINHTTP_FLAG_SECURE for the
    // Cygames websocket (TLS ClientHello on 8081 → 1C). Strip it so the upgrade
    // GET is sent in the clear.
    let flags2 = flags & !WINHTTP_FLAG_SECURE;
    if flags2 != flags {
        log_msg(&format!(
            "WinHttpOpenRequest strip SECURE {:#x} {} {}",
            flags,
            from_wide(verb),
            from_wide(object)
        ));
    }
    if let Some(orig) = ORIG_WH_OPEN.get() {
        orig(connect, verb, object, version, referrer, accept, flags2)
    } else {
        ptr::null_mut()
    }
}

unsafe extern "system" fn hook_wh_send(
    request: *mut c_void,
    headers: *const u16,
    hlen: u32,
    optional: *const u8,
    olen: u32,
    total: u32,
    context: usize,
) -> i32 {
    let mut sec = SECURITY_FLAG_IGNORE_UNKNOWN_CA
        | SECURITY_FLAG_IGNORE_CERT_WRONG_USAGE
        | SECURITY_FLAG_IGNORE_CERT_CN_INVALID
        | SECURITY_FLAG_IGNORE_CERT_DATE_INVALID;
    unsafe {
        WinHttpSetOption(
            request,
            WINHTTP_OPTION_SECURITY_FLAGS,
            &mut sec as *mut u32 as *mut c_void,
            4,
        );
    }
    if let Some(orig) = ORIG_WH_SEND.get() {
        orig(request, headers, hlen, optional, olen, total, context)
    } else {
        0
    }
}

fn hook_winhttp_modules() {
    // SAFETY: modules are resolved by name from the loader; iat_replace validates each import
    // descriptor before writing, and every replacement has the WinHTTP ABI.
    unsafe {
        let w = GetModuleHandleA(b"winhttp.dll\0".as_ptr());
        if w.is_null() {
            return;
        }
        if ORIG_WH_CONNECT.get().is_none() {
            let c = GetProcAddress(w, b"WinHttpConnect\0".as_ptr());
            let s = GetProcAddress(w, b"WinHttpSendRequest\0".as_ptr());
            let o = GetProcAddress(w, b"WinHttpOpenRequest\0".as_ptr());
            if !c.is_null() {
                let _ = ORIG_WH_CONNECT.set(std::mem::transmute(c));
            }
            if !s.is_null() {
                let _ = ORIG_WH_SEND.set(std::mem::transmute(s));
            }
            if !o.is_null() {
                let _ = ORIG_WH_OPEN.set(std::mem::transmute(o));
            }
        }
        if ORIG_WH_CONNECT.get().is_none() || ORIG_WH_SEND.get().is_none() {
            return;
        }
        for name in [
            b"PlayFabCore.Win32.dll\0".as_ptr(),
            b"libHttpClient.Win32.dll\0".as_ptr(),
            b"PlayFabServices.Win32.dll\0".as_ptr(),
            ptr::null(),
        ] {
            let m = GetModuleHandleA(name) as *mut u8;
            if m.is_null() {
                continue;
            }
            if let Some(old) = iat_replace(
                m,
                "winhttp.dll",
                "WinHttpConnect",
                hook_wh_connect as *const () as usize,
            ) {
                if old != hook_wh_connect as *const () as usize {
                    log_msg(&format!("hooked WinHttpConnect in {:p}", m));
                }
            }
            if let Some(old) = iat_replace(
                m,
                "winhttp.dll",
                "WinHttpSendRequest",
                hook_wh_send as *const () as usize,
            ) {
                if old != hook_wh_send as *const () as usize {
                    log_msg(&format!("hooked WinHttpSendRequest in {:p}", m));
                }
            }
            if ORIG_WH_OPEN.get().is_some() {
                if let Some(old) = iat_replace(
                    m,
                    "winhttp.dll",
                    "WinHttpOpenRequest",
                    hook_wh_open_request as *const () as usize,
                ) {
                    if old != hook_wh_open_request as *const () as usize {
                        log_msg(&format!("hooked WinHttpOpenRequest in {:p}", m));
                    }
                }
            }
        }
    }
}

fn hook_steam_iat() {
    if IAT_HOOKED.swap(true, Ordering::SeqCst) {
        return;
    }
    // SAFETY: the patched IAT slots belong to this process's own image, and each replacement
    // has the exact Steamworks ABI of the function it replaces.
    unsafe {
        let exe = GetModuleHandleA(ptr::null()) as *mut u8;
        if let Some(old) = iat_replace(
            exe,
            "steam_api64.dll",
            "SteamAPI_RunCallbacks",
            hook_run_callbacks as usize,
        ) {
            let _ = ORIG_RUN_CALLBACKS.set(std::mem::transmute(old));
            log_msg(&format!("IAT SteamAPI_RunCallbacks {:#x}", old));
        } else {
            log_msg("IAT SteamAPI_RunCallbacks not found");
        }
        if let Some(old) = iat_replace(
            exe,
            "steam_api64.dll",
            "SteamAPI_RegisterCallResult",
            hook_reg_call as usize,
        ) {
            let _ = ORIG_REG_CALL.set(std::mem::transmute(old));
            log_msg(&format!("IAT SteamAPI_RegisterCallResult {:#x}", old));
        } else {
            log_msg("IAT SteamAPI_RegisterCallResult not found");
        }
        if let Some(old) = iat_replace(
            exe,
            "steam_api64.dll",
            "SteamAPI_UnregisterCallResult",
            hook_unreg_call as usize,
        ) {
            let _ = ORIG_UNREG_CALL.set(std::mem::transmute(old));
        }
        if let Some(old) = iat_replace(
            exe,
            "steam_api64.dll",
            "SteamAPI_RegisterCallback",
            hook_reg_cb as usize,
        ) {
            let _ = ORIG_REG_CB.set(std::mem::transmute(old));
            log_msg(&format!("IAT SteamAPI_RegisterCallback {:#x}", old));
        } else {
            log_msg("IAT SteamAPI_RegisterCallback not found");
        }
        if let Some(old) = iat_replace(
            exe,
            "steam_api64.dll",
            "SteamAPI_UnregisterCallback",
            hook_unreg_cb as usize,
        ) {
            let _ = ORIG_UNREG_CB.set(std::mem::transmute(old));
        }
    }
}

/// Build the replacement ISteamHTTP vtable. The page is VirtualAlloc'd EXECUTE_READWRITE and
/// zeroed, so unimplemented slots stay null (never call them) and nothing must free it: it
/// lives for the process lifetime.
fn http_vtable() -> *mut *mut c_void {
    // SAFETY: VirtualAlloc returns process-lifetime memory; index arithmetic stays inside the
    // 32 allocated slots.
    unsafe {
        let mem = VirtualAlloc(
            ptr::null_mut(),
            32 * std::mem::size_of::<*mut c_void>(),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        ) as *mut *mut c_void;
        let fns: [*mut c_void; 25] = [
            create_http as *mut c_void,
            set_context as *mut c_void,
            set_timeout as *mut c_void,
            set_header as *mut c_void,
            set_param as *mut c_void,
            send_http as *mut c_void,
            send_stream as *mut c_void,
            bool_true as *mut c_void,
            bool_true as *mut c_void,
            header_size as *mut c_void,
            header_value as *mut c_void,
            body_size as *mut c_void,
            body_data as *mut c_void,
            stream_body as *mut c_void,
            release as *mut c_void,
            progress as *mut c_void,
            set_raw_body as *mut c_void,
            create_cookie_container as *mut c_void,
            release_cookie_container as *mut c_void,
            set_cookie as *mut c_void,
            set_cookie_container as *mut c_void,
            set_ua as *mut c_void,
            set_verify as *mut c_void,
            set_abs_timeout as *mut c_void,
            was_timeout as *mut c_void,
        ];
        for (i, f) in fns.iter().enumerate() {
            *mem.add(i) = *f;
        }
        mem
    }
}

fn try_patch() -> bool {
    unsafe {
        let steam = GetModuleHandleA(b"steam_api64.dll\0".as_ptr());
        if steam.is_null() {
            return false;
        }
        let init = GetProcAddress(steam, b"SteamAPI_Init\0".as_ptr());
        let find = GetProcAddress(steam, b"SteamInternal_FindOrCreateUserInterface\0".as_ptr());
        let huser_fn = GetProcAddress(steam, b"SteamAPI_GetHSteamUser\0".as_ptr());
        if init.is_null() || find.is_null() || huser_fn.is_null() {
            return false;
        }
        type FindFn = unsafe extern "system" fn(i32, *const u8) -> *mut c_void;
        type HUserFn = unsafe extern "system" fn() -> i32;
        // SAFETY: both symbols were resolved from steam_api64.dll; these are their documented
        // Steamworks signatures.
        let find: FindFn = std::mem::transmute(find);
        let huser: HUserFn = std::mem::transmute(huser_fn);
        let reported = huser();
        let mut http = ptr::null_mut();
        let mut utils = ptr::null_mut();
        for user in [reported, 1, 0] {
            http = find(user, b"STEAMHTTP_INTERFACE_VERSION003\0".as_ptr());
            utils = find(user, b"SteamUtils010\0".as_ptr());
            if utils.is_null() {
                utils = find(user, b"SteamUtils009\0".as_ptr());
            }
            if !http.is_null() && !utils.is_null() {
                break;
            }
        }
        if http.is_null() || utils.is_null() {
            return false;
        }
        let ours = http_vtable();
        // SAFETY: `http` is a live interface pointer from the emulator's
        // FindOrCreateUserInterface; the first field of an interface object is its vtable
        // pointer, which is exactly what is overwritten. The replacement has the Steam ABI.
        *(http as *mut *mut c_void) = ours as *mut c_void;
        let uv = *(utils as *mut *mut c_void) as *mut *mut c_void;
        let is_c = *uv.add(UTILS_IS_CALL_COMPLETED);
        let get_r = *uv.add(UTILS_GET_CALL_RESULT);
        let _ = ORIG_UTILS.set(UtilsOrig {
            is_completed: std::mem::transmute(is_c),
            get_result: std::mem::transmute(get_r),
        });
        patch_slot(uv, UTILS_IS_CALL_COMPLETED, utils_is_completed as *mut c_void);
        patch_slot(uv, UTILS_GET_CALL_RESULT, utils_get_result as *mut c_void);
        for ver in [
            b"SteamUser023\0".as_ptr(),
            b"SteamUser022\0".as_ptr(),
            b"SteamUser021\0".as_ptr(),
        ] {
            let mut user = ptr::null_mut();
            for uid in [reported, 1, 0] {
                user = find(uid, ver);
                if !user.is_null() {
                    break;
                }
            }
            if user.is_null() {
                continue;
            }
            let uvt = *(user as *mut *mut c_void) as *mut *mut c_void;
            let orig_web = *uvt.add(USER_GET_AUTH_TICKET_WEBAPI);
            if !orig_web.is_null() {
                let _ = ORIG_WEBAPI.set(std::mem::transmute(orig_web));
            }
            patch_slot(uvt, USER_BLOGGED_ON, blogged_on as *mut c_void);
            patch_slot(uvt, USER_GET_AUTH_TICKET_WEBAPI, get_auth_ticket_webapi as *mut c_void);
            log_msg(&format!("patched ISteamUser at {:p}", user));
            break;
        }
        hook_winhttp_modules();
        log_msg(&format!("patched ISteamHTTP at {:p} and SteamUtils at {:p}", http, utils));
        true
    }
}

fn patch_thread() {
    hook_steam_iat();
    hook_playfab_iat();
    hook_winhttp_modules();
    // Relink can sit on splash/shaders well past 20s before SteamAPI_Init.
    for i in 0..6000 {
        if PATCHED.load(Ordering::SeqCst) {
            return;
        }
        hook_winhttp_modules();
        if try_patch() {
            PATCHED.store(true, Ordering::SeqCst);
            return;
        }
        if i % 50 == 0 {
            unsafe {
                let steam = GetModuleHandleA(b"steam_api64.dll\0".as_ptr());
                let huser_fn = if steam.is_null() {
                    ptr::null_mut()
                } else {
                    GetProcAddress(steam, b"SteamAPI_GetHSteamUser\0".as_ptr())
                };
                let user = if huser_fn.is_null() {
                    -1
                } else {
                    let f: unsafe extern "system" fn() -> i32 = std::mem::transmute(huser_fn);
                    f()
                };
                log_msg(&format!(
                    "waiting for SteamAPI_Init (steam={:p} huser={})",
                    steam, user
                ));
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    log_msg("gave up waiting for SteamAPI_Init / ISteamHTTP");
}

/// # Safety
/// Loads the real %SystemRoot%\System32\version.dll (never a game-local copy, which would
/// recurse); the returned module handle must stay loaded for the process lifetime.
unsafe fn load_sys_version() -> *mut c_void {
    let mut buf = [0u16; 260];
    let n = GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) as usize;
    let mut path: Vec<u16> = buf[..n].to_vec();
    path.extend("\\version.dll".encode_utf16());
    path.push(0);
    LoadLibraryW(path.as_ptr())
}

static SYS_VERSION: OnceLock<usize> = OnceLock::new();

/// # Safety
/// `name` must be a NUL-terminated ASCII export name. The handle comes from
/// `load_sys_version` and remains valid; a failed load yields null, which is rejected here.
unsafe fn sys_proc(name: &[u8]) -> *mut c_void {
    let h = *SYS_VERSION.get().unwrap_or(&0) as *mut c_void;
    if h.is_null() {
        return ptr::null_mut();
    }
    GetProcAddress(h, name.as_ptr())
}

unsafe extern "C" fn on_load() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let sys = load_sys_version();
    let _ = SYS_VERSION.set(sys as usize);
    log_msg(concat!("loaded version.dll proxy build=", env!("BUILD_STAMP")));
    hook_steam_iat();
    hook_playfab_iat();
    hook_winhttp_modules();
    thread::spawn(patch_thread);
}

#[used]
#[link_section = ".CRT$XCU"]
static INIT: unsafe extern "C" fn() = on_load;

// VERSION.dll forwards (four-usize thunks cover the real APIs).
#[no_mangle]
pub unsafe extern "system" fn GetFileVersionInfoA(a: usize, b: usize, c: usize, d: usize) -> usize {
    let p = sys_proc(b"GetFileVersionInfoA\0");
    if p.is_null() {
        return 0;
    }
    let f: unsafe extern "system" fn(usize, usize, usize, usize) -> usize = std::mem::transmute(p);
    f(a, b, c, d)
}
#[no_mangle]
pub unsafe extern "system" fn GetFileVersionInfoW(a: usize, b: usize, c: usize, d: usize) -> usize {
    let p = sys_proc(b"GetFileVersionInfoW\0");
    if p.is_null() {
        return 0;
    }
    let f: unsafe extern "system" fn(usize, usize, usize, usize) -> usize = std::mem::transmute(p);
    f(a, b, c, d)
}
#[no_mangle]
pub unsafe extern "system" fn GetFileVersionInfoSizeA(a: usize, b: usize) -> usize {
    let p = sys_proc(b"GetFileVersionInfoSizeA\0");
    if p.is_null() {
        return 0;
    }
    let f: unsafe extern "system" fn(usize, usize) -> usize = std::mem::transmute(p);
    f(a, b)
}
#[no_mangle]
pub unsafe extern "system" fn GetFileVersionInfoSizeW(a: usize, b: usize) -> usize {
    let p = sys_proc(b"GetFileVersionInfoSizeW\0");
    if p.is_null() {
        return 0;
    }
    let f: unsafe extern "system" fn(usize, usize) -> usize = std::mem::transmute(p);
    f(a, b)
}
#[no_mangle]
pub unsafe extern "system" fn VerQueryValueA(a: usize, b: usize, c: usize, d: usize) -> usize {
    let p = sys_proc(b"VerQueryValueA\0");
    if p.is_null() {
        return 0;
    }
    let f: unsafe extern "system" fn(usize, usize, usize, usize) -> usize = std::mem::transmute(p);
    f(a, b, c, d)
}
#[no_mangle]
pub unsafe extern "system" fn VerQueryValueW(a: usize, b: usize, c: usize, d: usize) -> usize {
    let p = sys_proc(b"VerQueryValueW\0");
    if p.is_null() {
        return 0;
    }
    let f: unsafe extern "system" fn(usize, usize, usize, usize) -> usize = std::mem::transmute(p);
    f(a, b, c, d)
}
#[no_mangle]
pub unsafe extern "system" fn GetFileVersionInfoSizeExW(a: usize, b: usize, c: usize) -> usize {
    let p = sys_proc(b"GetFileVersionInfoSizeExW\0");
    if p.is_null() {
        return 0;
    }
    let f: unsafe extern "system" fn(usize, usize, usize) -> usize = std::mem::transmute(p);
    f(a, b, c)
}
#[no_mangle]
pub unsafe extern "system" fn GetFileVersionInfoExW(a: usize, b: usize, c: usize, d: usize, e: usize) -> usize {
    let p = sys_proc(b"GetFileVersionInfoExW\0");
    if p.is_null() {
        return 0;
    }
    let f: unsafe extern "system" fn(usize, usize, usize, usize, usize) -> usize = std::mem::transmute(p);
    f(a, b, c, d, e)
}

#[no_mangle]
pub unsafe extern "system" fn DllMain(_mod: *mut c_void, reason: u32, _res: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        on_load();
    }
    1
}

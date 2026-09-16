// Robust offline test suite for the async broker worker (Plan A / AUDIT_PLAYFAB PF-06).
//
// Loads the freshly built shim and drives it exactly like the game does:
//   PFMultiplayerInitialize -> SetEntityToken -> Create/Join/Find/PostUpdate/Leave
//   -> PFMultiplayerStartProcessingLobbyStateChanges / Finish -> read state changes.
//
// An in-process mock broker makes timing controllable, so the suite can prove the async
// contract instead of just the happy path:
//   * exports return before the broker answers (no HTTP on the game's thread);
//   * PFMultiplayerStartProcessing never blocks even while the worker is stuck in a 2 s call;
//   * completions arrive only through Start/Finish, in the documented order
//     (MemberAdded -> Updated -> Completed);
//   * a join parks until the host publishes a real network_descriptor, then completes;
//   * service errors are delivered asynchronously in the completion result;
//   * a stale in-flight job from a previous Initialize generation is discarded;
//   * create/join/find/leave/postupdate all complete and carry the right fields.
//
// Build / run:
//   cd playfab-mp-shim
//   rustc --edition 2021 -O tests\async_broker_test.rs -o tests\async_broker_test.exe
//   tests\async_broker_test.exe PlayFabMultiplayerWin.dll
//
// Exits non-zero if any expectation fails.

use std::ffi::{c_void, CString};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(path: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const i8) -> *mut c_void;
}

#[repr(C)]
struct EntityKey {
    id: *const i8,
    type_: *const i8,
}

#[repr(C)]
struct CreateCfg {
    max: u32,
    owner_policy: u32,
    access: u32,
    search_count: u32,
    search_keys: *const *const i8,
    search_vals: *const *const i8,
    lobby_count: u32,
    _pad: u32,
    lobby_keys: *const *const i8,
    lobby_vals: *const *const i8,
}

/// PFLobbyDataUpdate (1.8): newOwner@+0, maxMemberCount@+8, accessPolicy@+0x10,
/// membershipLock@+0x18, searchCount@+0x20, searchKeys@+0x28, searchValues@+0x30,
/// lobbyCount@+0x38, lobbyKeys@+0x40, lobbyValues@+0x48.
#[repr(C)]
struct DataUpdate {
    new_owner: *const EntityKey,
    max_member_count: *const u32,
    access_policy: *const u32,
    membership_lock: *const u32,
    search_count: u32,
    _pad: u32,
    search_keys: *const *const i8,
    search_vals: *const *const i8,
    lobby_count: u32,
    _pad2: u32,
    lobby_keys: *const *const i8,
    lobby_vals: *const *const i8,
}

#[repr(C)]
struct SearchCfg {
    friends_filter: *const c_void,
    filter_string: *const i8,
    sort_string: *const i8,
    client_search_result_count: *const u32,
}

#[derive(Clone, Copy)]
struct Sc {
    ty: u32,
    result: i32,
    n: u32,
}

static FAILURES: AtomicU64 = AtomicU64::new(0);

macro_rules! check {
    ($cond:expr, $($t:tt)*) => {
        if $cond {
            println!("ok  : {}", format!($($t)*));
        } else {
            FAILURES.fetch_add(1, Ordering::SeqCst);
            println!("FAIL: {}", format!($($t)*));
        }
    };
}

// ---------------------------------------------------------------------------
// Mock broker
// ---------------------------------------------------------------------------

struct MockState {
    delay_ms: u64,
    requests: Vec<String>,
    descriptor_ready: bool,
    fail_path: Option<String>,
    lobby_data: std::collections::HashMap<String, String>,
    search_data: std::collections::HashMap<String, String>,
}

struct MockBroker {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
}

impl MockBroker {
    fn start() -> MockBroker {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock broker");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(MockState {
            delay_ms: 0,
            requests: Vec::new(),
            descriptor_ready: true,
            fail_path: None,
            lobby_data: [("invitation_identifier", "lan-invite")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            search_data: [("number_key1", "1"), ("string_key1", "lan-test")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }));
        let st = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let st = st.clone();
                std::thread::spawn(move || {
                    let _ = handle_mock(&mut stream, &st);
                });
            }
        });
        MockBroker { addr, state }
    }

    fn stub(&self) -> String {
        format!("{}:{}", self.addr.ip(), self.addr.port())
    }

    fn set_delay(&self, ms: u64) {
        self.state.lock().unwrap().delay_ms = ms;
    }

    fn set_descriptor_ready(&self, ready: bool) {
        self.state.lock().unwrap().descriptor_ready = ready;
    }

    fn fail_path(&self, path: Option<&str>) {
        self.state.lock().unwrap().fail_path = path.map(|s| s.to_string());
    }

}

fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn read_json_string(bytes: &[u8], mut i: usize) -> Option<(String, usize)> {
    if i >= bytes.len() || bytes[i] != b'"' {
        return None;
    }
    i += 1;
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    return None;
                }
                out.push(bytes[i] as char);
                i += 1;
            }
            b'"' => return Some((out, i + 1)),
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    None
}

/// `"key":{...}` tail of a flat JSON object (matching brace, string-aware).
fn object_of<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":{{");
    let start = body.find(&pat)? + pat.len();
    let b = body.as_bytes();
    let mut depth = 1i32;
    let mut in_str = false;
    let mut esc = false;
    let mut i = start;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
        } else if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(&body[start..i]);
            }
        }
        i += 1;
    }
    None
}

fn parse_pairs(blob: &str) -> Vec<(String, String)> {
    let b = blob.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i] != b'"' {
            i += 1;
        }
        let Some((k, ni)) = read_json_string(b, i) else {
            break;
        };
        i = ni;
        while i < b.len() && b[i] != b':' {
            i += 1;
        }
        i += 1;
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b'"' {
            if let Some((v, ni)) = read_json_string(b, i) {
                out.push((k, v));
                i = ni;
                continue;
            }
        }
        while i < b.len() && b[i] != b',' {
            i += 1;
        }
    }
    out
}

fn handle_mock(stream: &mut TcpStream, state: &Arc<Mutex<MockState>>) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let (method, path, _body);
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = header_end(&buf) {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let cl = head
                .lines()
                .find_map(|l| {
                    let lower = l.to_ascii_lowercase();
                    lower
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if buf.len() >= pos + 4 + cl {
                let request_line = head.lines().next().unwrap_or("");
                let mut it = request_line.split_whitespace();
                method = it.next().unwrap_or("").to_string();
                path = it.next().unwrap_or("").to_string();
                _body = String::from_utf8_lossy(&buf[pos + 4..pos + 4 + cl]).to_string();
                break;
            }
        }
    }

    let (delay, fail) = {
        let mut g = state.lock().unwrap();
        g.requests.push(format!("{method} {path}"));
        (
            g.delay_ms,
            g.fail_path
                .as_ref()
                .map(|f| path.contains(f.as_str()))
                .unwrap_or(false),
        )
    };
    if delay > 0 {
        std::thread::sleep(Duration::from_millis(delay));
    }

    if fail {
        let body = "{\"code\":500,\"status\":\"InternalServerError\",\"error\":\"MockFailure\"}";
        write_response(stream, 500, body)?;
        return Ok(());
    }

    // The mock echoes property updates so the shim's GetLobby poll observes them (like the real
    // broker), instead of overwriting the local apply with stale canned data.
    if path.contains("CreateAndJoinLobby") || path.contains("UpdateLobby") {
        let mut g = state.lock().unwrap();
        if let Some(ld) = object_of(&_body, "LobbyData") {
            let fresh: std::collections::HashMap<String, String> = parse_pairs(ld).into_iter().collect();
            if path.contains("CreateAndJoinLobby") {
                g.lobby_data = fresh;
            } else {
                g.lobby_data.extend(fresh);
            }
        }
        if let Some(sd) = object_of(&_body, "SearchData") {
            let fresh: std::collections::HashMap<String, String> = parse_pairs(sd).into_iter().collect();
            if path.contains("CreateAndJoinLobby") {
                g.search_data = fresh;
            } else {
                g.search_data.extend(fresh);
            }
        }
    }

    let lobby_json = {
        let g = state.lock().unwrap();
        let mut lobby_data = g.lobby_data.clone();
        lobby_data.insert(
            "network_descriptor".into(),
            if g.descriptor_ready {
                "LAN1.18d59460-0000-0000-0000-000000000000.c0a86470".into()
            } else {
                "dummy".into()
            },
        );
        lobby_data.insert("invitation_identifier".into(), "lan-invite".into());
        let mut lk: Vec<&String> = lobby_data.keys().collect();
        lk.sort();
        let lpairs: Vec<String> = lk
            .iter()
            .map(|k| format!("\"{k}\":\"{}\"", lobby_data[*k]))
            .collect();
        let mut sk: Vec<&String> = g.search_data.keys().collect();
        sk.sort();
        let spairs: Vec<String> = sk
            .iter()
            .map(|k| format!("\"{k}\":\"{}\"", g.search_data[*k]))
            .collect();
        format!(
            "{{\"LobbyId\":\"lan-test\",\"ConnectionString\":\"lan.1AC1AD.lan-test\",\"MaxPlayers\":4,\
             \"MembershipLock\":\"Unlocked\",\"AccessPolicy\":\"Public\",\
             \"Owner\":{{\"Id\":\"owner-1\",\"Type\":\"title_player_account\"}},\
             \"Members\":[{{\"Id\":\"owner-1\",\"MemberEntity\":{{\"Id\":\"owner-1\",\"Type\":\"title_player_account\"}},\
             \"MemberData\":{{\"member_platform\":\"Steam\",\"member_platform_account_id\":\"76561190000000001\",\"member_platform_user_name\":\"Host\"}}}}],\
             \"LobbyData\":{{{}}},\
             \"SearchData\":{{{}}}}}",
            lpairs.join(","),
            spairs.join(",")
        )
    };

    let body = if path.contains("CreateAndJoinLobby") {
        "{\"code\":200,\"data\":{\"LobbyId\":\"lan-test\",\"ConnectionString\":\"lan.1AC1AD.lan-test\",\"MaxPlayers\":4},\"status\":\"OK\"}".to_string()
    } else if path.contains("JoinLobby") {
        "{\"code\":200,\"data\":{\"LobbyId\":\"lan-test\",\"MaxPlayers\":4},\"status\":\"OK\"}".to_string()
    } else if path.contains("GetLobby") {
        format!("{{\"code\":200,\"data\":{lobby_json},\"status\":\"OK\"}}")
    } else if path.contains("FindLobbies") {
        format!(
            "{{\"code\":200,\"data\":{{\"Lobbies\":[{lobby_json}],\"Pagination\":{{}}}},\"status\":\"OK\"}}"
        )
    } else if path.contains("party/peers") {
        "{\"peers\":[{\"entity_id\":\"owner-1\"}]}".to_string()
    } else {
        "{\"code\":200,\"data\":{},\"status\":\"OK\"}".to_string()
    };
    write_response(stream, 200, &body)
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// Shim bindings
// ---------------------------------------------------------------------------

struct Shim {
    handle: *mut c_void,
    init: extern "system" fn(*const i8, *mut *mut c_void) -> i32,
    uninit: extern "system" fn(*mut c_void) -> i32,
    set_token: extern "system" fn(*mut c_void, *const EntityKey, *const i8) -> i32,
    create: extern "system" fn(
        *mut c_void,
        *const EntityKey,
        *const u8,
        *const u8,
        *mut c_void,
        *mut *mut c_void,
    ) -> i32,
    join: extern "system" fn(
        *mut c_void,
        *const EntityKey,
        *const i8,
        *const u8,
        *mut c_void,
        *mut *mut c_void,
    ) -> i32,
    find: extern "system" fn(*mut c_void, *const EntityKey, *const c_void, *mut c_void) -> i32,
    post: extern "system" fn(*mut c_void, *const EntityKey, *const u8, *const u8, *mut c_void) -> i32,
    leave: extern "system" fn(*mut c_void, *const EntityKey, *mut c_void) -> i32,
    start: extern "system" fn(*mut c_void, *mut u32, *mut *mut *mut u8) -> i32,
    finish: extern "system" fn(*mut c_void, u32, *mut *mut u8) -> i32,
    get_lobby_id: extern "system" fn(*mut c_void, *mut *const i8) -> i32,
    get_owner: extern "system" fn(*mut c_void, *mut *const EntityKey) -> i32,
    get_lock: extern "system" fn(*mut c_void, *mut i32) -> i32,
    get_prop: extern "system" fn(*mut c_void, *const i8, *mut *const i8) -> i32,
}

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn load_shim(path: &str) -> *mut c_void {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe { LoadLibraryW(wide.as_ptr()) };
    assert!(!h.is_null(), "LoadLibrary failed for {path}");
    h
}

impl Shim {
    fn new(lib: *mut c_void) -> Shim {
        let get = |n: &str| -> *mut c_void {
            let c = cs(n);
            let p = unsafe { GetProcAddress(lib, c.as_ptr()) };
            assert!(!p.is_null(), "missing export {n}");
            p
        };
        unsafe {
            Shim {
                handle: ptr::null_mut(),
                init: std::mem::transmute(get("PFMultiplayerInitialize")),
                uninit: std::mem::transmute(get("PFMultiplayerUninitialize")),
                set_token: std::mem::transmute(get("PFMultiplayerSetEntityToken")),
                create: std::mem::transmute(get("PFMultiplayerCreateAndJoinLobby")),
                join: std::mem::transmute(get("PFMultiplayerJoinLobby")),
                find: std::mem::transmute(get("PFMultiplayerFindLobbies")),
                post: std::mem::transmute(get("PFLobbyPostUpdate")),
                leave: std::mem::transmute(get("PFLobbyLeave")),
                start: std::mem::transmute(get("PFMultiplayerStartProcessingLobbyStateChanges")),
                finish: std::mem::transmute(get("PFMultiplayerFinishProcessingLobbyStateChanges")),
                get_lobby_id: std::mem::transmute(get("PFLobbyGetLobbyId")),
                get_owner: std::mem::transmute(get("PFLobbyGetOwner")),
                get_lock: std::mem::transmute(get("PFLobbyGetMembershipLock")),
                get_prop: std::mem::transmute(get("PFLobbyGetLobbyProperty")),
            }
        }
    }

    fn init(&mut self, title: &str) -> i32 {
        let t = cs(title);
        (self.init)(t.as_ptr(), &mut self.handle)
    }

    fn set_token(&self, id: &str) -> i32 {
        let ek = EntityKey {
            id: cs(id).into_raw(),
            type_: cs("title_player_account").into_raw(),
        };
        let tok = cs("MOCK-TOKEN");
        (self.set_token)(self.handle, &ek, tok.as_ptr())
    }

    /// One Start/Finish pump; appends every change to `log`. Returns the batch size. Start is
    /// timed and must never block, so the caller can assert on it.
    fn pump(&self, log: &mut Vec<Sc>) -> (u32, Duration) {
        let t0 = Instant::now();
        let mut count: u32 = 0;
        let mut changes: *mut *mut u8 = ptr::null_mut();
        let rc = (self.start)(self.handle, &mut count, &mut changes);
        assert_eq!(rc, 0, "Start failed");
        let dt = t0.elapsed();
        if count > 0 && !changes.is_null() {
            for i in 0..count as usize {
                let p = unsafe { *changes.add(i) };
                let ty = unsafe { *(p as *const u32) };
                let result = unsafe { *((p.add(4)) as *const i32) };
                let n = if ty == 12 {
                    unsafe { *((p.add(0x20)) as *const u32) }
                } else {
                    0
                };
                log.push(Sc { ty, result, n });
            }
        }
        (self.finish)(self.handle, count, changes);
        (count, dt)
    }

    /// Pump until `pred` is satisfied over the accumulated log, or the timeout expires.
    fn pump_until(
        &self,
        log: &mut Vec<Sc>,
        timeout: Duration,
        pred: impl Fn(&[Sc]) -> bool,
    ) -> bool {
        let t0 = Instant::now();
        loop {
            self.pump(log);
            if pred(log) {
                return true;
            }
            if t0.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn create_cfg<'a>(
        &self,
        keys: &'a [*const i8],
        vals: &'a [*const i8],
        lobby_keys: &'a [*const i8],
        lobby_vals: &'a [*const i8],
    ) -> CreateCfg {
        CreateCfg {
            max: 4,
            owner_policy: 0,
            access: 0,
            search_count: keys.len() as u32,
            search_keys: keys.as_ptr(),
            search_vals: vals.as_ptr(),
            lobby_count: lobby_keys.len() as u32,
            _pad: 0,
            lobby_keys: lobby_keys.as_ptr(),
            lobby_vals: lobby_vals.as_ptr(),
        }
    }
}

fn type_index(log: &[Sc], ty: u32) -> Option<usize> {
    log.iter().position(|s| s.ty == ty)
}

fn last_of(log: &[Sc], ty: u32) -> Option<Sc> {
    log.iter().rev().copied().find(|s| s.ty == ty)
}

fn has_type(log: &[Sc], ty: u32) -> bool {
    type_index(log, ty).is_some()
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

fn main() {
    let dll = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("PlayFabMultiplayerWin.dll"));
    let broker = MockBroker::start();
    // lan_cfg() is loaded once per process, so the env override must be set before the first
    // shim call.
    std::env::set_var("GBFR_LAN_STUB", broker.stub());
    std::env::set_var("GBFR_LAN_DEBUG", "0");
    let lib = load_shim(&dll.to_string_lossy());
    let mut shim = Shim::new(lib);
    check!(shim.init("async-test") == 0 && !shim.handle.is_null(), "PFMultiplayerInitialize");
    check!(shim.set_token("player-1") == 0, "SetEntityToken");

    let creator = EntityKey {
        id: cs("player-1").into_raw(),
        type_: cs("title_player_account").into_raw(),
    };
    let creator2 = EntityKey {
        id: cs("player-2").into_raw(),
        type_: cs("title_player_account").into_raw(),
    };

    // ------------------------------------------------------------------
    // 1. Create is asynchronous and delivers MemberAdded -> Completed.
    // ------------------------------------------------------------------
    {
        println!("\n-- scenario: create_is_async --");
        broker.set_delay(400);
        let sk = [cs("string_key1").into_raw() as *const i8];
        let sv = [cs("lan-test").into_raw() as *const i8];
        let lk = [cs("comment1").into_raw() as *const i8];
        let lv = [cs("hello").into_raw() as *const i8];
        let cfg = shim.create_cfg(&sk, &sv, &lk, &lv);
        let mut lobby: *mut c_void = ptr::null_mut();
        let t0 = Instant::now();
        let rc = (shim.create)(
            shim.handle,
            &creator,
            &cfg as *const CreateCfg as *const u8,
            ptr::null(),
            ptr::null_mut(),
            &mut lobby,
        );
        let dt = t0.elapsed();
        check!(rc == 0 && !lobby.is_null(), "create returned S_OK + handle (rc=0x{rc:08X})");
        check!(
            dt < Duration::from_millis(150),
            "create returned in {dt:?} while the broker sleeps 400 ms (async)"
        );
        // Identity getters must answer STILL_PENDING before the completion.
        let mut id_out: *const i8 = ptr::null();
        let pending_rc = (shim.get_lobby_id)(lobby, &mut id_out);
        check!(
            pending_rc == 0x8923_6205u32 as i32,
            "PFLobbyGetLobbyId before completion = E_PF_OBJECT_STILL_PENDING (0x{pending_rc:08X})"
        );

        let mut log = Vec::new();
        check!(
            !shim.pump_until(&mut log, Duration::from_millis(250), |l| has_type(l, 0)),
            "no CreateAndJoinLobbyCompleted in the first 250 ms"
        );
        check!(
            shim.pump_until(&mut log, Duration::from_millis(2500), |l| has_type(l, 0)),
            "CreateAndJoinLobbyCompleted arrives after the broker responds"
        );
        let (i2, i0) = (type_index(&log, 2), type_index(&log, 0));
        check!(
            i2.is_some() && i0.is_some() && i2 < i0,
            "MemberAdded (type 2) precedes CreateAndJoinLobbyCompleted (type 0)"
        );
        let done = last_of(&log, 0).unwrap();
        check!(done.result == 0, "create completion result = 0");
        let mut lock = -1i32;
        check!((shim.get_lock)(lobby, &mut lock) == 0, "membership lock readable after completion");
        let mut owner: *const EntityKey = ptr::null();
        check!(
            (shim.get_owner)(lobby, &mut owner) == 0 && !owner.is_null(),
            "owner readable after completion"
        );

        // ------------------------------------------------------------------
        // 2. Start never blocks while the worker is stuck in a 2 s HTTP call.
        // ------------------------------------------------------------------
        println!("\n-- scenario: start_never_blocks --");
        broker.set_delay(2000);
        let t0 = Instant::now();
        let mut worst = Duration::ZERO;
        for _ in 0..25 {
            let (_n, dt) = shim.pump(&mut Vec::new());
            worst = worst.max(dt);
        }
        let total = t0.elapsed();
        check!(
            worst < Duration::from_millis(100),
            "worst Start call {worst:?} (<100 ms) while the worker is blocked"
        );
        check!(
            total < Duration::from_millis(1000),
            "25 Start/Finish pumps in {total:?} (no HTTP on the tick)"
        );
        broker.set_delay(0);
        let _ = shim.pump_until(&mut Vec::new(), Duration::from_millis(3000), |_| true);
    }

    // ------------------------------------------------------------------
    // 3. Join: parks on a missing descriptor, completes after it appears.
    // ------------------------------------------------------------------
    {
        println!("\n-- scenario: join_waits_for_descriptor --");
        broker.set_descriptor_ready(false);
        let mut lobby: *mut c_void = ptr::null_mut();
        let conn = cs("lan.1AC1AD.lan-test");
        let t0 = Instant::now();
        let rc = (shim.join)(
            shim.handle,
            &creator2,
            conn.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            &mut lobby,
        );
        let dt = t0.elapsed();
        check!(rc == 0 && !lobby.is_null(), "join returned S_OK + handle (rc=0x{rc:08X})");
        check!(dt < Duration::from_millis(150), "join returned in {dt:?} (async)");

        let mut log = Vec::new();
        shim.pump_until(&mut log, Duration::from_millis(700), |l| has_type(l, 1));
        check!(
            !has_type(&log, 1),
            "no JoinLobbyCompleted while the host descriptor is a dummy"
        );
        broker.set_descriptor_ready(true);
        check!(
            shim.pump_until(&mut log, Duration::from_millis(3000), |l| has_type(l, 1)),
            "JoinLobbyCompleted arrives once the descriptor is real"
        );
        let (i2, i7, i1) = (
            type_index(&log, 2),
            type_index(&log, 7),
            type_index(&log, 1),
        );
        check!(
            i2.is_some() && i7.is_some() && i1.is_some() && i2 < i7 && i7 < i1,
            "join order MemberAdded (2) -> Updated (7) -> JoinLobbyCompleted (1)"
        );
        check!(
            last_of(&log, 1).unwrap().result == 0,
            "join completion result = 0"
        );
    }

    // ------------------------------------------------------------------
    // 4. FindLobbies: async rows, then an async service error.
    // ------------------------------------------------------------------
    {
        println!("\n-- scenario: find_is_async --");
        broker.set_descriptor_ready(true);
        let count: u32 = 30;
        let filter = cs("number_key1 eq 1");
        let sort = cs("");
        let scfg = SearchCfg {
            friends_filter: ptr::null(),
            filter_string: filter.as_ptr(),
            sort_string: sort.as_ptr(),
            client_search_result_count: &count,
        };
        let t0 = Instant::now();
        let rc = (shim.find)(
            shim.handle,
            &creator,
            &scfg as *const SearchCfg as *const c_void,
            ptr::null_mut(),
        );
        check!(rc == 0, "find returned S_OK immediately (rc=0x{rc:08X})");
        check!(
            t0.elapsed() < Duration::from_millis(150),
            "find returned in {:?} (async)",
            t0.elapsed()
        );
        let mut log = Vec::new();
        check!(
            shim.pump_until(&mut log, Duration::from_millis(3000), |l| has_type(l, 12)),
            "FindLobbiesCompleted (type 12) arrives"
        );
        let find = last_of(&log, 12).unwrap();
        check!(find.result == 0, "find result = 0");
        check!(find.n >= 1, "find returned {} live rows (>=1)", find.n);

        broker.fail_path(Some("FindLobbies"));
        let rc = (shim.find)(
            shim.handle,
            &creator,
            &scfg as *const SearchCfg as *const c_void,
            ptr::null_mut(),
        );
        check!(rc == 0, "failing find still returns S_OK immediately");
        let mut log2 = Vec::new();
        check!(
            shim.pump_until(&mut log2, Duration::from_millis(3000), |l| has_type(l, 12)),
            "error completion arrives"
        );
        let err = last_of(&log2, 12).unwrap();
        check!(err.result != 0, "service error delivered in completion result (0x{:08X})", err.result);
        check!(err.n == 0, "error completion carries 0 rows");
        broker.fail_path(None);
    }

    // ------------------------------------------------------------------
    // 5. PostUpdate: local apply, async completion + Updated echo.
    // ------------------------------------------------------------------
    {
        println!("\n-- scenario: postupdate_is_async --");
        let sk = [cs("string_key2").into_raw() as *const i8];
        let sv = [cs("posted").into_raw() as *const i8];
        let cfg = shim.create_cfg(&sk, &sv, &[], &[]);
        let mut lobby: *mut c_void = ptr::null_mut();
        let rc = (shim.create)(
            shim.handle,
            &creator,
            &cfg as *const CreateCfg as *const u8,
            ptr::null(),
            ptr::null_mut(),
            &mut lobby,
        );
        check!(rc == 0 && !lobby.is_null(), "create for postupdate returned a handle");
        let mut log = Vec::new();
        check!(
            shim.pump_until(&mut log, Duration::from_millis(3000), |l| has_type(l, 0)),
            "create completed"
        );

        let uk = [cs("post1").into_raw() as *const i8];
        let uv = [cs("v1").into_raw() as *const i8];
        let du = DataUpdate {
            new_owner: ptr::null(),
            max_member_count: ptr::null(),
            access_policy: ptr::null(),
            membership_lock: ptr::null(),
            search_count: 0,
            _pad: 0,
            search_keys: ptr::null(),
            search_vals: ptr::null(),
            lobby_count: 1,
            _pad2: 0,
            lobby_keys: uk.as_ptr() as *const *const i8,
            lobby_vals: uv.as_ptr() as *const *const i8,
        };
        let t0 = Instant::now();
        let rc = (shim.post)(
            lobby,
            &creator,
            &du as *const DataUpdate as *const u8,
            ptr::null(),
            ptr::null_mut(),
        );
        check!(rc == 0, "postupdate returned S_OK (rc=0x{rc:08X})");
        check!(t0.elapsed() < Duration::from_millis(100), "postupdate returned immediately");
        // Local apply is synchronous, so the getter sees the write before the completion.
        let key = cs("post1");
        let mut val: *const i8 = ptr::null();
        check!(
            (shim.get_prop)(lobby, key.as_ptr(), &mut val) == 0 && !val.is_null(),
            "postupdate applied locally before the service reply"
        );
        let mut log2 = Vec::new();
        check!(
            shim.pump_until(&mut log2, Duration::from_millis(3000), |l| has_type(l, 8)),
            "PostUpdateCompleted (type 8) arrives"
        );
        check!(
            last_of(&log2, 8).unwrap().result == 0,
            "postupdate completion result = 0"
        );
        check!(has_type(&log2, 7), "Updated echo (type 7) follows the completion");

        // Leave completes asynchronously too.
        let rc = (shim.leave)(lobby, &creator, ptr::null_mut());
        check!(rc == 0, "leave returned S_OK");
        let mut log3 = Vec::new();
        check!(
            shim.pump_until(&mut log3, Duration::from_millis(3000), |l| has_type(l, 6)),
            "LeaveLobbyCompleted (type 6) arrives"
        );
        check!(last_of(&log3, 6).unwrap().result == 0, "leave result = 0");
    }

    // ------------------------------------------------------------------
    // 6. Service errors: create fails but the failure is delivered async.
    // ------------------------------------------------------------------
    {
        println!("\n-- scenario: create_error_is_async --");
        broker.fail_path(Some("CreateAndJoinLobby"));
        let cfg = shim.create_cfg(&[], &[], &[], &[]);
        let mut lobby: *mut c_void = ptr::null_mut();
        let t0 = Instant::now();
        let rc = (shim.create)(
            shim.handle,
            &creator,
            &cfg as *const CreateCfg as *const u8,
            ptr::null(),
            ptr::null_mut(),
            &mut lobby,
        );
        check!(rc == 0, "create against a 500 still returns S_OK (rc=0x{rc:08X})");
        check!(t0.elapsed() < Duration::from_millis(150), "create error path is async");
        let mut log = Vec::new();
        check!(
            shim.pump_until(&mut log, Duration::from_millis(3000), |l| has_type(l, 0)),
            "CreateAndJoinLobbyCompleted (type 0) delivers the failure"
        );
        let done = last_of(&log, 0).unwrap();
        check!(done.result != 0, "create completion carries the service error (0x{:08X})", done.result);
        check!(!has_type(&log, 2), "failed create emits no MemberAdded");
        broker.fail_path(None);
    }

    // ------------------------------------------------------------------
    // 7. Instance generation: a stale in-flight job cannot leak into a new instance.
    // ------------------------------------------------------------------
    {
        println!("\n-- scenario: stale_generation_is_discarded --");
        broker.set_delay(700);
        let cfg = shim.create_cfg(&[], &[], &[], &[]);
        let mut lobby: *mut c_void = ptr::null_mut();
        let rc = (shim.create)(
            shim.handle,
            &creator,
            &cfg as *const CreateCfg as *const u8,
            ptr::null(),
            ptr::null_mut(),
            &mut lobby,
        );
        check!(rc == 0, "stale create accepted");
        check!((shim.uninit)(shim.handle) == 0, "Uninitialize with a job in flight");
        let mut stale = Vec::new();
        check!(shim.init("async-test-2") == 0 && !shim.handle.is_null(), "re-initialize");
        check!(shim.set_token("player-3") == 0, "SetEntityToken after re-init");
        // Wait past the broker delay: the old job finishes and must be dropped.
        shim.pump_until(&mut stale, Duration::from_millis(1500), |_| false);
        check!(
            !has_type(&stale, 0) && !has_type(&stale, 2),
            "stale completion from the previous generation was discarded"
        );
        broker.set_delay(0);
        // The worker must still serve the new instance.
        let creator3 = EntityKey {
            id: cs("player-3").into_raw(),
            type_: cs("title_player_account").into_raw(),
        };
        let mut lobby2: *mut c_void = ptr::null_mut();
        let cfg = shim.create_cfg(&[], &[], &[], &[]);
        let rc = (shim.create)(
            shim.handle,
            &creator3,
            &cfg as *const CreateCfg as *const u8,
            ptr::null(),
            ptr::null_mut(),
            &mut lobby2,
        );
        check!(rc == 0 && !lobby2.is_null(), "new instance create accepted");
        let mut log = Vec::new();
        check!(
            shim.pump_until(&mut log, Duration::from_millis(3000), |l| has_type(l, 0)),
            "new instance completes after the stale job"
        );
    }

    let failures = FAILURES.load(Ordering::SeqCst);
    if failures == 0 {
        println!("\nALL ASYNC BROKER EXPECTATIONS PASSED");
    } else {
        println!("\n{failures} EXPECTATION(S) FAILED");
        std::process::exit(1);
    }
}

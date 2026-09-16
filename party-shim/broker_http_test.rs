//! Integration test for the broker HTTP thread, the peer-poll epoch gate and the `ensure_remote`
//! fast path — driven against the real `PartyWin.dll`.
//!
//! Build:  rustc --edition 2021 -O -o broker_http_test.exe broker_http_test.rs
//! Run:    .\broker_http_test.exe        (PartyWin.dll must sit next to the exe)
//! Modes:  .\broker_http_test.exe            full threaded flow (23 checks)
//!         .\broker_http_test.exe --loss     live retransmit/ack through the transport thread
//!         .\broker_http_test.exe --outage   silent broker: prompt calls, poll retry, recovery
//!         .\broker_http_test.exe --fallback GBFR_PARTY_FORCE_INLINE inline path
//!
//! A mock HTTP broker runs in-process and the shim is pointed at it with `GBFR_LAN_STUB` before
//! the DLL is loaded. The test also fakes the one piece of game memory `ensure_remote` consults
//! (the member-list container at `GetModuleHandleA(NULL)+0x7c52ce0`), so endpoint creation can
//! be exercised outside the game.
//!
//! What it proves, end to end:
//!   1. register/leave are fire-and-forget: `PartyCreateNewNetwork` and `PartyNetworkLeaveNetwork`
//!      return promptly while the broker delays every response by 700 ms.
//!   2. no `PartyStartProcessingStateChanges` call ever blocks on broker HTTP (`max_tick`), also
//!      with 700 ms broker delays.
//!   3. the peer poll runs off-thread but is applied on the tick: a broker member becomes a remote
//!      endpoint (type-12 state change with the right entity id).
//!   4. the `ensure_remote` fast path: with the game's member row removed, a datagram from a known
//!      peer still updates its address (observable as the forced standalone ack arriving at the
//!      datagram's source socket).
//!   5. a stale in-flight poll response for a left network is not applied to a re-joined network
//!      (epoch gate) while the fresh response is.
//!   6. the outbox burst does not block the game thread; the heartbeat reports jobs and a
//!      non-zero high-water mark.
//!   7. the cached log handle re-opens after the log file is deleted.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::os::raw::c_char;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static FAILURES: AtomicU32 = AtomicU32::new(0);

fn check(name: &str, ok: bool, detail: &str) {
    if ok {
        println!("PASS  {name}  {detail}");
    } else {
        println!("FAIL  {name}  {detail}");
        FAILURES.fetch_add(1, Ordering::SeqCst);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Mock broker
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Req {
    method: String,
    path: String,
    body: String,
}

#[derive(Clone, Debug)]
struct Member {
    entity: String,
    ip: String,
    udp_port: u16,
}

fn member(e: &str, ip: &str, p: u16) -> Member {
    Member {
        entity: e.into(),
        ip: ip.into(),
        udp_port: p,
    }
}

struct Script {
    idx: usize,
    lists: Vec<Vec<Member>>,
}

struct Broker {
    port: u16,
    reqs: Arc<Mutex<Vec<Req>>>,
    script: Arc<Mutex<Script>>,
    delay_ms: Arc<Mutex<u64>>,
    /// While true, connections are accepted and requests recorded but never answered (a broker
    /// outage). Set false to let every held handler answer.
    hang: Arc<Mutex<bool>>,
}

fn members_json(list: &[Member]) -> String {
    let objs: Vec<String> = list
        .iter()
        .map(|m| {
            format!(
                "{{\"entity_id\":\"{}\",\"ip\":\"{}\",\"udp_port\":{}}}",
                m.entity, m.ip, m.udp_port
            )
        })
        .collect();
    format!("{{\"members\":[{}]}}", objs.join(","))
}

fn handle_conn(
    mut s: TcpStream,
    reqs: Arc<Mutex<Vec<Req>>>,
    script: Arc<Mutex<Script>>,
    delay: Arc<Mutex<u64>>,
    hang: Arc<Mutex<bool>>,
) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    if buf.is_empty() {
        return; // connect-and-drop probe from lan_ip(); not an HTTP request
    }
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let clen: usize = head
        .to_ascii_lowercase()
        .split("content-length:")
        .nth(1)
        .and_then(|v| v.split(['\r', '\n']).next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end.min(buf.len())..].to_vec();
    while body.len() < clen {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    body.truncate(clen);
    let body = String::from_utf8_lossy(&body).into_owned();
    reqs.lock().unwrap().push(Req {
        method: method.clone(),
        path: path.clone(),
        body,
    });
    // Pick the scripted peer list at request time: the artificial delay models broker latency and
    // must not change which response a request gets.
    let resp_body = if path.starts_with("/party/peers") {
        let list = {
            let mut g = script.lock().unwrap();
            let idx = if g.lists.is_empty() {
                0
            } else {
                g.idx.min(g.lists.len() - 1)
            };
            let l = g.lists.get(idx).cloned().unwrap_or_default();
            g.idx = g.idx.saturating_add(1);
            l
        };
        members_json(&list)
    } else {
        "{}".to_string()
    };
    // An outage is modelled by accepting the connection and never answering; the client's own
    // 3 s read timeout ends each attempt. `set_hang(false)` lets every held handler answer.
    while *hang.lock().unwrap() {
        std::thread::sleep(Duration::from_millis(20));
    }
    let d = *delay.lock().unwrap();
    if d > 0 {
        std::thread::sleep(Duration::from_millis(d));
    }
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        resp_body.len(),
        resp_body
    );
    let _ = s.write_all(resp.as_bytes());
    let _ = s.flush();
}

impl Broker {
    fn start() -> Broker {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock broker bind");
        let port = listener.local_addr().unwrap().port();
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(Script {
            idx: 0,
            lists: vec![vec![member("guest", "192.0.2.10", 27016)]],
        }));
        let delay_ms = Arc::new(Mutex::new(0u64));
        let hang = Arc::new(Mutex::new(false));
        {
            let (reqs, script, delay_ms, hang) =
                (reqs.clone(), script.clone(), delay_ms.clone(), hang.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(s) = stream else { continue };
                    let (reqs, script, delay_ms, hang) =
                        (reqs.clone(), script.clone(), delay_ms.clone(), hang.clone());
                    std::thread::spawn(move || handle_conn(s, reqs, script, delay_ms, hang));
                }
            });
        }
        Broker {
            port,
            reqs,
            script,
            delay_ms,
            hang,
        }
    }

    fn set_delay(&self, ms: u64) {
        *self.delay_ms.lock().unwrap() = ms;
    }

    fn set_hang(&self, v: bool) {
        *self.hang.lock().unwrap() = v;
    }

    fn set_script(&self, lists: Vec<Vec<Member>>) {
        let mut g = self.script.lock().unwrap();
        g.lists = lists;
        g.idx = 0;
    }

    fn wait_req(&self, pred: impl Fn(&Req) -> bool, timeout: Duration) -> Option<Req> {
        self.wait_req_from(0, pred, timeout)
    }

    /// Wait for a request at index `>= from` (used to observe a request made after a marker).
    fn wait_req_from(
        &self,
        from: usize,
        pred: impl Fn(&Req) -> bool,
        timeout: Duration,
    ) -> Option<Req> {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if let Some(r) = self
                .reqs
                .lock()
                .unwrap()
                .iter()
                .skip(from)
                .find(|r| pred(r))
            {
                return Some(r.clone());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fake game member list (the container ensure_remote consults)
// ─────────────────────────────────────────────────────────────────────────────

const MEMBER_LIST_RVA: usize = 0x7c52ce0;
const MEM_COMMIT_RESERVE: u32 = 0x3000;
const PAGE_READWRITE: u32 = 0x04;

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleA(name: *const u8) -> *mut c_void;
    fn VirtualAlloc(addr: *mut c_void, size: usize, fl_alloc: u32, fl_protect: u32) -> *mut c_void;
}

/// `DAT_147c52ce0` in the game: a pointer to a container with begin/end at +0x50/+0x58 of
/// 0x20-stride rows; row+0x18 -> inner, inner+0x18 -> work, work+0x38 -> std::string entity.
/// This reproduces exactly the fields `member_list_has_entity` walks.
struct FakeMembers {
    container: *mut u8,
    elems: *mut u8,
    inner: *mut u8,
    cap: usize,
}

impl FakeMembers {
    unsafe fn install(cap: usize) -> FakeMembers {
        let base = GetModuleHandleA(null()) as usize;
        assert!(base != 0, "GetModuleHandleA(NULL) failed");
        let slot_addr = base + MEMBER_LIST_RVA;
        // VirtualAlloc rounds a fixed address down to a 64 KiB boundary, so reserve from the
        // rounded base and commit enough to cover the slot itself.
        let page64 = slot_addr & !0xffff;
        let span = (slot_addr - page64) + 0x1000;
        let global = VirtualAlloc(
            page64 as *mut c_void,
            span,
            MEM_COMMIT_RESERVE,
            PAGE_READWRITE,
        );
        assert!(!global.is_null(), "VirtualAlloc member-list slot failed");
        let container = VirtualAlloc(null_mut(), 0x1000, MEM_COMMIT_RESERVE, PAGE_READWRITE) as *mut u8;
        let elems = VirtualAlloc(null_mut(), 0x1000, MEM_COMMIT_RESERVE, PAGE_READWRITE) as *mut u8;
        let inner = VirtualAlloc(null_mut(), 0x2000, MEM_COMMIT_RESERVE, PAGE_READWRITE) as *mut u8;
        assert!(!container.is_null() && !elems.is_null() && !inner.is_null());
        *(slot_addr as *mut usize) = container as usize;
        *(container.add(0x50) as *mut usize) = elems as usize;
        *(container.add(0x58) as *mut usize) = elems as usize;
        let f = FakeMembers {
            container,
            elems,
            inner,
            cap,
        };
        f.set(&[]);
        f
    }

    /// Replace the member rows. An empty slice makes `member_list_has_entity` return false for
    /// every entity, which is the state the fast-path test depends on.
    unsafe fn set(&self, ids: &[&str]) {
        assert!(ids.len() <= self.cap, "FakeMembers::set over capacity");
        for i in 0..self.cap {
            std::ptr::write_bytes(self.elems.add(i * 0x20), 0, 0x20);
            std::ptr::write_bytes(self.inner.add(i * 0x200), 0, 0x200);
        }
        for (i, id) in ids.iter().enumerate() {
            let inner_i = self.inner.add(i * 0x200);
            let work = inner_i.add(0x100);
            *(inner_i.add(0x18) as *mut usize) = work as usize;
            let so = work.add(0x38); // std::string object (SSO)
            *(so.add(0x10) as *mut usize) = id.len();
            *(so.add(0x18) as *mut usize) = 15; // SSO capacity: bytes live inline at so+0
            std::ptr::copy_nonoverlapping(id.as_ptr(), so, id.len());
            let e = self.elems.add(i * 0x20);
            *(e.add(0x18) as *mut usize) = inner_i as usize;
        }
        *(self.container.add(0x58) as *mut usize) = self.elems as usize + ids.len() * 0x20;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PartyWin.dll exports
// ─────────────────────────────────────────────────────────────────────────────

type H = *mut c_void;
type InitFn = unsafe extern "C" fn(*const u8, *mut H) -> u32;
type MkUserFn = unsafe extern "C" fn(H, *const u8, *const u8, *mut H) -> u32;
type MkNetFn = unsafe extern "C" fn(
    H,
    H,
    *const c_void,
    u32,
    *const c_void,
    *const c_void,
    *mut c_void,
    *mut u8,
    *mut u8,
) -> u32;
type ConnectFn = unsafe extern "C" fn(H, *const u8, *mut c_void, *mut H) -> u32;
type MkEpFn = unsafe extern "C" fn(H, H, u32, *const *const u8, *const c_void, *mut c_void, *mut H) -> u32;
type EpEntityFn = unsafe extern "C" fn(H, *mut *const c_char) -> u32;
type SendFn =
    unsafe extern "C" fn(H, u32, *const *mut c_void, u32, *const c_void, u32, *const u8, *mut c_void) -> u32;
type LeaveFn = unsafe extern "C" fn(H, *mut c_void) -> u32;
type CleanupFn = unsafe extern "C" fn(H) -> u32;
type StartFn = unsafe extern "C" fn(H, *mut u32, *mut *mut *mut u8) -> u32;
type FinishFn = unsafe extern "C" fn(H, u32, *mut *mut u8) -> u32;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryA(name: *const u8) -> *mut c_void;
    fn GetProcAddress(h: *mut c_void, name: *const u8) -> *mut c_void;
}

unsafe fn sym(h: H, name: &[u8]) -> *mut c_void {
    let p = GetProcAddress(h, name.as_ptr());
    assert!(!p.is_null(), "missing export {}", String::from_utf8_lossy(name));
    p
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
}

// ─────────────────────────────────────────────────────────────────────────────
// Pump: the game's Start/Finish loop
// ─────────────────────────────────────────────────────────────────────────────

struct Pump {
    handle: H,
    start: StartFn,
    finish: FinishFn,
    ep_entity: EpEntityFn,
    max_tick: Duration,
    ticks: u64,
    types_seen: Vec<u32>,
    remote_eps: Vec<(String, H)>,
}

impl Pump {
    fn tick(&mut self) {
        let t0 = Instant::now();
        let mut count = 0u32;
        let mut arr: *mut *mut u8 = null_mut();
        unsafe { (self.start)(self.handle, &mut count, &mut arr) };
        self.max_tick = self.max_tick.max(t0.elapsed());
        self.ticks += 1;
        if count > 0 && !arr.is_null() {
            unsafe {
                for i in 0..count as usize {
                    let p = *arr.add(i);
                    if p.is_null() {
                        continue;
                    }
                    let ty = *(p as *const u32);
                    self.types_seen.push(ty);
                    if ty == 12 {
                        let ep = *(p.add(0x10) as *const H);
                        if !ep.is_null() {
                            let mut s: *const c_char = null();
                            if (self.ep_entity)(ep, &mut s) == 0 {
                                let eid = cstr(s);
                                if !self.remote_eps.iter().any(|(e, _)| *e == eid) {
                                    self.remote_eps.push((eid, ep));
                                }
                            }
                        }
                    }
                }
                (self.finish)(self.handle, count, arr);
            }
        }
    }

    fn pump_for(&mut self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            self.tick();
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    fn pump_until(&mut self, mut pred: impl FnMut(&Pump) -> bool, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        loop {
            self.tick();
            if pred(self) {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
#[allow(dead_code)]
struct DataBuffer {
    ptr: *const u8,
    size: u32,
    _pad: u32,
}

fn wire_packet(entity: &str, options: u32, seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0u8; 60 + payload.len()];
    p[..4].copy_from_slice(b"GBFR");
    p[4] = 2; // KIND_MSG
    p[5] = 3; // HDR_VERSION
    p[8..8 + 3].copy_from_slice(b"TST");
    let e = entity.len().min(20);
    p[24..24 + e].copy_from_slice(&entity.as_bytes()[..e]);
    p[44..48].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    p[48..52].copy_from_slice(&options.to_le_bytes());
    p[52..56].copy_from_slice(&seq.to_le_bytes());
    p[60..].copy_from_slice(payload);
    p
}

/// A standalone cumulative ack (KIND_ACK) for `entity`. The v3 header's ack lives at +56.
fn wire_ack(entity: &str, ack: u32) -> Vec<u8> {
    let mut p = wire_packet(entity, 0, 0, &[]);
    p[4] = 3;
    p[56..60].copy_from_slice(&ack.to_le_bytes());
    p
}

fn shim_log_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent().unwrap().join("party_shim.log")
}

fn log_text() -> String {
    std::fs::read_to_string(shim_log_path()).unwrap_or_default()
}

/// Wait until the shim log contains a line matching `pred`.
fn wait_log_line(pred: impl Fn(&str) -> bool, timeout: Duration) -> Option<String> {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        for line in log_text().lines().rev() {
            if pred(line) {
                return Some(line.to_string());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn field_u64(line: &str, key: &str) -> Option<u64> {
    let pat = format!("{key}=");
    let rest = line.split(&pat).nth(1)?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

// ─────────────────────────────────────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────────────────────────────────────

/// `--fallback`: run the same end-to-end flow with `GBFR_PARTY_FORCE_INLINE=1`, i.e. exactly the
/// degraded state a thread-spawn failure produces. Registration/poll, peer application, inbound
/// receive and outbound send must all still work on the tick-driven inline paths.
fn run_fallback_mode() {
    println!("broker_http_test --fallback: inline (no worker threads) transport/broker paths");
    let broker = Broker::start();
    broker.set_delay(0);
    std::env::set_var("GBFR_LAN_STUB", format!("127.0.0.1:{}", broker.port));
    let fake = unsafe { FakeMembers::install(8) };
    unsafe { fake.set(&["guest"]) };
    let _ = std::fs::remove_file(shim_log_path());

    unsafe {
        let dll = LoadLibraryA(b"PartyWin.dll\0".as_ptr());
        assert!(!dll.is_null(), "LoadLibraryA(PartyWin.dll) failed");
        let init: InitFn = std::mem::transmute(sym(dll, b"PartyInitialize\0"));
        let mk_user: MkUserFn = std::mem::transmute(sym(dll, b"PartyCreateLocalUser\0"));
        let mk_net: MkNetFn = std::mem::transmute(sym(dll, b"PartyCreateNewNetwork\0"));
        let connect: ConnectFn = std::mem::transmute(sym(dll, b"PartyConnectToNetwork\0"));
        let mk_ep: MkEpFn = std::mem::transmute(sym(dll, b"PartyNetworkCreateEndpoint\0"));
        let ep_entity: EpEntityFn = std::mem::transmute(sym(dll, b"PartyEndpointGetEntityId\0"));
        let send: SendFn = std::mem::transmute(sym(dll, b"PartyEndpointSendMessage\0"));
        let cleanup: CleanupFn = std::mem::transmute(sym(dll, b"PartyCleanup\0"));
        let start: StartFn = std::mem::transmute(sym(dll, b"PartyStartProcessingStateChanges\0"));
        let finish: FinishFn = std::mem::transmute(sym(dll, b"PartyFinishProcessingStateChanges\0"));

        let mut handle: H = null_mut();
        check(
            "fallback PartyInitialize",
            init(b"1AC1AD\0".as_ptr(), &mut handle) == 0 && !handle.is_null(),
            "",
        );
        let mut user: H = null_mut();
        check(
            "fallback PartyCreateLocalUser",
            mk_user(handle, b"host\0".as_ptr(), null(), &mut user) == 0 && !user.is_null(),
            "",
        );
        let cfg = [0u8; 16];
        let mut desc = [0u8; 357];
        let r = mk_net(
            handle,
            user,
            cfg.as_ptr() as *const c_void,
            1,
            null(),
            null(),
            null_mut(),
            desc.as_mut_ptr(),
            null_mut(),
        );
        check("fallback CreateNewNetwork", r == 0, &format!("r={r:#x}"));
        let mut net: H = null_mut();
        check(
            "fallback ConnectToNetwork",
            connect(handle, desc.as_ptr(), null_mut(), &mut net) == 0 && !net.is_null(),
            "",
        );
        let mut local_ep: H = null_mut();
        check(
            "fallback CreateEndpoint",
            mk_ep(net, user, 0, null(), null(), null_mut(), &mut local_ep) == 0 && !local_ep.is_null(),
            "",
        );

        let mut pump = Pump {
            handle,
            start,
            finish,
            ep_entity,
            max_tick: Duration::ZERO,
            ticks: 0,
            types_seen: Vec::new(),
            remote_eps: Vec::new(),
        };
        pump.pump_until(|p| p.types_seen.contains(&3) && p.types_seen.contains(&10), Duration::from_secs(3));
        let got_guest = pump.pump_until(
            |p| p.remote_eps.iter().any(|(e, _)| e == "guest"),
            Duration::from_secs(6),
        );
        check("fallback inline poll becomes a remote endpoint", got_guest, "");

        // Point the remote endpoint at a socket we can observe: send a datagram from "guest",
        // which the inline receive path must adopt, then give the tick a moment to process it.
        let udp = UdpSocket::bind("127.0.0.1:0").expect("fallback udp bind");
        udp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let shim_port = u16::from_le_bytes([desc[57], desc[58]]);
        let pkt = wire_packet("guest", 0x3, 1, b"op5 sub3 fallback");
        udp.send_to(&pkt, ("127.0.0.1", shim_port)).expect("fallback send datagram");
        pump.pump_for(Duration::from_millis(100));

        // One Party send must reach that socket synchronously (inline send_udp_all).
        let payload = [0x42u8; 32];
        let b = DataBuffer {
            ptr: payload.as_ptr(),
            size: payload.len() as u32,
            _pad: 0,
        };
        let t0 = Instant::now();
        let sr = send(local_ep, 1, null(), 0x2, null(), 1, &b as *const DataBuffer as *const u8, null_mut());
        let send_ms = t0.elapsed().as_millis();
        check("fallback send call succeeds", sr == 0, &format!("r={sr:#x} took={send_ms}ms"));

        let mut got = false;
        let mut rbuf = [0u8; 512];
        udp.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            pump.tick();
            match udp.recv_from(&mut rbuf) {
                Ok((n, _)) if n >= 60 && &rbuf[..4] == b"GBFR" && rbuf[4] == 2 => {
                    got = true;
                    break;
                }
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        check("fallback inline send reaches the peer socket", got, "");

        let log = log_text();
        check(
            "no worker threads started in fallback mode",
            !log.contains("transport thread started") && !log.contains("broker thread started"),
            "",
        );
        check(
            "fallback mode logged the inline decisions",
            log.contains("transport disabled (GBFR_PARTY_FORCE_INLINE)")
                && log.contains("broker disabled (GBFR_PARTY_FORCE_INLINE)"),
            "",
        );
        check(
            "fallback broker still receives /party/join",
            broker
                .wait_req(|r| r.path == "/party/join", Duration::from_secs(2))
                .is_some(),
            "",
        );
        check(
            "fallback tick stays responsive",
            pump.max_tick < Duration::from_millis(100),
            &format!("max_tick={}ms ticks={}", pump.max_tick.as_millis(), pump.ticks),
        );
        let _ = cleanup(handle);
    }
}

fn finish_mode(name: &str) {
    let failures = FAILURES.load(Ordering::SeqCst);
    if failures > 0 {
        println!("{failures} FAILURE(S)");
        std::process::exit(1);
    }
    println!("all {name} checks passed");
}

// ─────────────────────────────────────────────────────────────────────────────
// Loss / outage modes
// ─────────────────────────────────────────────────────────────────────────────

/// The DLL's export table, loaded once per mode.
struct ShimFns {
    init: InitFn,
    mk_user: MkUserFn,
    mk_net: MkNetFn,
    connect: ConnectFn,
    mk_ep: MkEpFn,
    ep_entity: EpEntityFn,
    send: SendFn,
    leave: LeaveFn,
    cleanup: CleanupFn,
    start: StartFn,
    finish: FinishFn,
}

unsafe fn load_shim_fns() -> ShimFns {
    let dll = LoadLibraryA(b"PartyWin.dll\0".as_ptr());
    assert!(!dll.is_null(), "LoadLibraryA(PartyWin.dll) failed");
    ShimFns {
        init: std::mem::transmute(sym(dll, b"PartyInitialize\0")),
        mk_user: std::mem::transmute(sym(dll, b"PartyCreateLocalUser\0")),
        mk_net: std::mem::transmute(sym(dll, b"PartyCreateNewNetwork\0")),
        connect: std::mem::transmute(sym(dll, b"PartyConnectToNetwork\0")),
        mk_ep: std::mem::transmute(sym(dll, b"PartyNetworkCreateEndpoint\0")),
        ep_entity: std::mem::transmute(sym(dll, b"PartyEndpointGetEntityId\0")),
        send: std::mem::transmute(sym(dll, b"PartyEndpointSendMessage\0")),
        leave: std::mem::transmute(sym(dll, b"PartyNetworkLeaveNetwork\0")),
        cleanup: std::mem::transmute(sym(dll, b"PartyCleanup\0")),
        start: std::mem::transmute(sym(dll, b"PartyStartProcessingStateChanges\0")),
        finish: std::mem::transmute(sym(dll, b"PartyFinishProcessingStateChanges\0")),
    }
}

struct Booted {
    f: ShimFns,
    handle: H,
    user: H,
    local_ep: H,
    desc: [u8; 357],
    pump: Pump,
}

/// Common prologue for the loss/outage modes: fresh fake member list and log, DLL load, handle,
/// local user, one network + endpoint, and a pump that has delivered types 3 and 10.
unsafe fn boot_shim(label: &str, members: &[&str]) -> Booted {
    let fake = FakeMembers::install(8);
    fake.set(members);
    let _ = std::fs::remove_file(shim_log_path());
    let f = load_shim_fns();
    let mut handle: H = null_mut();
    check(
        &format!("{label} PartyInitialize"),
        (f.init)(b"1AC1AD\0".as_ptr(), &mut handle) == 0 && !handle.is_null(),
        "",
    );
    let mut user: H = null_mut();
    check(
        &format!("{label} PartyCreateLocalUser"),
        (f.mk_user)(handle, b"host\0".as_ptr(), null(), &mut user) == 0 && !user.is_null(),
        "",
    );
    let cfg = [0u8; 16];
    let mut desc = [0u8; 357];
    let r = (f.mk_net)(
        handle,
        user,
        cfg.as_ptr() as *const c_void,
        1,
        null(),
        null(),
        null_mut(),
        desc.as_mut_ptr(),
        null_mut(),
    );
    check(&format!("{label} CreateNewNetwork"), r == 0, &format!("r={r:#x}"));
    let mut net: H = null_mut();
    check(
        &format!("{label} ConnectToNetwork"),
        (f.connect)(handle, desc.as_ptr(), null_mut(), &mut net) == 0 && !net.is_null(),
        "",
    );
    let mut local_ep: H = null_mut();
    check(
        &format!("{label} CreateEndpoint"),
        (f.mk_ep)(net, user, 0, null(), null(), null_mut(), &mut local_ep) == 0 && !local_ep.is_null(),
        "",
    );
    let mut pump = Pump {
        handle,
        start: f.start,
        finish: f.finish,
        ep_entity: f.ep_entity,
        max_tick: Duration::ZERO,
        ticks: 0,
        types_seen: Vec::new(),
        remote_eps: Vec::new(),
    };
    pump.pump_until(
        |p| p.types_seen.contains(&3) && p.types_seen.contains(&10),
        Duration::from_secs(3),
    );
    Booted {
        f,
        handle,
        user,
        local_ep,
        desc,
        pump,
    }
}

/// `--loss`: one guaranteed message, never acked, must be retransmitted by the live transport
/// thread with the same sequence number and the intended RTO cadence; a later ack must clear it.
fn run_loss_mode() {
    println!("broker_http_test --loss: live retransmit through the transport thread");
    let broker = Broker::start();
    broker.set_delay(0);
    // The peer is advertised once, then the list is empty: the endpoint is created from the first
    // poll and nothing later rewrites the address the test socket adopts below.
    broker.set_script(vec![vec![member("guest", "192.0.2.10", 27016)], vec![]]);
    std::env::set_var("GBFR_LAN_STUB", format!("127.0.0.1:{}", broker.port));
    let mut b = unsafe { boot_shim("loss", &["guest"]) };

    let got_guest = b.pump.pump_until(
        |p| p.remote_eps.iter().any(|(e, _)| e == "guest"),
        Duration::from_secs(6),
    );
    check("loss peer endpoint exists", got_guest, "");

    // Adopt this socket as the remote endpoint's address. The shim answers the guaranteed hello
    // with a standalone ack; drain whatever is already queued before the measurement.
    let udp = UdpSocket::bind("127.0.0.1:0").expect("loss udp bind");
    udp.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
    let shim_port = u16::from_le_bytes([b.desc[57], b.desc[58]]);
    let hello = wire_packet("guest", 0x3, 1, b"op5 sub3 loss");
    udp.send_to(&hello, ("127.0.0.1", shim_port)).expect("loss hello");
    b.pump.pump_for(Duration::from_millis(100));
    let mut buf = [0u8; 2048];
    while udp.recv_from(&mut buf).is_ok() {}

    // Guaranteed + sequential; the first copy is never acked.
    let payload = [0x55u8; 24];
    let db = DataBuffer {
        ptr: payload.as_ptr(),
        size: payload.len() as u32,
        _pad: 0,
    };
    let r = unsafe {
        (b.f.send)(
            b.local_ep,
            1,
            null(),
            0x3,
            null(),
            1,
            &db as *const DataBuffer as *const u8,
            null_mut(),
        )
    };
    check("loss send call succeeds", r == 0, &format!("r={r:#x}"));

    let mut copies: Vec<Instant> = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        b.pump.tick();
        match udp.recv_from(&mut buf) {
            Ok((n, _)) if n >= 60 && &buf[..4] == b"GBFR" && buf[5] == 3 && buf[4] == 2 => {
                let seq = u32::from_le_bytes(buf[52..56].try_into().unwrap());
                if seq == 1 {
                    copies.push(Instant::now());
                }
            }
            Ok(_) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            // A Windows UDP socket can surface a recoverable ICMP-derived error (e.g.
            // ConnectionReset) once; keep reading rather than ending the measurement.
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    check(
        "the guaranteed message is transmitted",
        !copies.is_empty(),
        &format!("copies={}", copies.len()),
    );
    check(
        "the un-acked message is retransmitted with the same sequence number",
        copies.len() >= 2,
        &format!("copies={}", copies.len()),
    );
    check(
        "retransmits ride the RTO cadence",
        copies.windows(2).all(|w| w[1] - w[0] >= Duration::from_millis(45)),
        &format!(
            "gaps_ms={:?}",
            copies
                .windows(2)
                .map(|w| (w[1] - w[0]).as_millis())
                .collect::<Vec<_>>()
        ),
    );

    // Cumulative ack for seq 1 must clear the sender's pending entry. `bump_msg_stats` only
    // emits on its 5 s timer or every 50 events, and an ack logs no payload, so feed it tiny
    // best-effort sends until a stats line carries the acked counter.
    let ack = wire_ack("guest", 1);
    udp.send_to(&ack, ("127.0.0.1", shim_port)).expect("loss ack");
    let mut acked = None;
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end {
        let probe = [0u8; 4];
        let db = DataBuffer {
            ptr: probe.as_ptr(),
            size: probe.len() as u32,
            _pad: 0,
        };
        let _ = unsafe {
            (b.f.send)(
                b.local_ep,
                1,
                null(),
                0x0,
                null(),
                1,
                &db as *const DataBuffer as *const u8,
                null_mut(),
            )
        };
        b.pump.tick();
        if let Some(l) = wait_log_line(
            |l| l.contains("guar-seq(") && l.contains("acked=1"),
            Duration::from_millis(50),
        ) {
            acked = Some(l);
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    check(
        "the ack is processed (reliable stats show acked=1)",
        acked.is_some(),
        &format!("{}", acked.unwrap_or_default()),
    );

    // After that ack the message must be gone; allow one already-in-flight straggler.
    let mut after = 0usize;
    let end = Instant::now() + Duration::from_millis(400);
    while Instant::now() < end {
        b.pump.tick();
        if let Ok((n, _)) = udp.recv_from(&mut buf) {
            if n >= 60 && &buf[..4] == b"GBFR" && buf[4] == 2 {
                if u32::from_le_bytes(buf[52..56].try_into().unwrap()) == 1 {
                    after += 1;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    check(
        "no further retransmits after the ack",
        after <= 1,
        &format!("late_copies={after}"),
    );

    let _ = unsafe { (b.f.cleanup)(b.handle) };
}

/// `--outage`: the broker accepts connections but never answers. Game-facing calls must stay
/// prompt, polls must keep retrying (a failed poll must clear `poll_inflight`), and a fresh poll
/// must apply once the broker recovers.
fn run_outage_mode() {
    println!("broker_http_test --outage: silent broker, then recovery");
    let broker = Broker::start();
    broker.set_delay(0);
    broker.set_script(vec![vec![]]); // no peers before the outage
    std::env::set_var("GBFR_LAN_STUB", format!("127.0.0.1:{}", broker.port));
    let mut b = unsafe { boot_shim("outage", &["late_peer"]) };
    b.pump.pump_for(Duration::from_millis(300));

    broker.set_hang(true);
    let mark = broker.reqs.lock().unwrap().len();

    // Pump through the outage: each poll attempt costs the client a 3 s read timeout, and the
    // next attempt may only start once the failed one cleared `poll_inflight`.
    let end = Instant::now() + Duration::from_millis(7500);
    while Instant::now() < end {
        b.pump.tick();
        std::thread::sleep(Duration::from_millis(4));
    }
    check(
        "tick stays responsive during the outage",
        b.pump.max_tick < Duration::from_millis(100),
        &format!("max_tick={}ms", b.pump.max_tick.as_millis()),
    );
    let attempts = broker
        .reqs
        .lock()
        .unwrap()
        .iter()
        .skip(mark)
        .filter(|r| r.method == "GET" && r.path.starts_with("/party/peers"))
        .count();
    check(
        "polls keep retrying while the broker is silent (inflight clears on failure)",
        attempts >= 2,
        &format!("attempts={attempts}"),
    );

    // Game-facing calls must not wait on the broker thread (the queue absorbs them).
    let cfg = [0u8; 16];
    let mut desc2 = [0u8; 357];
    let t0 = Instant::now();
    let r = unsafe {
        (b.f.mk_net)(
            b.handle,
            b.user,
            cfg.as_ptr() as *const c_void,
            1,
            null(),
            null(),
            null_mut(),
            desc2.as_mut_ptr(),
            null_mut(),
        )
    };
    let create_ms = t0.elapsed().as_millis();
    check(
        "outage CreateNewNetwork returns promptly",
        r == 0 && create_ms < 250,
        &format!("r={r:#x} took={create_ms}ms"),
    );
    let mut net2: H = null_mut();
    let c2 = unsafe { (b.f.connect)(b.handle, desc2.as_ptr(), null_mut(), &mut net2) };
    check("outage ConnectToNetwork returns promptly", c2 == 0 && !net2.is_null(), "");
    let t0 = Instant::now();
    let lv = unsafe { (b.f.leave)(net2, null_mut()) };
    let leave_ms = t0.elapsed().as_millis();
    check(
        "outage LeaveNetwork returns promptly",
        lv == 0 && leave_ms < 250,
        &format!("took={leave_ms}ms"),
    );

    // Recover with a member the shim has never seen; the next poll must apply it.
    broker.set_script(vec![vec![member("late_peer", "192.0.2.20", 27020)]]);
    broker.set_hang(false);
    let fresh = b.pump.pump_until(
        |p| p.remote_eps.iter().any(|(e, _)| e == "late_peer"),
        Duration::from_secs(12),
    );
    check(
        "a fresh poll applies after the broker recovers",
        fresh,
        &format!(
            "remote_eps={:?}",
            b.pump
                .remote_eps
                .iter()
                .map(|(e, _)| e.clone())
                .collect::<Vec<_>>()
        ),
    );
    let _ = unsafe { (b.f.cleanup)(b.handle) };
}

fn main() {
    if std::env::args().any(|a| a == "--fallback") {
        std::env::set_var("GBFR_PARTY_FORCE_INLINE", "1");
        run_fallback_mode();
        finish_mode("fallback");
        return;
    }
    if std::env::args().any(|a| a == "--loss") {
        run_loss_mode();
        finish_mode("loss/retransmit");
        return;
    }
    if std::env::args().any(|a| a == "--outage") {
        run_outage_mode();
        finish_mode("broker-outage");
        return;
    }
    println!("broker_http_test: driving the real PartyWin.dll against an in-process mock broker");

    let broker = Broker::start();
    broker.set_delay(700);
    std::env::set_var("GBFR_LAN_STUB", format!("127.0.0.1:{}", broker.port));

    let fake = unsafe { FakeMembers::install(8) };
    unsafe { fake.set(&["guest"]) };

    // Fresh log for this run.
    let _ = std::fs::remove_file(shim_log_path());

    unsafe {
        let dll = LoadLibraryA(b"PartyWin.dll\0".as_ptr());
        assert!(!dll.is_null(), "LoadLibraryA(PartyWin.dll) failed");

        let init: InitFn = std::mem::transmute(sym(dll, b"PartyInitialize\0"));
        let mk_user: MkUserFn = std::mem::transmute(sym(dll, b"PartyCreateLocalUser\0"));
        let mk_net: MkNetFn = std::mem::transmute(sym(dll, b"PartyCreateNewNetwork\0"));
        let connect: ConnectFn = std::mem::transmute(sym(dll, b"PartyConnectToNetwork\0"));
        let mk_ep: MkEpFn = std::mem::transmute(sym(dll, b"PartyNetworkCreateEndpoint\0"));
        let ep_entity: EpEntityFn = std::mem::transmute(sym(dll, b"PartyEndpointGetEntityId\0"));
        let send: SendFn = std::mem::transmute(sym(dll, b"PartyEndpointSendMessage\0"));
        let leave: LeaveFn = std::mem::transmute(sym(dll, b"PartyNetworkLeaveNetwork\0"));
        let cleanup: CleanupFn = std::mem::transmute(sym(dll, b"PartyCleanup\0"));
        let start: StartFn = std::mem::transmute(sym(dll, b"PartyStartProcessingStateChanges\0"));
        let finish: FinishFn = std::mem::transmute(sym(dll, b"PartyFinishProcessingStateChanges\0"));

        let mut handle: H = null_mut();
        check(
            "PartyInitialize",
            init(b"1AC1AD\0".as_ptr(), &mut handle) == 0 && !handle.is_null(),
            "",
        );
        let mut user: H = null_mut();
        check(
            "PartyCreateLocalUser",
            mk_user(handle, b"host\0".as_ptr(), null(), &mut user) == 0 && !user.is_null(),
            "",
        );

        // ── 1. fire-and-forget register: the delayed /party/join must not stall this call ──
        let cfg = [0u8; 16];
        let mut desc = [0u8; 357];
        let t0 = Instant::now();
        let r = mk_net(
            handle,
            user,
            cfg.as_ptr() as *const c_void,
            1,
            null(),
            null(),
            null_mut(),
            desc.as_mut_ptr(),
            null_mut(),
        );
        let create_ms = t0.elapsed().as_millis();
        check(
            "CreateNewNetwork returns with /party/join delayed 700ms",
            r == 0 && create_ms < 250,
            &format!("r={r:#x} took={create_ms}ms"),
        );

        let mut net: H = null_mut();
        check(
            "ConnectToNetwork",
            connect(handle, desc.as_ptr(), null_mut(), &mut net) == 0 && !net.is_null(),
            "",
        );
        let mut local_ep: H = null_mut();
        check(
            "CreateEndpoint",
            mk_ep(net, user, 0, null(), null(), null_mut(), &mut local_ep) == 0 && !local_ep.is_null(),
            "",
        );

        let mut pump = Pump {
            handle,
            start,
            finish,
            ep_entity,
            max_tick: Duration::ZERO,
            ticks: 0,
            types_seen: Vec::new(),
            remote_eps: Vec::new(),
        };
        pump.pump_until(|p| p.types_seen.contains(&3), Duration::from_secs(3));
        pump.pump_until(|p| p.types_seen.contains(&10), Duration::from_secs(3));
        check(
            "type 3 (connect) and type 10 (local endpoint) delivered",
            pump.types_seen.contains(&3) && pump.types_seen.contains(&10),
            &format!("types={:?}", pump.types_seen),
        );

        let join = broker.wait_req(
            |r| r.method == "POST" && r.path == "/party/join",
            Duration::from_secs(3),
        );
        check(
            "broker thread sends /party/join off-thread",
            join.as_ref()
                .map(|r| r.body.contains("\"entity_id\":\"host\""))
                .unwrap_or(false),
            &format!("{:?}", join.map(|r| r.body)),
        );

        // ── 2. peer poll off-thread, applied on the tick ──
        let got_guest = pump.pump_until(
            |p| p.remote_eps.iter().any(|(e, _)| e == "guest"),
            Duration::from_secs(6),
        );
        if !got_guest {
            println!("DEBUG broker GET reqs:");
            for r in broker.reqs.lock().unwrap().iter().filter(|r| r.path.starts_with("/party/peers")) {
                println!("DEBUG   {} {}", r.method, r.path);
            }
            println!("DEBUG shim log lines of interest:");
            for l in log_text().lines().filter(|l| {
                l.contains("guest") || l.contains("ensure_remote") || l.contains("EndpointCreated")
                    || l.contains("party register") || l.contains("poll")
            }).take(60) {
                println!("DEBUG LOG {l}");
            }
            println!("DEBUG types={:?} remote_eps={:?}", pump.types_seen, pump.remote_eps.len());
        }
        check(
            "delayed /party/peers result becomes a remote endpoint on the tick",
            got_guest,
            &format!("remote_eps={:?}", pump.remote_eps.iter().map(|(e, _)| e.clone()).collect::<Vec<_>>()),
        );
        check(
            "tick never blocks on broker HTTP (700ms delays)",
            pump.max_tick < Duration::from_millis(100),
            &format!(
                "max_tick={}ms ticks={}",
                pump.max_tick.as_millis(),
                pump.ticks
            ),
        );
        check(
            "shim log shows the broker thread started",
            log_text().contains("broker thread started tid="),
            "",
        );

        // ── 3. ensure_remote fast path with the game's member row gone ──
        fake.set(&[]); // the exe's member list no longer has "guest"
        let udp = UdpSocket::bind("127.0.0.1:0").expect("test udp bind");
        udp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let shim_port = u16::from_le_bytes([desc[57], desc[58]]);
        check("descriptor carries the UDP port", shim_port != 0, &format!("port={shim_port}"));
        let pkt = wire_packet("guest", 0x3, 1, b"op5 sub3 fastpath");
        udp.send_to(&pkt, ("127.0.0.1", shim_port)).expect("send datagram");
        udp.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
        let mut rbuf = [0u8; 512];
        let mut ack: Result<(usize, std::net::SocketAddr), std::io::Error> =
            Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "not yet"));
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            // Keep the tick running while we wait; the transport thread receives the datagram
            // and sends the forced ack, and it must reach this socket.
            pump.tick();
            match udp.recv_from(&mut rbuf) {
                Ok((n, from)) => {
                    if n >= 60
                        && &rbuf[..4] == b"GBFR"
                        && rbuf[4] == 3
                        && rbuf[5] == 3
                        && u32::from_le_bytes(rbuf[56..60].try_into().unwrap()) >= 1
                    {
                        ack = Ok((n, from));
                        break;
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    ack = Err(e);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        check(
            "known peer updated its address with no member row (forced ack reaches the sender)",
            ack.is_ok(),
            &format!("ack={:?}", ack.map(|(n, a)| (n, a.to_string()))),
        );
        pump.pump_for(Duration::from_millis(200));

        // ── 4. burst: the send path must not block; the outbox observables must report it ──
        check(
            "transport and broker threads both started",
            log_text().contains("transport thread started")
                && log_text().contains("broker thread started"),
            "",
        );
        let mut max_send = Duration::ZERO;
        let burst = 1000u32;
        let payload = [0x41u8; 64];
        for _ in 0..burst {
            let b = DataBuffer {
                ptr: payload.as_ptr(),
                size: payload.len() as u32,
                _pad: 0,
            };
            let t0 = Instant::now();
            let r = send(
                local_ep,
                1,
                null(),
                0x2, // best-effort sequential: no retransmit storm
                null(),
                1,
                &b as *const DataBuffer as *const u8,
                null_mut(),
            );
            max_send = max_send.max(t0.elapsed());
            assert_eq!(r, 0, "send {burst} burst failed");
        }
        check(
            "1000 sends do not block the game thread",
            max_send < Duration::from_millis(20),
            &format!("max_send={}us", max_send.as_micros()),
        );
        // Drain the datagrams the transport thread sent to our endpoint (now 127.0.0.1:our port).
        udp.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
        let mut drained = 0u32;
        while udp.recv_from(&mut rbuf).is_ok() {
            drained += 1;
        }
        let hb = wait_log_line(
            |l| {
                l.contains("transport[heartbeat]")
                    && field_u64(l, "jobs").unwrap_or(0) >= burst as u64
            },
            Duration::from_secs(8),
        );
        let jobs = hb.as_deref().and_then(|l| field_u64(l, "jobs")).unwrap_or(0);
        let hwm = hb
            .as_deref()
            .and_then(|l| field_u64(l, "outbox_hwm"))
            .unwrap_or(0);
        check(
            "heartbeat reports the drained burst and the outbox high-water mark",
            jobs >= burst as u64 && hwm > 0,
            &format!("jobs={jobs} outbox_hwm={hwm} datagrams_to_test_socket={drained}"),
        );
        let stats = wait_log_line(
            |l| l.contains("msg_stats") && l.contains("transport[") && l.contains("broker["),
            Duration::from_secs(8),
        );
        check(
            "msg_stats carries the transport and broker counters",
            stats.is_some(),
            &format!("{}", stats.unwrap_or_default()),
        );

        // ── 5. stale in-flight poll must not be applied to the re-joined network ──
        fake.set(&["stale_peer", "fresh_peer"]);
        broker.set_script(vec![
            vec![member("stale_peer", "192.0.2.30", 27030)],
            vec![member("fresh_peer", "192.0.2.31", 27031)],
        ]);
        // Wait for A's next poll (issued at the 200ms tick gate; one in flight at a time), which
        // the broker records before its 700ms delay, then leave while that response is still in
        // flight. The tick must keep running while we wait — that is what issues the request.
        let mark = broker.reqs.lock().unwrap().len();
        let mut a_poll = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            pump.tick();
            if let Some(r) = broker
                .reqs
                .lock()
                .unwrap()
                .iter()
                .skip(mark)
                .find(|r| r.method == "GET" && r.path.starts_with("/party/peers"))
            {
                a_poll = Some(r.clone());
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        check("peer poll request reaches the broker", a_poll.is_some(), "");
        let t0 = Instant::now();
        let leave_r = leave(net, null_mut());
        let leave_ms = t0.elapsed().as_millis();
        check(
            "LeaveNetwork returns with /party/leave delayed 700ms",
            leave_r == 0 && leave_ms < 250,
            &format!("took={leave_ms}ms"),
        );
        let mut net_b: H = null_mut();
        let connect_b = connect(handle, desc.as_ptr(), null_mut(), &mut net_b);
        check(
            "reconnect to the same descriptor creates a new network box",
            connect_b == 0 && !net_b.is_null() && net_b != net,
            &format!("a={net:?} b={net_b:?}"),
        );
        let mut ep_b: H = null_mut();
        mk_ep(net_b, user, 0, null(), null(), null_mut(), &mut ep_b);
        pump.remote_eps.clear();
        let fresh = pump.pump_until(
            |p| p.remote_eps.iter().any(|(e, _)| e == "fresh_peer"),
            Duration::from_secs(6),
        );
        let stale_seen = pump.remote_eps.iter().any(|(e, _)| e == "stale_peer");
        check(
            "stale in-flight poll is dropped; the fresh one is applied",
            fresh && !stale_seen,
            &format!(
                "remote_eps={:?}",
                pump.remote_eps.iter().map(|(e, _)| e.clone()).collect::<Vec<_>>()
            ),
        );
        let leave_req = broker.wait_req(
            |r| r.method == "POST" && r.path == "/party/leave",
            Duration::from_secs(3),
        );
        check(
            "broker thread sends /party/leave off-thread",
            leave_req.is_some(),
            "",
        );
        pump.pump_for(Duration::from_millis(200));

        // ── 6. cached log handle re-opens after the file is deleted ──
        let log = shim_log_path();
        let removed = std::fs::remove_file(&log).is_ok();
        let reappeared = {
            let end = Instant::now() + Duration::from_secs(9);
            let payload8 = [0u8; 8];
            let mut ok = false;
            while Instant::now() < end {
                // Force a log line so the reopen is driven by an actual write (the heartbeat
                // would also do it; this keeps the wait deterministic).
                let b = DataBuffer {
                    ptr: payload8.as_ptr(),
                    size: payload8.len() as u32,
                    _pad: 0,
                };
                send(
                    local_ep,
                    1,
                    null(),
                    0x2,
                    null(),
                    1,
                    &b as *const DataBuffer as *const u8,
                    null_mut(),
                );
                if log.exists() {
                    ok = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            ok
        };
        check(
            "log handle re-opens within the 5s window after deletion",
            removed && reappeared,
            &format!("removed={removed} reappeared={reappeared}"),
        );

        check(
            "every StartProcessing stayed responsive across the whole run",
            pump.max_tick < Duration::from_millis(100),
            &format!(
                "max_tick={}ms ticks={}",
                pump.max_tick.as_millis(),
                pump.ticks
            ),
        );

        let _ = cleanup(handle);
    }

    let failures = FAILURES.load(Ordering::SeqCst);
    if failures > 0 {
        println!("{failures} FAILURE(S)");
        std::process::exit(1);
    }
    println!("all broker/http/transport checks passed");
}

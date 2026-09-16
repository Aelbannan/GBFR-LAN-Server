//! LAN replacement for PlayFabMultiplayerWin.dll (exe IAT subset).
//! Lobbies are stored on the LAN HTTP server (gbfr-lan-server.exe). No SignalR.

#![allow(non_snake_case, clippy::missing_safety_doc)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{c_char, c_void, CString};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

include!("../../common/lan_cfg.rs");

const S_OK: i32 = 0;
const E_FAIL: i32 = 0x8000_4005u32 as i32;

// E_PF_* codes returned by the genuine Microsoft SDK in the conditions this shim can also
// detect. Extracted read-only from PlayFabMultiplayerWin.dll.ms 1.8.2506.05002 (GetErrorMessage
// helper RVA 0x35c90; service-error mapper RVA 0xb2790).
const E_PF_NOT_INITIALIZED: i32 = 0x8923_6400u32 as i32;
const E_PF_INSTANCE_ALREADY_EXISTS: i32 = 0x8923_6401u32 as i32;
const E_PF_ENTITY_KEY_MALFORMED: i32 = 0x8923_6404u32 as i32;
const E_PF_ENTITY_TOKEN_MALFORMED: i32 = 0x8923_6213u32 as i32;
const E_PF_OBJECT_STILL_PENDING: i32 = 0x8923_6205u32 as i32;
const E_PF_LOBBY_NOT_FOUND: i32 = 0x8923_6226u32 as i32; // service 13000
const E_PF_LOBBY_NOT_JOINABLE: i32 = 0x8923_6227u32 as i32; // service 13003
const E_PF_LOBBY_ALREADY_MEMBER: i32 = 0x8923_6225u32 as i32; // service 13002
const E_PF_LOBBY_FULL: i32 = 0x8923_6206u32 as i32; // service 13005
const E_PF_LOBBY_MEMBER_NOT_IN_LOBBY: i32 = 0x8923_6220u32 as i32;
const E_PF_LOBBY_EMPTY_UPDATE: i32 = 0x8923_6222u32 as i32;
const E_PF_LOBBY_UPDATE_AFTER_DISCONNECT: i32 = 0x8923_6223u32 as i32;
const E_PF_SERVICE_BAD_REQUEST: i32 = 0x8923_621eu32 as i32; // service 13007
const E_PF_SERVICE_UNEXPECTED: i32 = 0x8923_6208u32 as i32;
const E_PF_SERVICE_MALFORMED_RESPONSE: i32 = 0x8923_6221u32 as i32;
const E_PF_SERVICE_4XX: i32 = 0x8923_6409u32 as i32;
const E_PF_SERVICE_5XX: i32 = 0x8923_640au32 as i32;

fn log_path() -> String {
    static P: OnceLock<String> = OnceLock::new();
    P.get_or_init(|| {
        let mut buf = [0u16; 260];
        let n = unsafe { GetModuleFileNameW(ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) };
        let s = String::from_utf16_lossy(&buf[..n as usize]);
        let dir = std::path::Path::new(&s)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        dir.join("playfab_mp_shim.log").to_string_lossy().into_owned()
    })
    .clone()
}

static LOG_TRUNCATED: AtomicBool = AtomicBool::new(false);

/// Rate-limited logging for hot paths. See the party shim's twin: one line per second per key,
/// with a suppressed count so a runaway loop stays visible.
fn log_throttled(key: &str, msg: &str) {
    static LAST: OnceLock<Mutex<HashMap<String, (Instant, u32)>>> = OnceLock::new();
    let m = LAST.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    let e = g.entry(key.to_string()).or_insert((Instant::now(), 0));
    if e.0.elapsed() >= Duration::from_secs(1) {
        let n = e.1;
        *e = (Instant::now(), 0);
        if n > 0 {
            log_line(&format!("{msg} [+{n} more in the previous second]"));
        } else {
            log_line(msg);
        }
    } else {
        e.1 += 1;
    }
}

/// Cached log handle, re-opened periodically (same scheme as the Party shim): the per-line
/// open/write/close this used to do was a syscall storm on the game's tick thread.
const LOG_REOPEN_MS: u128 = 5000;
static LOG_FILE: OnceLock<Mutex<Option<(std::fs::File, Instant)>>> = OnceLock::new();

fn log_line(msg: &str) {
    let line = format!("[{}] {msg}\n", now_secs());
    let m = LOG_FILE.get_or_init(|| Mutex::new(None));
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    let stale = match g.as_ref() {
        Some((_, opened)) => opened.elapsed().as_millis() >= LOG_REOPEN_MS,
        None => true,
    };
    if stale {
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true);
        if !LOG_TRUNCATED.swap(true, Ordering::SeqCst) {
            opts.write(true).truncate(true);
        } else {
            opts.append(true);
        }
        match opts.open(log_path()) {
            Ok(f) => *g = Some((f, Instant::now())),
            Err(_) => return,
        }
    }
    if let Some((f, _)) = g.as_mut() {
        if f.write_all(line.as_bytes()).is_err() {
            *g = None;
        }
    }
}

/// Verbose-only line. Off unless lan.ini `[debug] enabled = true` or `GBFR_LAN_DEBUG=1`;
/// `log_line` is reserved for errors and warnings so debug-off runs stay quiet.
#[inline]
fn debug_log(msg: &str) {
    if debug_enabled() {
        log_line(msg);
    }
}

#[inline]
fn debug_throttled(key: &str, msg: &str) {
    if debug_enabled() {
        log_throttled(key, msg);
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// CHANGE 1 — PlayFab lobby state-change queue lifecycle probe (FIX_BACKLOG §8.1-8.3).
//
// The 2026-09-15 run lost ~85 s of PlayFab-side log right after the join (the guest's last line was
// `JoinLobbyCompleted`, while its party log continued). The shim could not answer the one question
// that mattered — were those queued changes ever handed to the title? — because Start was logged
// only when its batch was non-empty and Finish was not logged at all. Every queue event now emits a
// compact `pfqueue ...` line, and a ~2 s heartbeat thread keeps the (pending, in_flight,
// outstanding) tuple visible even if the title stops calling Start/Finish entirely, which is the
// exact way that 85 s silence presented.
//
// CHANGE 1b (log volume, 2026-09-15 follow-up): the first instrumented run wrote one `pfqueue
// start` and one `pfqueue finish` per call (~12k each), almost all identical empty pumps, and paid
// a `format!` on the game's tick thread for every one. Start/Finish state lines are now emitted
// only when their (n, pending, inflight, outstanding) tuple changes, plus one ~5 s heartbeat each
// so a stuck tuple stays visible; the existing hb thread still restates the tuple every ~2 s. The
// cumulative view lives on that hb line (`starts=`, `finishes=`, `batches_nonzero=`,
// `max_pending=`, `max_inflight=`), so the suppressed per-call totals stay recoverable.
// ---------------------------------------------------------------------------

/// Heartbeat cadence and probe-thread tick.
const PFQUEUE_HEARTBEAT_MS: u64 = 30_000;
const PFQUEUE_TICK_MS: u64 = 500;
/// CHANGE 1b: per-site state-line heartbeat. A tuple that never changes is restated at most this
/// often, so a stuck Start/Finish state cannot hide even between the 30 s thread heartbeats.
const PFQUEUE_STATE_HEARTBEAT_MS: u64 = 30_000;

/// Published queue snapshot. Written under the handle lock, read by the heartbeat thread without
/// it (so the probe can never block or be blocked by the game's tick thread).
static Q_PENDING: AtomicUsize = AtomicUsize::new(0);
static Q_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
static Q_OUTSTANDING: AtomicBool = AtomicBool::new(false);
/// Lifetime counters, so the heartbeat line also says how much the pump has moved overall.
static Q_STARTS: AtomicU64 = AtomicU64::new(0);
static Q_FINISHES: AtomicU64 = AtomicU64::new(0);
static Q_QUEUED: AtomicU64 = AtomicU64::new(0);
static Q_RECLAIMED: AtomicU64 = AtomicU64::new(0);
static Q_LATCHED: AtomicU64 = AtomicU64::new(0);
static Q_STRAY: AtomicU64 = AtomicU64::new(0);
static Q_MISMATCH: AtomicU64 = AtomicU64::new(0);
/// CHANGE 1b: cumulative summary shown on the hb line, so the suppressed per-call lines stay
/// recoverable. `batches_nonzero` counts fresh batches with n>0; the maxes are high-water marks
/// published whenever the queue snapshot is taken.
static Q_BATCHES_NONZERO: AtomicU64 = AtomicU64::new(0);
static Q_MAX_PENDING: AtomicUsize = AtomicUsize::new(0);
static Q_MAX_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
static Q_HEARTBEAT_RUNNING: AtomicBool = AtomicBool::new(false);

// CHANGE 2(a), PF-15/D4: the SDK contract is that StartProcessing returns the *whole* pending
// batch; the old 32-change cap was our deviation. It stays available for A/B runs but is off by
// default; when off, the entire queue is handed out and only oversized batches are called out.
const BATCH_CAP_ENABLED: bool = false;
const BATCH_CAP: usize = 32;
/// Batches larger than this are logged (throttled) so throughput problems stay visible.
const BATCH_LARGE: usize = 64;

/// CHANGE 1b: cheap change gate for the per-call Start/Finish state lines. Comparing the tuple
/// and one `Instant` costs no allocation; the caller builds the line only when `due` returns
/// nonzero, matching the party shim's "never format a suppressed line" rule.
struct StateLog {
    key: (usize, usize, usize, bool),
    at: Instant,
}

impl StateLog {
    fn new() -> Self {
        StateLog {
            // Sentinel: no real tuple is all-max, so the first state line always logs.
            key: (usize::MAX, usize::MAX, usize::MAX, true),
            at: Instant::now(),
        }
    }

    /// 0 = suppressed (tuple unchanged and heartbeat not due), 1 = tuple changed, 2 = heartbeat.
    fn due(&mut self, next: (usize, usize, usize, bool)) -> u8 {
        let changed = self.key != next;
        if !changed && self.at.elapsed() < Duration::from_millis(PFQUEUE_STATE_HEARTBEAT_MS) {
            return 0;
        }
        self.key = next;
        self.at = Instant::now();
        if changed { 1 } else { 2 }
    }
}

/// Publish the current tuple for the heartbeat thread. Cheap: three relaxed stores, no allocation
/// beyond the two guarded high-water marks.
fn probe_snapshot(m: &Mp) {
    let pending = m.pending.len();
    let inflight = m.in_flight.len();
    Q_PENDING.store(pending, Ordering::Relaxed);
    Q_INFLIGHT.store(inflight, Ordering::Relaxed);
    Q_OUTSTANDING.store(m.batch_outstanding, Ordering::Relaxed);
    // The relaxed load first keeps the common no-new-max case free of a locked RMW; only the
    // queue mutator (under the global lock) increases these, the hb thread only reads them.
    if pending > Q_MAX_PENDING.load(Ordering::Relaxed) {
        Q_MAX_PENDING.fetch_max(pending, Ordering::Relaxed);
    }
    if inflight > Q_MAX_INFLIGHT.load(Ordering::Relaxed) {
        Q_MAX_INFLIGHT.fetch_max(inflight, Ordering::Relaxed);
    }
}

/// Start the one probe thread. It emits `pfqueue hb ...` on every tuple change (observed at most
/// every 500 ms) and otherwise at most every 2 s, so a permanently stuck queue is never silent.
/// Allocation happens only when a line is actually emitted.
fn start_queue_heartbeat() {
    if Q_HEARTBEAT_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("pfqueue-hb".into())
        .spawn(|| {
            let mut last_key = (usize::MAX, usize::MAX, false);
            let mut last_log = Instant::now() - Duration::from_secs(10);
            loop {
                std::thread::sleep(Duration::from_millis(PFQUEUE_TICK_MS));
                let pending = Q_PENDING.load(Ordering::Relaxed);
                let inflight = Q_INFLIGHT.load(Ordering::Relaxed);
                let outstanding = Q_OUTSTANDING.load(Ordering::Relaxed);
                let key = (pending, inflight, outstanding);
                let changed = key != last_key;
                if !changed && (last_log.elapsed().as_millis() as u64) < PFQUEUE_HEARTBEAT_MS {
                    continue;
                }
                last_key = key;
                last_log = Instant::now();
                debug_log(&format!(
                    "pfqueue hb pending={pending} inflight={inflight} outstanding={} starts={} finishes={} batches_nonzero={} queued={} reclaimed={} max_pending={} max_inflight={} latch={} stray={} mismatch={}{}",
                    outstanding as u8,
                    Q_STARTS.load(Ordering::Relaxed),
                    Q_FINISHES.load(Ordering::Relaxed),
                    Q_BATCHES_NONZERO.load(Ordering::Relaxed),
                    Q_QUEUED.load(Ordering::Relaxed),
                    Q_RECLAIMED.load(Ordering::Relaxed),
                    Q_MAX_PENDING.load(Ordering::Relaxed),
                    Q_MAX_INFLIGHT.load(Ordering::Relaxed),
                    Q_LATCHED.load(Ordering::Relaxed),
                    Q_STRAY.load(Ordering::Relaxed),
                    Q_MISMATCH.load(Ordering::Relaxed),
                    if changed { " changed=1" } else { "" }
                ));
            }
        });
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleFileNameW(module: *mut c_void, buf: *mut u16, size: u32) -> u32;
}

fn stub_host() -> (String, u16) {
    let c = lan_cfg();
    (c.host, c.port)
}

fn http_json(method: &str, path: &str, body: &str) -> Option<String> {
    http_json_status(method, path, body).map(|(_status, body)| body)
}

/// Bounded connect. The OS default connect timeout is ~21 s on Windows, and these calls run on
/// the game's tick thread while the shim's global mutex is held, so a dead broker host would
/// otherwise freeze the whole tick.
fn connect_stub(host: &str, port: u16) -> Option<TcpStream> {
    use std::net::ToSocketAddrs;
    let addr = (host, port).to_socket_addrs().ok()?.next()?;
    TcpStream::connect_timeout(&addr, Duration::from_millis(1000)).ok()
}

/// Same as `http_json`, but also returns the HTTP status so callers can tell a refusal from a
/// success. `None` means no usable response at all (broker down, timeout, malformed reply);
/// `.0 == 0` means the status line could not be parsed.
fn http_json_status(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    let (host, port) = stub_host();
    let mut stream = connect_stub(&host, port)?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    let auth = auth_header();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let idx = text.find("\r\n\r\n")?;
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    Some((status, text[idx + 4..].to_string()))
}

/// Map the broker's PlayFab-shaped error envelope to the code the genuine SDK returns for the
/// same service condition. The broker emits
/// `{"code":..,"status":..,"error":"LobbyNotFound"|"LobbyMemberLimitExceeded"}`.
/// PFMP's service-error mapper (RVA 0xb2790) maps 13000 LobbyDoesNotExist -> 0x89236226,
/// 13003 LobbyNotJoinable -> 0x89236227, 13002 LobbyPlayerAlreadyJoined -> 0x89236225 and
/// 13005 LobbyCurrentPlayersMoreThanMaxPlayers -> 0x89236206.
fn broker_error_code(status: u16, body: &str) -> i32 {
    if body.contains("LobbyNotFound") {
        return E_PF_LOBBY_NOT_FOUND;
    }
    if body.contains("LobbyMemberLimitExceeded") || body.contains("PartyMemberLimitExceeded") {
        return E_PF_LOBBY_FULL;
    }
    if body.contains("LobbyNotJoinable") {
        return E_PF_LOBBY_NOT_JOINABLE;
    }
    if body.contains("LobbyPlayerAlreadyJoined") || body.contains("LobbyAlreadyMember") {
        return E_PF_LOBBY_ALREADY_MEMBER;
    }
    match status {
        400 => E_PF_SERVICE_BAD_REQUEST,
        404 => E_PF_LOBBY_NOT_FOUND,
        409 => E_PF_SERVICE_UNEXPECTED,
        s if (500..600).contains(&s) => E_PF_SERVICE_5XX,
        s if (400..500).contains(&s) => E_PF_SERVICE_4XX,
        _ => E_PF_SERVICE_UNEXPECTED,
    }
}

/// True when the exported PFMultiplayerHandle refers to the live instance. The genuine DLL
/// returns E_PF_NOT_INITIALIZED for a null/foreign handle (depth-0 literals in CreateAndJoin,
/// Join, FindLobbies, Start, Finish, SetEntityToken; depth-1 in Uninitialize).
fn mp_ready(handle: *const c_void) -> bool {
    !handle.is_null() && g().lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// Char-boundary-safe truncation for log lines (byte slicing panics on multibyte input).
fn truncate_log(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && b[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

fn parse_json_string(b: &[u8], i: &mut usize) -> Option<String> {
    if *i >= b.len() || b[*i] != b'"' {
        return None;
    }
    *i += 1;
    let mut out = Vec::new();
    while *i < b.len() {
        let c = b[*i];
        if c == b'\\' {
            *i += 1;
            if *i >= b.len() {
                break;
            }
            match b[*i] {
                b'"' => out.push(b'"'),
                b'\\' => out.push(b'\\'),
                b'/' => out.push(b'/'),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'u' => {
                    *i += 1;
                    let hex_end = (*i + 4).min(b.len());
                    if let Ok(hex) = std::str::from_utf8(&b[*i..hex_end]) {
                        if let Ok(cp) = u32::from_str_radix(hex, 16) {
                            if let Some(ch) = char::from_u32(cp) {
                                let mut buf = [0u8; 4];
                                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            }
                        }
                    }
                    *i = hex_end;
                    continue;
                }
                other => out.push(other),
            }
            *i += 1;
            continue;
        }
        if c == b'"' {
            *i += 1;
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
        out.push(c);
        *i += 1;
    }
    None
}

fn parse_json_nested(b: &[u8], i: &mut usize) -> String {
    let start = *i;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    while *i < b.len() {
        let c = b[*i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    *i += 1;
                    if depth == 0 {
                        return String::from_utf8_lossy(&b[start..*i]).into_owned();
                    }
                    continue;
                }
                _ => {}
            }
        }
        *i += 1;
    }
    String::from_utf8_lossy(&b[start..*i]).into_owned()
}

fn parse_json_value(b: &[u8], i: &mut usize) -> String {
    skip_ws(b, i);
    if *i >= b.len() {
        return String::new();
    }
    match b[*i] {
        b'"' => parse_json_string(b, i).unwrap_or_default(),
        b'{' | b'[' => parse_json_nested(b, i),
        _ => {
            let start = *i;
            while *i < b.len()
                && b[*i] != b','
                && b[*i] != b'}'
                && b[*i] != b']'
                && !b[*i].is_ascii_whitespace()
            {
                *i += 1;
            }
            String::from_utf8_lossy(&b[start..*i]).into_owned()
        }
    }
}

fn json_u32(blob: &str, key: &str) -> Option<u32> {
    json_str(blob, key)?.parse().ok()
}

fn json_max_players(blob: &str) -> u32 {
    json_u32(blob, "MaxPlayers")
        .or_else(|| json_u32(blob, "max_member"))
        .unwrap_or(8)
        .clamp(2, 32)
}

/// Broker GetLobby serves membershipLock as "Unlocked"/"Locked" (main.rs lobby_public),
/// but accept the numeric enum too. Anything unknown is Unlocked, which is the documented
/// default the service assigns at creation.
fn parse_membership_lock(s: &str) -> i32 {
    if s.eq_ignore_ascii_case("locked") || s == "1" {
        1
    } else {
        0
    }
}

/// Broker GetLobby serves accessPolicy as "Public"/"Friends"/"Private".
fn parse_access_policy(s: &str) -> u32 {
    match s.to_ascii_lowercase().as_str() {
        "friends" => 1,
        "private" => 2,
        _ => 0,
    }
}

fn json_str(blob: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":");
    let idx = blob.find(&pat)?;
    let b = blob.as_bytes();
    let mut i = idx + pat.len();
    skip_ws(b, &mut i);
    if i < b.len() && b[i] == b'"' {
        parse_json_string(b, &mut i)
    } else {
        let v = parse_json_value(b, &mut i);
        if v.is_empty() || v == "null" {
            None
        } else {
            Some(v)
        }
    }
}

fn json_obj(blob: &str, key: &str) -> HashMap<String, String> {
    let pat = format!("\"{key}\":");
    let Some(idx) = blob.find(&pat) else {
        return HashMap::new();
    };
    let b = blob.as_bytes();
    let mut i = idx + pat.len();
    skip_ws(b, &mut i);
    if i >= b.len() || b[i] != b'{' {
        return HashMap::new();
    }
    i += 1;
    let mut map = HashMap::new();
    loop {
        skip_ws(b, &mut i);
        if i >= b.len() || b[i] == b'}' {
            break;
        }
        if b[i] == b',' {
            i += 1;
            continue;
        }
        let Some(k) = parse_json_string(b, &mut i) else {
            break;
        };
        skip_ws(b, &mut i);
        if i >= b.len() || b[i] != b':' {
            break;
        }
        i += 1;
        let v = parse_json_value(b, &mut i);
        if !k.is_empty() {
            map.insert(k, v);
        }
        if map.len() >= 64 {
            break;
        }
    }
    map
}

/// Read-only diagnostic: short printable preview of a string.
fn preview_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("...");
        out
    }
}

/// Read-only diagnostic: `key=value` list for a CString map (sorted, capped).
fn map_preview(m: &HashMap<String, CString>, max: usize) -> String {
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    let mut parts = Vec::new();
    for k in keys {
        let v = m[k].to_string_lossy();
        parts.push(format!("{k}={}", preview_str(&v, 48)));
    }
    preview_str(&parts.join(", "), max)
}

fn json_arr_objects(blob: &str, key: &str) -> Vec<String> {
    let pat = format!("\"{key}\":[");
    let Some(rest) = blob.split(&pat).nth(1) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = None;
    for (i, c) in rest.char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        out.push(rest[s..=i].to_string());
                    }
                    start = None;
                    if out.len() >= 32 {
                        break;
                    }
                }
            }
            ']' if depth == 0 => break,
            _ => {}
        }
    }
    out
}

fn read_cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: `p` is null-checked; the scan is capped at 4096 bytes, so a missing terminator
    // truncates instead of walking forever. Pointers come from game- or SDK-owned strings
    // (PFEntityKey fields, state-change keys) that outlive the call.
    let mut n = 0usize;
    unsafe {
        while n < 4096 && *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, n)).into_owned()
    }
}

fn intern(s: &str) -> *const c_char {
    let c = CString::new(s).unwrap_or_else(|_| CString::new("").unwrap());
    let p = c.as_ptr();
    std::mem::forget(c);
    p
}

/// # Safety
/// Allocates a fresh array of interned C strings. The memory is intentionally leaked: the SDK
/// contract keeps the change's key arrays valid until PFMultiplayerFinishProcessingLobbyStateChanges,
/// and there is currently no reclaim path (see TODO.md). Counts are capped at 32, so the
/// allocation size is bounded by the caller.
unsafe fn intern_kv_arrays(map: &HashMap<String, String>) -> (u32, *const *const c_char, *const *const c_char) {
    let n = map.len().min(32);
    if n == 0 {
        return (0, ptr::null(), ptr::null());
    }
    let layout = std::alloc::Layout::array::<*const c_char>(n).unwrap();
    let keys = std::alloc::alloc_zeroed(layout) as *mut *const c_char;
    let vals = std::alloc::alloc_zeroed(layout) as *mut *const c_char;
    if keys.is_null() || vals.is_null() {
        return (0, ptr::null(), ptr::null());
    }
    for (i, (k, v)) in map.iter().take(n).enumerate() {
        *keys.add(i) = intern(k);
        *vals.add(i) = intern(v);
    }
    (n as u32, keys, vals)
}

fn intern_owner_key(row: &str) -> *const EntityKey {
    let owner = json_obj(row, "Owner");
    let id = owner
        .get("Id")
        .cloned()
        .or_else(|| json_str(row, "Owner"))
        .unwrap_or_default();
    if id.is_empty() {
        return ptr::null();
    }
    let ty = owner
        .get("Type")
        .cloned()
        .unwrap_or_else(|| "title_player_account".into());
    Box::into_raw(Box::new(EntityKey {
        id: intern(&id),
        type_: intern(&ty),
    }))
}

fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// # Safety
/// `src` is a game-owned PFEntityKey (two NUL-terminated char* fields) valid for the call;
/// null maps to the default LAN identity. Values are copied into interned CStrings, so the
/// result does not borrow `src`.
unsafe fn intern_entity(src: *const EntityKey) -> EntityKey {
    if src.is_null() {
        return EntityKey {
            id: intern("lan-user"),
            type_: intern("title_player_account"),
        };
    }
    let id = read_cstr((*src).id);
    let ty = read_cstr((*src).type_);
    EntityKey {
        id: intern(if id.is_empty() { "lan-user" } else { &id }),
        type_: intern(if ty.is_empty() {
            "title_player_account"
        } else {
            &ty
        }),
    }
}

fn decimal_account_id(entity_id: &str) -> String {
    if !entity_id.is_empty() && entity_id.chars().all(|c| c.is_ascii_digit()) {
        return entity_id.to_string();
    }
    let mut n: u64 = 2166136261;
    for b in entity_id.as_bytes() {
        n ^= *b as u64;
        n = n.wrapping_mul(16777619);
    }
    if n == 0 {
        n = 1;
    }
    n.to_string()
}

fn ensure_member_props(mp: &mut HashMap<String, CString>, entity_id: &str) {
    // Exe FUN_140ad6eb0 maps the string "Steam" to enum 1. A digit "1" becomes 0xFFFFFFFF.
    let plat = mp
        .get("member_platform")
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default();
    if plat.is_empty() || plat == "1" {
        mp.insert("member_platform".into(), CString::new("Steam").unwrap());
    }
    // MemberAdded parses this with strtoull base 10 and aborts on "invalid stoull argument".
    let numeric = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let account = mp
        .get("member_platform_account_id")
        .map(|c| c.to_string_lossy().into_owned())
        .filter(|s| numeric(s))
        .or_else(|| {
            if numeric(entity_id) {
                Some(entity_id.to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| decimal_account_id(entity_id));
    mp.insert(
        "member_platform_account_id".into(),
        CString::new(account).unwrap_or_else(|_| CString::new("1").unwrap()),
    );
    if !mp.contains_key("member_platform_user_name") {
        mp.insert(
            "member_platform_user_name".into(),
            CString::new("LAN").unwrap(),
        );
    }
}

fn member_data_json(entity_id: &str, extra: &HashMap<String, String>) -> String {
    let mut mp = HashMap::new();
    apply_props(&mut mp, extra.clone());
    ensure_member_props(&mut mp, entity_id);
    let mut s = String::from("{");
    for (i, (k, v)) in mp.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "\"{k}\":\"{}\"",
            json_escape(&v.to_string_lossy())
        ));
    }
    s.push('}');
    s
}

/// # Safety
/// Reads the documented PFLobbyJoinConfiguration layout: u32 count @0, key array @8, value
/// array @16. The game owns both arrays for the duration of the call; every element is read
/// through `kv_list`, which validates the pointers before reading.
unsafe fn join_cfg_kv(join_cfg: *const u8) -> HashMap<String, String> {
    if join_cfg.is_null() {
        return HashMap::new();
    }
    let count = ptr::read_unaligned(join_cfg as *const u32);
    let keys = ptr::read_unaligned(join_cfg.add(8) as *const *const *const c_char);
    let vals = ptr::read_unaligned(join_cfg.add(16) as *const *const *const c_char);
    kv_list(count, keys, vals)
}

/// Intern a key list into a freshly allocated C-string pointer array for a state change.
/// The allocation is intentionally leaked: the SDK contract keeps the change, and its
/// arrays, valid until PFMultiplayerFinishProcessingLobbyStateChanges.
unsafe fn intern_cstr_list(items: &[String]) -> (u32, *const *const c_char) {
    if items.is_empty() {
        return (0, ptr::null());
    }
    let layout = std::alloc::Layout::array::<*const c_char>(items.len()).unwrap();
    let arr = std::alloc::alloc_zeroed(layout) as *mut *const c_char;
    if arr.is_null() {
        return (0, ptr::null());
    }
    for (i, s) in items.iter().enumerate() {
        *arr.add(i) = intern(s);
    }
    (items.len() as u32, arr)
}

/// Build the memberUpdates array of a PFLobbyUpdatedStateChange.
/// PFLobbyMemberUpdateSummary layout (header + exe FUN_143B48800): inline PFEntityKey at
/// +0x00 (16 bytes), bool connectionStatusUpdated at +0x10, uint32
/// updatedMemberPropertyCount at +0x14, const char* const* updatedMemberPropertyKeys at
/// +0x18; stride 0x20 (exe: `shl rdi, 5` at 0x143B48947). The count sits at +0x14 — the
/// field the exe gates its GetMemberConnectionStatus call on (0x143B4894B).
/// The block is allocated from Rust's heap and intentionally leaked until FinishProcessing
/// (see TODO.md); the stride and field offsets are the exe's, verified above.
unsafe fn member_update_entries(updates: &[MemberUpdate]) -> (u32, *mut u8) {
    if updates.is_empty() {
        return (0, ptr::null_mut());
    }
    const STRIDE: usize = 0x20;
    let layout = std::alloc::Layout::from_size_align(updates.len() * STRIDE, 8).unwrap();
    let base = std::alloc::alloc_zeroed(layout);
    if base.is_null() {
        return (0, ptr::null_mut());
    }
    for (i, u) in updates.iter().enumerate() {
        let e = base.add(i * STRIDE);
        ptr::write_unaligned(e as *mut EntityKey, u.member);
        ptr::write_unaligned(e.add(0x10) as *mut u8, u.connection_status_updated as u8);
        let (n, keys) = intern_cstr_list(&u.property_keys);
        ptr::write_unaligned(e.add(0x14) as *mut u32, n);
        ptr::write_unaligned(e.add(0x18) as *mut *const *const c_char, keys);
    }
    (updates.len() as u32, base)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EntityKey {
    id: *const c_char,
    type_: *const c_char,
}

struct Lobby {
    id: CString,
    connection: CString,
    owner: EntityKey,
    /// False once the service reports no owner (the owner left under migration policy
    /// None/Manual). PFLobbyGetOwner then answers S_OK + NULL, as the genuine SDK does.
    has_owner: bool,
    max_players: u32,
    props: HashMap<String, CString>,
    search: HashMap<String, CString>,
    member_props: HashMap<String, HashMap<String, CString>>,
    members: Vec<EntityKey>,
    /// Service-assigned membership lock (0 = Unlocked, 1 = Locked), as the broker's
    /// GetLobby reports it. The exe's Updated handler reads it through
    /// PFLobbyGetMembershipLock only when an Updated sets membershipLockUpdated.
    membership_lock: i32,
    /// Service-assigned access policy (0 = Public, 1 = Friends, 2 = Private).
    access_policy: u32,
    /// Whether an Updated carrying membershipLockUpdated=true has been delivered for this
    /// lobby. Until then the title's cached lock is unpopulated; the first Updated must
    /// carry the flag (the SDK documents PFLobbyGetMembershipLock as erroring until one
    /// arrives, and as guaranteed populated before JoinLobbyCompleted).
    lock_known: bool,
    /// Entity ids already delivered as MemberAdded (type 2). Duplicate emits are
    /// skipped by the exe (stride-0x60 scan) but missing emits drop party UI.
    announced: HashSet<String>,
    /// True until this lobby's create/join completion has been announced; the genuine SDK
    /// returns E_PF_OBJECT_STILL_PENDING from the identity getters until then.
    pending: bool,
    /// Set when this shim has queued a Leave/Disconnected for the lobby; the genuine SDK
    /// returns E_PF_LOBBY_UPDATE_AFTER_DISCONNECT for a PostUpdate afterwards.
    left: bool,
}

/// The lobby fields an Updated compares. Taken before applying a local post or before
/// refreshing from the broker, then diffed against the current lobby.
struct LobbySnapshot {
    owner: EntityKey,
    has_owner: bool,
    max_players: u32,
    access_policy: u32,
    membership_lock: i32,
    props: HashMap<String, CString>,
    search: HashMap<String, CString>,
    member_props: HashMap<String, HashMap<String, CString>>,
}

/// One entry of PFLobbyUpdatedStateChange::memberUpdates
/// (PFLobbyMemberUpdateSummary, stride 0x20 - see member_update_entries for the layout).
struct MemberUpdate {
    member: EntityKey,
    connection_status_updated: bool,
    property_keys: Vec<String>,
}

/// Everything an Updated can report. Empty means "nothing changed" and no change is emitted,
/// so a flag is never set without a real difference behind it.
#[derive(Default)]
struct LobbyDelta {
    owner_updated: bool,
    max_players_updated: bool,
    access_policy_updated: bool,
    membership_lock_updated: bool,
    search_keys: Vec<String>,
    lobby_keys: Vec<String>,
    member_updates: Vec<MemberUpdate>,
}

impl LobbyDelta {
    fn is_empty(&self) -> bool {
        !self.owner_updated
            && !self.max_players_updated
            && !self.access_policy_updated
            && !self.membership_lock_updated
            && self.search_keys.is_empty()
            && self.lobby_keys.is_empty()
            && self.member_updates.is_empty()
    }
}

struct Mp {
    title: CString,
    token: Option<CString>,
    entity: Option<EntityKey>,
    lobbies: Vec<*mut Lobby>,
    pending: VecDeque<*mut u8>,
    in_flight: Vec<*mut u8>,
    /// True while the batch pointed to by `in_flight` has been handed to the title and not yet
    /// returned to PFMultiplayerFinishProcessingLobbyStateChanges. The SDK documents the array as
    /// library-allocated and the changes as valid until Finish, so nothing may rebuild or clear it
    /// in the meantime — a Start arriving before Finish must re-return the same array unchanged.
    batch_outstanding: bool,
    last_poll: Instant,
    pending_joins: Vec<PendingJoin>,
    /// CHANGE 1b: last emitted Start/Finish state tuples, so unchanged pumps are not formatted.
    start_state: StateLog,
    finish_state: StateLog,
}

struct PendingJoin {
    lobby: *mut Lobby,
    joiner: EntityKey,
    async_ctx: *mut c_void,
    emitted: bool,
}

unsafe impl Send for EntityKey {}
unsafe impl Sync for EntityKey {}
unsafe impl Send for Lobby {}
unsafe impl Sync for Lobby {}
unsafe impl Send for PendingJoin {}
unsafe impl Sync for PendingJoin {}
unsafe impl Send for Mp {}
unsafe impl Sync for Mp {}

static G: OnceLock<Mutex<Option<Box<Mp>>>> = OnceLock::new();
static ERR: OnceLock<CString> = OnceLock::new();
static AUTH: OnceLock<Mutex<String>> = OnceLock::new();
static LOCAL_ENTITY: OnceLock<Mutex<String>> = OnceLock::new();

/// Last value logged per search-property key. The exe polls these every frame, so logging every
/// call produced 1,134 of 1,165 log lines in one run and buried the real evidence.
static SEARCH_PROP_LOG: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// Full create payloads already dumped, so each distinct one is logged exactly once.
static CREATE_PAYLOAD_LOG: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn auth_header() -> String {
    let tok = AUTH
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default();
    if tok.is_empty() {
        String::new()
    } else {
        format!("X-EntityToken: {tok}\r\n")
    }
}

fn local_entity_id() -> String {
    LOCAL_ENTITY
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default()
}

fn set_local_identity(entity: *const EntityKey, token: &str) {
    let tok = token.replace(['\r', '\n'], "");
    if let Ok(mut g) = AUTH.get_or_init(|| Mutex::new(String::new())).lock() {
        *g = tok;
    }
    if !entity.is_null() {
        let id = unsafe { read_cstr((*entity).id) };
        if let Ok(mut g) = LOCAL_ENTITY.get_or_init(|| Mutex::new(String::new())).lock() {
            *g = id;
        }
    }
}

fn entity_id_of(entity: *const EntityKey) -> String {
    if !entity.is_null() {
        let id = unsafe { read_cstr((*entity).id) };
        if !id.is_empty() {
            return id;
        }
    }
    local_entity_id()
}

fn g() -> &'static Mutex<Option<Box<Mp>>> {
    G.get_or_init(|| Mutex::new(None))
}

fn with_mp<R>(f: impl FnOnce(&mut Mp) -> R, default: R) -> R {
    match g().lock().unwrap().as_mut() {
        Some(m) => f(m),
        None => default,
    }
}

/// Allocate one zeroed state-change record from Rust's global heap; the first 4 bytes are the
/// type tag. The block is intentionally leaked: it is handed to the title and there is
/// currently no reclaim path (see TODO.md). Size comes from the caller's fixed layout.
unsafe fn alloc_sc(size: usize, ty: u32) -> *mut u8 {
    let layout = std::alloc::Layout::from_size_align(size.max(8), 8).unwrap();
    let p = std::alloc::alloc_zeroed(layout);
    if !p.is_null() {
        ptr::write_unaligned(p as *mut u32, ty);
    }
    p
}

fn queue(m: &mut Mp, p: *mut u8) {
    if p.is_null() {
        return;
    }
    m.pending.push_back(p);
    Q_QUEUED.fetch_add(1, Ordering::Relaxed);
    // CHANGE 1: every enqueue is visible with its type and the resulting depth, so a change that
    // is never handed out can be named (exactly the guest's JoinLobbyCompleted + 2x MemberAdded).
    let ty = unsafe { ptr::read_unaligned(p as *const u32) };
    debug_log(&format!("pfqueue queue type={ty} pending={}", m.pending.len()));
    probe_snapshot(m);
}

unsafe fn kv_list(count: u32, keys: *const *const c_char, vals: *const *const c_char) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if keys.is_null() || vals.is_null() || count == 0 {
        return map;
    }
    if count > 32 {
        log_line(&format!("kv_list refuse count={count}"));
        return map;
    }
    for i in 0..count as usize {
        let k = read_cstr(*keys.add(i));
        let v = read_cstr(*vals.add(i));
        if !k.is_empty() {
            map.insert(k, v);
        }
    }
    map
}

fn apply_props(dst: &mut HashMap<String, CString>, src: HashMap<String, String>) {
    for (k, v) in src {
        dst.insert(k, CString::new(v).unwrap_or_else(|_| CString::new("").unwrap()));
    }
}

/// Remove keys locally after a null-valued property update. `PFLobbyDataUpdate`/
/// `PFLobbyMemberDataUpdate` use a null value pointer to mean "delete this key", which the SDK
/// maps to the service's `<Field>ToDelete` arrays (NUMBER_KEY9 §6: folding nulls into empty
/// strings left the key stored forever).
fn drop_props(dst: &mut HashMap<String, CString>, keys: &[String]) {
    for k in keys {
        dst.remove(k);
    }
}

/// `PFLobbyDataUpdate` list semantics: null value pointer = delete. Returns the set pairs and the
/// delete keys separately so the caller can both apply locally and forward the deletes.
unsafe fn kv_list_split(
    count: u32,
    keys: *const *const c_char,
    vals: *const *const c_char,
) -> (HashMap<String, String>, Vec<String>) {
    let mut sets = HashMap::new();
    let mut deletes = Vec::new();
    if keys.is_null() || vals.is_null() || count == 0 {
        return (sets, deletes);
    }
    if count > 32 {
        log_line(&format!("kv_list refuse count={count}"));
        return (sets, deletes);
    }
    for i in 0..count as usize {
        let k = read_cstr(*keys.add(i));
        if k.is_empty() {
            continue;
        }
        let vp = *vals.add(i);
        if vp.is_null() {
            deletes.push(k);
        } else {
            sets.insert(k, read_cstr(vp));
        }
    }
    (sets, deletes)
}

/// Wire form of the access-policy enum.
fn access_policy_name(v: u32) -> &'static str {
    match v {
        1 => "Friends",
        2 => "Private",
        _ => "Public",
    }
}

/// JSON string array for the documented `*ToDelete` request fields.
fn q_str_list(keys: &[String]) -> String {
    keys.iter()
        .map(|k| format!("\"{}\"", json_escape(k)))
        .collect::<Vec<String>>()
        .join(",")
}

/// `key=value` list with values untruncated (bounded only by the whole-string cap). The create
/// payload dump needs this because `map_preview` caps every value at 48 chars, which is exactly
/// what hid the `string_key5`/`string_key6` contents behind an ellipsis.
fn map_full(m: &HashMap<String, CString>, max: usize) -> String {
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    let parts: Vec<String> = keys
        .iter()
        .map(|k| format!("{k}={}", m[*k].to_string_lossy()))
        .collect();
    preview_str(&parts.join(", "), max)
}

fn lobby_from_json(text: &str) -> Option<Box<Lobby>> {
    let id = json_str(text, "LobbyId")?;
    let conn = json_str(text, "ConnectionString").unwrap_or_else(|| format!("lan.{id}"));
    let mut props = HashMap::new();
    for (k, v) in json_obj(text, "LobbyData") {
        props.insert(k, CString::new(v).unwrap_or_else(|_| CString::new("").unwrap()));
    }
    let mut search = HashMap::new();
    for (k, v) in json_obj(text, "SearchData") {
        search.insert(k, CString::new(v).unwrap_or_else(|_| CString::new("").unwrap()));
    }
    let mut members = Vec::new();
    let mut member_props = HashMap::new();
    for m in json_arr_objects(text, "Members") {
        let eid = json_str(&m, "Id")
            .or_else(|| json_str(&m, "id"))
            .or_else(|| {
                json_obj(&m, "MemberEntity")
                    .get("Id")
                    .cloned()
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| {
                m.split("\"Id\":\"")
                    .nth(1)
                    .and_then(|s| s.split('"').next().map(|x| x.to_string()))
            });
        if let Some(eid) = eid {
            let ek = EntityKey {
                id: intern(&eid),
                type_: intern("title_player_account"),
            };
            members.push(ek);
            let nested = json_obj(&m, "MemberData");
            let mut mp = HashMap::new();
            apply_props(&mut mp, nested);
            ensure_member_props(&mut mp, &eid);
            member_props.insert(eid, mp);
        }
    }
    let membership_lock = json_str(text, "MembershipLock")
        .map(|s| parse_membership_lock(&s))
        .unwrap_or(0);
    let access_policy = json_str(text, "AccessPolicy")
        .map(|s| parse_access_policy(&s))
        .unwrap_or(0);
    let owner_id = json_obj(text, "Owner")
        .get("Id")
        .cloned()
        .filter(|s| !s.is_empty());
    let has_owner = owner_id.is_some();
    let owner_id = owner_id.unwrap_or_else(|| "owner".into());
    Some(Box::new(Lobby {
        id: CString::new(id).ok()?,
        connection: CString::new(conn).ok()?,
        owner: EntityKey {
            id: intern(&owner_id),
            type_: intern("title_player_account"),
        },
        has_owner,
        max_players: json_max_players(text),
        props,
        search,
        member_props,
        members,
        membership_lock,
        access_policy,
        lock_known: false,
        announced: HashSet::new(),
        pending: false,
        left: false,
    }))
}

fn post_create(
    max: u32,
    owner_migration_policy: u32,
    lobby_data: &HashMap<String, String>,
    search: &HashMap<String, String>,
    owner_id: &str,
    member_kv: &HashMap<String, String>,
) -> Result<Box<Lobby>, i32> {
    let mut ld = String::from("{");
    for (i, (k, v)) in lobby_data.iter().enumerate() {
        if i > 0 {
            ld.push(',');
        }
        ld.push_str(&format!("\"{k}\":\"{}\"", json_escape(v)));
    }
    ld.push('}');
    let mut sd = String::from("{");
    for (i, (k, v)) in search.iter().enumerate() {
        if i > 0 {
            sd.push(',');
        }
        sd.push_str(&format!("\"{k}\":\"{}\"", json_escape(v)));
    }
    sd.push('}');
    let owner = if owner_id.is_empty() {
        String::new()
    } else {
        format!(
            ",\"Owner\":{{\"Id\":\"{}\",\"Type\":\"title_player_account\"}}",
            json_escape(owner_id)
        )
    };
    let md = member_data_json(owner_id, member_kv);
    let body = format!(
        "{{\"MaxPlayers\":{max},\"OwnerMigrationPolicy\":{owner_migration_policy},\"LobbyData\":{ld},\"SearchData\":{sd}{owner},\"MemberData\":{md}}}"
    );
    let (status, text) = http_json_status("POST", "/Lobby/CreateAndJoinLobby", &body)
        .ok_or(E_PF_SERVICE_UNEXPECTED)?;
    if status != 200 {
        log_line(&format!(
            "CreateAndJoinLobby REJECTED status={status} body={}",
            truncate_log(&text, 300)
        ));
        return Err(broker_error_code(status, &text));
    }
    let id = json_str(&text, "LobbyId").ok_or(E_PF_SERVICE_MALFORMED_RESPONSE)?;
    let conn = json_str(&text, "ConnectionString").unwrap_or_else(|| format!("lan.{id}"));
    let resolved_max = json_max_players(&text).max(max);
    let mut props = HashMap::new();
    apply_props(&mut props, lobby_data.clone());
    let mut sch = HashMap::new();
    apply_props(&mut sch, search.clone());
    Ok(Box::new(Lobby {
        id: CString::new(id).map_err(|_| E_PF_SERVICE_MALFORMED_RESPONSE)?,
        connection: CString::new(conn).map_err(|_| E_PF_SERVICE_MALFORMED_RESPONSE)?,
        owner: EntityKey {
            id: intern(if owner_id.is_empty() { "lan-user" } else { owner_id }),
            type_: intern("title_player_account"),
        },
        has_owner: !owner_id.is_empty(),
        max_players: resolved_max,
        props,
        search: sch,
        member_props: HashMap::new(),
        members: Vec::new(),
        membership_lock: 0,
        access_policy: 0,
        lock_known: false,
        announced: HashSet::new(),
        pending: false,
        left: false,
    }))
}

fn is_real_network_descriptor(s: &str) -> bool {
    s.starts_with("LAN1.") && s.len() > 5
}

fn lan1_network_id(desc: &str) -> Option<String> {
    let rest = desc.strip_prefix("LAN1.")?;
    let nid = rest.split('.').next().unwrap_or("");
    if nid.len() >= 8 {
        Some(nid.to_string())
    } else {
        None
    }
}

fn party_has_entity(nid: &str, eid: &str) -> Option<bool> {
    if nid.is_empty() || eid.is_empty() {
        return Some(false);
    }
    let path = format!("/party/peers?network_id={nid}");
    let text = http_json("GET", &path, "")?;
    Some(text.contains(&format!("\"entity_id\":\"{eid}\"")))
}

fn find_row_live(row: &str) -> bool {
    let id = json_str(row, "LobbyId").unwrap_or_default();
    if id.is_empty() {
        return false;
    }
    let body = format!("{{\"LobbyId\":\"{}\"}}", json_escape(&id));
    let Some((status, got)) = http_json_status("POST", "/Lobby/GetLobby", &body) else {
        // No response at all: do not hide a possibly-live session over a transient blip.
        log_line(&format!("FindLobbies probe id={id} no broker response (keeping row)"));
        return true;
    };
    if status != 200 {
        // A definitive answer from the broker (`LobbyNotFound` is a 404) must not be offered
        // to the player as a joinable session. This used to fail open because the error body
        // carries no LobbyData, which looked identical to "no descriptor yet".
        log_line(&format!(
            "FindLobbies drop id={id} broker status={status} body={}",
            truncate_log(&got, 200)
        ));
        return false;
    }
    let desc = json_obj(&got, "LobbyData")
        .get("network_descriptor")
        .cloned()
        .unwrap_or_default();
    let Some(nid) = lan1_network_id(&desc) else {
        log_line(&format!("FindLobbies drop id={id} no LAN1 descriptor yet"));
        return false;
    };
    let owner = json_obj(&got, "Owner")
        .get("Id")
        .cloned()
        .unwrap_or_default();
    match party_has_entity(&nid, &owner) {
        Some(false) => {
            log_line(&format!(
                "FindLobbies drop stale id={id} net={nid} owner={owner}"
            ));
            false
        }
        _ => true,
    }
}

fn lobby_ready_for_guest(lobby: &Lobby) -> bool {
    let Some(desc) = lobby.props.get("network_descriptor") else {
        return false;
    };
    if !is_real_network_descriptor(&desc.to_string_lossy()) {
        return false;
    }
    match lobby.props.get("invitation_identifier") {
        Some(inv) => {
            let s = inv.to_string_lossy();
            !s.is_empty() && s != "dummy"
        }
        None => false,
    }
}

unsafe fn emit_member_added(m: &mut Mp, lobby: *mut Lobby, member: EntityKey) {
    let id = read_cstr(member.id);
    if id.is_empty() || id == "owner" {
        return;
    }
    if !(*lobby).announced.insert(id.clone()) {
        return;
    }
    let sc2 = alloc_sc(0x28, 2);
    if sc2.is_null() {
        (*lobby).announced.remove(&id);
        return;
    }
    ptr::write_unaligned(sc2.add(8) as *mut *mut Lobby, lobby);
    ptr::write_unaligned(sc2.add(0x10) as *mut EntityKey, member);
    queue(m, sc2);
    debug_log(&format!("MemberAdded entity={id}"));
}

unsafe fn announce_lobby_members(m: &mut Mp, lobby: *mut Lobby) {
    let owner = (*lobby).owner;
    emit_member_added(m, lobby, owner);
    let members: Vec<EntityKey> = (*lobby).members.iter().copied().collect();
    for member in members {
        emit_member_added(m, lobby, member);
    }
}

/// Queue a PFLobbyUpdatedStateChange (type 7) built from a real delta.
///
/// Layout matches the real header and the exe handler (FUN_143B48800): lobby@+8,
/// ownerUpdated@+0x10, maxMembersUpdated@+0x11, accessPolicyUpdated@+0x12,
/// membershipLockUpdated@+0x13, search count/keys@+0x14/+0x18, lobby count/keys@+0x20/+0x28,
/// memberUpdateCount@+0x30, memberUpdates@+0x38. alloc_sc zeroes the rest, so the server
/// fields at +0x40.. stay "not updated".
///
/// `force_lock` is the join-completion guarantee: the membership lock must be populated by
/// the time JoinLobbyCompleted is delivered, and an Updated with membershipLockUpdated=true
/// is the only mechanism for that. Everywhere else the flag rides only the first Updated
/// after create/join (population) or a real lock-value change.
unsafe fn emit_updated(m: &mut Mp, lobby: *mut Lobby, mut delta: LobbyDelta, force_lock: bool) {
    if force_lock || !(*lobby).lock_known {
        delta.membership_lock_updated = true;
    }
    if delta.is_empty() {
        return;
    }
    let sc = alloc_sc(0x58, 7);
    if sc.is_null() {
        return;
    }
    ptr::write_unaligned(sc.add(8) as *mut *mut Lobby, lobby);
    ptr::write_unaligned(sc.add(0x10) as *mut u8, delta.owner_updated as u8);
    ptr::write_unaligned(sc.add(0x11) as *mut u8, delta.max_players_updated as u8);
    ptr::write_unaligned(sc.add(0x12) as *mut u8, delta.access_policy_updated as u8);
    ptr::write_unaligned(sc.add(0x13) as *mut u8, delta.membership_lock_updated as u8);
    let (sn, sk) = intern_cstr_list(&delta.search_keys);
    ptr::write_unaligned(sc.add(0x14) as *mut u32, sn);
    ptr::write_unaligned(sc.add(0x18) as *mut *const *const c_char, sk);
    let (ln, lk) = intern_cstr_list(&delta.lobby_keys);
    ptr::write_unaligned(sc.add(0x20) as *mut u32, ln);
    ptr::write_unaligned(sc.add(0x28) as *mut *const *const c_char, lk);
    let (mn, me) = member_update_entries(&delta.member_updates);
    ptr::write_unaligned(sc.add(0x30) as *mut u32, mn);
    ptr::write_unaligned(sc.add(0x38) as *mut *mut u8, me);
    (*lobby).lock_known = true;
    // Probe: one line per real Updated, with the exact flags/keys/entries the title will
    // see. This is what proves a host that only writes now receives type-7 changes.
    let members: Vec<String> = delta
        .member_updates
        .iter()
        .map(|u| {
            format!(
                "{}:conn={}:keys={}",
                read_cstr(u.member.id),
                u.connection_status_updated as u8,
                u.property_keys.len()
            )
        })
        .collect();
    debug_log(&format!(
        "LobbyUpdated type7 id={} owner={} max={} access={} lock={} search=[{}] lobby=[{}] members=[{}]",
        (*lobby).id.to_string_lossy(),
        delta.owner_updated as u8,
        delta.max_players_updated as u8,
        delta.access_policy_updated as u8,
        delta.membership_lock_updated as u8,
        delta.search_keys.join(","),
        delta.lobby_keys.join(","),
        members.join(", ")
    ));
    queue(m, sc);
}

unsafe fn emit_join_completed(m: &mut Mp, lobby: *mut Lobby, joiner: EntityKey, async_ctx: *mut c_void) {
    // The identity getters answer E_PF_OBJECT_STILL_PENDING until this completion is queued.
    (*lobby).pending = false;
    // Documented order (PFMultiplayerJoinLobby): MemberAdded, then Updated, then
    // JoinLobbyCompleted. MemberAdded seeds the exe's cached lobby handle (verified:
    // FUN_143B47C30 at 0x143B47C5A-0x143B47C68 stores SC+8 into ctx+0x1D0 when it is
    // null), which is the gate FUN_143B48800 checks at 0x143B48849 before it processes
    // the Updated; queueing the completion first would invert the doc and that gate.
    announce_lobby_members(m, lobby);
    emit_member_added(m, lobby, joiner);
    // The joining member's data and connection status are announced in the join Updated.
    // The exe only reaches GetMemberConnectionStatus for entries with a nonzero key
    // count (0x143B4894B), so carry the member's real keys when there are any.
    let mut delta = LobbyDelta::default();
    let mid = read_cstr(joiner.id);
    if let Some(mp) = (*lobby).member_props.get(&mid) {
        let mut keys: Vec<String> = mp.keys().cloned().collect();
        keys.sort();
        if !keys.is_empty() {
            delta.member_updates.push(MemberUpdate {
                member: joiner,
                connection_status_updated: true,
                property_keys: keys,
            });
        }
    }
    // Guaranteed before JoinLobbyCompleted: the membership lock is populated here.
    emit_updated(m, lobby, delta, true);
    let sc = alloc_sc(0x28, 1);
    if sc.is_null() {
        return;
    }
    ptr::write_unaligned(sc.add(4) as *mut i32, 0);
    // PlayFabReadLobbyProperties: +8 is joiner entity-id C string, +0x20 is lobby handle
    ptr::write_unaligned(sc.add(8) as *mut *const c_char, joiner.id);
    ptr::write_unaligned(sc.add(0x20) as *mut *mut Lobby, lobby);
    let _ = async_ctx;
    queue(m, sc);
    debug_log("JoinLobbyCompleted type1 lobby@+0x20");
}

fn post_join(
    conn: &str,
    joiner_id: &str,
    member_kv: &HashMap<String, String>,
) -> Result<Box<Lobby>, i32> {
    let member = if joiner_id.is_empty() {
        String::new()
    } else {
        format!(
            ",\"MemberEntity\":{{\"Id\":\"{}\",\"Type\":\"title_player_account\"}},\"MemberData\":{}",
            json_escape(joiner_id),
            member_data_json(joiner_id, member_kv)
        )
    };
    let body = format!(
        "{{\"ConnectionString\":\"{}\"{member}}}",
        json_escape(conn)
    );
    let (status, text) = http_json_status("POST", "/Lobby/JoinLobby", &body)
        .ok_or(E_PF_SERVICE_UNEXPECTED)?;
    if status != 200 {
        log_line(&format!(
            "JoinLobby REJECTED conn={conn} status={status} body={}",
            truncate_log(&text, 300)
        ));
        return Err(broker_error_code(status, &text));
    }
    let id = json_str(&text, "LobbyId").ok_or(E_PF_SERVICE_MALFORMED_RESPONSE)?;
    let (gstatus, got) = http_json_status("POST", "/Lobby/GetLobby", &format!("{{\"LobbyId\":\"{id}\"}}"))
        .ok_or(E_PF_SERVICE_UNEXPECTED)?;
    if gstatus != 200 {
        return Err(broker_error_code(gstatus, &got));
    }
    if let Some(mut l) = lobby_from_json(&got) {
        l.pending = true;
        return Ok(l);
    }
    Ok(Box::new(Lobby {
            id: CString::new(id.clone()).map_err(|_| E_PF_SERVICE_MALFORMED_RESPONSE)?,
            connection: CString::new(conn).map_err(|_| E_PF_SERVICE_MALFORMED_RESPONSE)?,
            owner: EntityKey {
                id: intern("owner"),
                type_: intern("title_player_account"),
            },
            has_owner: true,
            max_players: json_max_players(&got),
            props: HashMap::new(),
            search: HashMap::new(),
            member_props: HashMap::new(),
            members: Vec::new(),
            membership_lock: 0,
            access_policy: 0,
            lock_known: false,
            announced: HashSet::new(),
            pending: true,
            left: false,
        }))
}

fn member_props_json_for(lobby: &Lobby, eid: &str) -> Option<String> {
    let mp = lobby.member_props.get(eid)?;
    let extra: HashMap<String, String> = mp
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string_lossy().into_owned()))
        .collect();
    Some(member_data_json(eid, &extra))
}

/// `PFLobbyDataUpdate` scalar fields present in one PostUpdate. `None` means the title did not
/// include the field, so it must be omitted from the service request instead of resent with the
/// shim's current value (which could clobber a concurrent owner update).
#[derive(Default)]
struct PostUpdateScalars {
    owner: Option<EntityKey>,
    max_players: Option<u32>,
    access_policy: Option<u32>,
    membership_lock: Option<i32>,
}

fn post_update(
    lobby: &Lobby,
    search_delete: &[String],
    lobby_delete: &[String],
    member_target: Option<&str>,
    member_delete: &[String],
    scalars: &PostUpdateScalars,
) -> bool {
    let mut ld = String::from("{");
    for (i, (k, v)) in lobby.props.iter().enumerate() {
        if i > 0 {
            ld.push(',');
        }
        ld.push_str(&format!(
            "\"{k}\":\"{}\"",
            json_escape(&v.to_string_lossy())
        ));
    }
    ld.push('}');
    let mut sd = String::from("{");
    for (i, (k, v)) in lobby.search.iter().enumerate() {
        if i > 0 {
            sd.push(',');
        }
        sd.push_str(&format!(
            "\"{k}\":\"{}\"",
            json_escape(&v.to_string_lossy())
        ));
    }
    sd.push('}');
    let target = member_target
        .map(|s| s.to_string())
        .or_else(|| {
            let e = local_entity_id();
            if e.is_empty() {
                None
            } else {
                Some(e)
            }
        });
    let md = match &target {
        Some(eid) => member_props_json_for(lobby, eid)
            .map(|s| {
                if member_target.is_some() {
                    format!(
                        ",\"MemberData\":{s},\"MemberEntity\":{{\"Id\":\"{}\",\"Type\":\"title_player_account\"}}",
                        json_escape(eid)
                    )
                } else {
                    format!(",\"MemberData\":{s}")
                }
            })
            .unwrap_or_default(),
        None => String::new(),
    };
    // The genuine SDK maps null property values to the documented *ToDelete arrays and forwards
    // the PFLobbyDataUpdate scalars; omitting them left deleted keys stored and scalar updates
    // invisible to the service and to the peers.
    let mut extras = String::new();
    if !search_delete.is_empty() {
        extras.push_str(&format!(
            ",\"SearchDataToDelete\":[{}]",
            q_str_list(search_delete)
        ));
    }
    if !lobby_delete.is_empty() {
        extras.push_str(&format!(
            ",\"LobbyDataToDelete\":[{}]",
            q_str_list(lobby_delete)
        ));
    }
    if let Some(o) = &scalars.owner {
        extras.push_str(&format!(
            ",\"Owner\":{{\"Id\":\"{}\",\"Type\":\"{}\"}}",
            json_escape(&read_cstr(o.id)),
            json_escape(&read_cstr(o.type_))
        ));
    }
    if let Some(m) = scalars.max_players {
        extras.push_str(&format!(",\"MaxPlayers\":{m}"));
    }
    if let Some(a) = scalars.access_policy {
        extras.push_str(&format!(",\"AccessPolicy\":\"{}\"", access_policy_name(a)));
    }
    if let Some(k) = scalars.membership_lock {
        extras.push_str(&format!(
            ",\"MembershipLock\":\"{}\"",
            if k == 1 { "Locked" } else { "Unlocked" }
        ));
    }
    if member_target.is_some() && !member_delete.is_empty() {
        extras.push_str(&format!(
            ",\"MemberDataToDelete\":[{}]",
            q_str_list(member_delete)
        ));
    }
    let body = format!(
        "{{\"LobbyId\":\"{}\",\"LobbyData\":{ld},\"SearchData\":{sd}{md}{extras}}}",
        lobby.id.to_string_lossy()
    );
    match http_json_status("POST", "/Lobby/UpdateLobby", &body) {
        Some((200, _)) => true,
        Some((status, resp)) => {
            log_line(&format!(
                "UpdateLobby REJECTED id={} status={status} body={}",
                lobby.id.to_string_lossy(),
                truncate_log(&resp, 200)
            ));
            false
        }
        None => {
            log_line(&format!(
                "UpdateLobby no broker response id={}",
                lobby.id.to_string_lossy()
            ));
            false
        }
    }
}

/// Snapshot of the fields an Updated diffs. Cheap shallow clones of the small maps.
fn snapshot(l: &Lobby) -> LobbySnapshot {
    LobbySnapshot {
        owner: l.owner,
        has_owner: l.has_owner,
        max_players: l.max_players,
        access_policy: l.access_policy,
        membership_lock: l.membership_lock,
        props: l.props.clone(),
        search: l.search.clone(),
        member_props: l.member_props.clone(),
    }
}

/// Keys whose values were added, changed or removed between two property bags.
fn changed_keys(before: &HashMap<String, CString>, after: &HashMap<String, CString>) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for (k, v) in after {
        match before.get(k) {
            Some(old) if old.as_c_str() == v.as_c_str() => {}
            _ => keys.push(k.clone()),
        }
    }
    for k in before.keys() {
        if !after.contains_key(k) {
            keys.push(k.clone());
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

/// # Safety
/// Both keys hold game- or shim-owned C strings; `read_cstr` null-checks and caps its scan, so
/// a damaged pointer cannot fault. Identity is compared by string value, not by address.
unsafe fn entity_eq(a: &EntityKey, b: &EntityKey) -> bool {
    read_cstr(a.id) == read_cstr(b.id) && read_cstr(a.type_) == read_cstr(b.type_)
}

/// Diff the lobby against a snapshot taken before an apply/refresh. Every flag and key list
/// is derived from a real difference, so an unchanged lobby yields an empty delta and the
/// caller emits nothing (a spurious change is as bad as a missing one).
unsafe fn diff_lobby(before: &LobbySnapshot, after: &Lobby) -> LobbyDelta {
    let mut d = LobbyDelta::default();
    d.owner_updated = before.has_owner != after.has_owner || !entity_eq(&before.owner, &after.owner);
    d.max_players_updated = before.max_players != after.max_players;
    d.access_policy_updated = before.access_policy != after.access_policy;
    d.membership_lock_updated = before.membership_lock != after.membership_lock;
    d.lobby_keys = changed_keys(&before.props, &after.props);
    d.search_keys = changed_keys(&before.search, &after.search);
    let empty = HashMap::new();
    for (eid, props) in &after.member_props {
        let new_member = !before.member_props.contains_key(eid);
        let bp = before.member_props.get(eid).unwrap_or(&empty);
        let keys = changed_keys(bp, props);
        // A newly seen member is announced even without property keys: the member's
        // connection status just became known, which is what connectionStatusUpdated
        // reports. A known member is announced only for real property differences.
        if new_member || !keys.is_empty() {
            let member = after
                .members
                .iter()
                .find(|m| read_cstr(m.id) == *eid)
                .copied()
                .unwrap_or_else(|| EntityKey {
                    id: intern(eid),
                    type_: intern("title_player_account"),
                });
            d.member_updates.push(MemberUpdate {
                member,
                connection_status_updated: new_member,
                property_keys: keys,
            });
        }
    }
    d
}

fn refresh_lobby(lobby: &mut Lobby) -> (Vec<String>, bool, LobbyDelta) {
    // Entity ids that were announced (type 2) and are no longer in the refreshed roster. PF-02: the
    // exe has a live MemberRemoved (type 4) handler, but we never emitted one, so a player who left
    // stayed visible in the other peer's session forever.
    let mut departed: Vec<String> = Vec::new();
    let mut delta = LobbyDelta::default();
    let body = format!("{{\"LobbyId\":\"{}\"}}", lobby.id.to_string_lossy());
    // PF-03: a definitive "no such lobby" is terminal. The exe's type-10 (Disconnected) handler
    // reads lobby@+8, calls PFLobbyGetLobbyId and clears ctx+0x1d0 — i.e. it is the state change
    // that makes the game accept the lobby is gone. A mere no-response is transient, not terminal.
    let (status, text) = match http_json_status("POST", "/Lobby/GetLobby", &body) {
        Some(v) => v,
        None => return (departed, false, delta),
    };
    if status != 200 {
        return (departed, true, delta);
    }
    if let Some(mut fresh) = lobby_from_json(&text) {
        for (eid, mp) in &lobby.member_props {
            let entry = fresh.member_props.entry(eid.clone()).or_insert_with(HashMap::new);
            for (k, v) in mp {
                // Server MemberData wins when present (Steam KV). Local fills gaps only.
                entry.entry(k.clone()).or_insert_with(|| v.clone());
            }
            ensure_member_props(entry, eid);
        }
        for (eid, mp) in fresh.member_props.iter_mut() {
            ensure_member_props(mp, eid);
        }
        let before = snapshot(lobby);
        lobby.props = fresh.props;
        lobby.search = fresh.search;
        lobby.connection = fresh.connection;
        lobby.owner = fresh.owner;
        lobby.has_owner = fresh.has_owner;
        lobby.max_players = fresh.max_players;
        lobby.access_policy = fresh.access_policy;
        lobby.membership_lock = fresh.membership_lock;
        if fresh.members.is_empty() && !lobby.members.is_empty() {
            debug_log(&format!(
                "GetLobby Members empty id={}; keep local n={}",
                lobby.id.to_string_lossy(),
                lobby.members.len()
            ));
        } else {
            lobby.members = fresh.members;
            lobby.member_props = fresh.member_props;
        }
        // PF-02: diff the announced set against the roster we just refreshed. Anyone we announced
        // who is no longer a member has left (or was dropped by the broker) and needs a Type 4 so
        // the game removes their character. The exe's handler matches ctx+0x1d0 against SC+8 and
        // reads the EntityKey at SC+0x10, so both must be what we used for the other lobby SCs.
        if !lobby.members.is_empty() || !lobby.announced.is_empty() {
            let live: std::collections::HashSet<String> = lobby
                .members
                .iter()
                .map(|k| read_cstr(k.id))
                .collect();
            departed = lobby
                .announced
                .iter()
                .filter(|e| !live.contains(*e))
                .cloned()
                .collect();
        }
        delta = unsafe { diff_lobby(&before, lobby) };
    }
    (departed, false, delta)
}

#[no_mangle]
pub unsafe extern "C" fn PFMultiplayerInitialize(
    title_id: *const c_char,
    handle: *mut *mut c_void,
) -> i32 {
    if handle.is_null() {
        return E_FAIL;
    }
    if g().lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        // Genuine 0x1800426b0 rejects a second live instance with 0x89236401.
        return E_PF_INSTANCE_ALREADY_EXISTS;
    }
    let title = CString::new(read_cstr(title_id)).unwrap_or_else(|_| CString::new("lan").unwrap());
    debug_log(&format!("PFMultiplayerInitialize title={}", title.to_string_lossy()));
    let mut boxed = Box::new(Mp {
        title,
        token: None,
        entity: None,
        lobbies: Vec::new(),
        pending: VecDeque::new(),
        in_flight: Vec::new(),
        batch_outstanding: false,
        last_poll: Instant::now() - Duration::from_secs(10),
        pending_joins: Vec::new(),
        start_state: StateLog::new(),
        finish_state: StateLog::new(),
    });
    *handle = boxed.as_mut() as *mut Mp as *mut c_void;
    probe_snapshot(&boxed);
    *g().lock().unwrap() = Some(boxed);
    start_queue_heartbeat();
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFMultiplayerUninitialize(handle: *mut c_void) -> i32 {
    if !mp_ready(handle) {
        return E_PF_NOT_INITIALIZED;
    }
    *g().lock().unwrap_or_else(|e| e.into_inner()) = None;
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFMultiplayerSetEntityToken(
    handle: *mut c_void,
    entity: *const EntityKey,
    token: *const c_char,
) -> i32 {
    if !mp_ready(handle) {
        return E_PF_NOT_INITIALIZED;
    }
    if entity.is_null()
        || (*entity).id.is_null()
        || (*entity).type_.is_null()
        || read_cstr((*entity).id).is_empty()
        || read_cstr((*entity).type_).is_empty()
    {
        return E_PF_ENTITY_KEY_MALFORMED;
    }
    if token.is_null() || read_cstr(token).is_empty() {
        return E_PF_ENTITY_TOKEN_MALFORMED;
    }
    let tok = read_cstr(token);
    debug_log(&format!("SetEntityToken len={}", tok.len()));
    set_local_identity(entity, &tok);
    with_mp(
        |m| {
            m.token = CString::new(tok).ok();
            if !entity.is_null() {
                m.entity = Some(unsafe { intern_entity(entity) });
            }
        },
        (),
    );
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFMultiplayerCreateAndJoinLobby(
    handle: *mut c_void,
    creator: *const EntityKey,
    create_cfg: *const u8,
    join_cfg: *const u8,
    async_ctx: *mut c_void,
    out_lobby: *mut *mut c_void,
) -> i32 {
    if !mp_ready(handle) {
        return E_PF_NOT_INITIALIZED;
    }
    let max = if create_cfg.is_null() {
        8
    } else {
        ptr::read_unaligned(create_cfg as *const u32).clamp(1, 32)
    };
    // PFLobbyCreateConfiguration: +0 maxMemberCount, +4 ownerMigrationPolicy,
    // +8 accessPolicy. The broker stores accessPolicy only when its create body carries
    // it; the shim mirrors the create request locally until the first GetLobby refresh.
    // ownerMigrationPolicy cannot change after create and is what decides owner handling
    // when the owner leaves, so it is forwarded to the broker with the create body.
    let owner_migration_policy = if create_cfg.is_null() {
        0
    } else {
        ptr::read_unaligned(create_cfg.add(4) as *const u32).min(3)
    };
    let access_policy = if create_cfg.is_null() {
        0
    } else {
        ptr::read_unaligned(create_cfg.add(8) as *const u32).min(2)
    };
    // PFLobbyCreateConfiguration after 3 dwords: searchPropertyCount then two ptrs...
    // Layout used by this exe (see PlayFabCreateAndJoinLobby): max, policies, then counts/pointers.
    let mut lobby_data = HashMap::new();
    let mut search = HashMap::new();
    if !create_cfg.is_null() {
        // try documented layout: u32 max, u32 ownerPolicy, u32 access, u32 searchCount, keys*, vals*, u32 lobbyCount, keys*, vals*
        let search_count = ptr::read_unaligned(create_cfg.add(12) as *const u32);
        let search_keys = ptr::read_unaligned(create_cfg.add(16) as *const *const *const c_char);
        let search_vals = ptr::read_unaligned(create_cfg.add(24) as *const *const *const c_char);
        search = kv_list(search_count, search_keys, search_vals);
        let lobby_count = ptr::read_unaligned(create_cfg.add(32) as *const u32);
        let lobby_keys = ptr::read_unaligned(create_cfg.add(40) as *const *const *const c_char);
        let lobby_vals = ptr::read_unaligned(create_cfg.add(48) as *const *const *const c_char);
        lobby_data = kv_list(lobby_count, lobby_keys, lobby_vals);
    }
    let owner = intern_entity(creator);
    let member_kv = join_cfg_kv(join_cfg);
    debug_log(&format!(
        "CreateAndJoinLobby max={max} owner_policy={owner_migration_policy} lobby_props={} search={} member_props={}",
        lobby_data.len(),
        search.len(),
        member_kv.len()
    ));
    let mut lobby = match post_create(
        max,
        owner_migration_policy,
        &lobby_data,
        &search,
        &read_cstr(owner.id),
        &member_kv,
    ) {
        Ok(l) => l,
        Err(code) => {
            log_line(&format!("CreateAndJoinLobby failed code=0x{code:08X}"));
            if !out_lobby.is_null() {
                *out_lobby = ptr::null_mut();
            }
            return code;
        }
    };
    lobby.owner = owner;
    lobby.has_owner = true;
    lobby.access_policy = access_policy;
    lobby.members.push(owner);
    let mid = read_cstr(owner.id);
    let mut mp = HashMap::new();
    apply_props(&mut mp, member_kv);
    ensure_member_props(&mut mp, &mid);
    debug_log(&format!(
        "CreateAndJoinLobby member_props={} id={mid}",
        mp.len()
    ));
    debug_log(&format!(
        "CreateAndJoinLobby data search_keys=[{}] lobby_keys=[{}]",
        map_preview(&lobby.search, 400),
        map_preview(&lobby.props, 400)
    ));
    // NUMBER_KEY9.md §7 / SERVICE_SURFACE_DIFF §4.2: whether the open/joinable flag and the join
    // row's comment1/comment2 prerequisites ever exist is decided from this create payload, and
    // the 400-char preview above truncates the string_key5/string_key6 JSON. Dump each distinct
    // payload once, in full, so one run answers it without a probe.
    {
        let dump = format!(
            "CreateAndJoinLobby full search=[{}] lobby=[{}]",
            map_full(&lobby.search, 4096),
            map_full(&lobby.props, 4096)
        );
        let seen = CREATE_PAYLOAD_LOG.get_or_init(|| Mutex::new(HashSet::new()));
        if seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(dump.clone())
        {
            debug_log(&dump);
        }
    }
    lobby.member_props.insert(mid, mp);
    let lp = Box::into_raw(lobby);
    with_mp(
        |m| {
            m.lobbies.push(lp);
            // Documented order (PFMultiplayerCreateAndJoinLobby): MemberAdded, then
            // CreateAndJoinLobbyCompleted. MemberAdded seeds the exe's cached handle.
            emit_member_added(m, lp, owner);
            let sc = alloc_sc(0x20, 0);
            if !sc.is_null() {
                ptr::write_unaligned(sc.add(4) as *mut i32, 0);
                ptr::write_unaligned(sc.add(8) as *mut *mut c_void, async_ctx);
                ptr::write_unaligned(sc.add(0x10) as *mut *mut Lobby, lp);
                queue(m, sc);
            }
        },
        (),
    );
    if !out_lobby.is_null() {
        *out_lobby = lp as *mut c_void;
    }
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFMultiplayerJoinLobby(
    handle: *mut c_void,
    joiner: *const EntityKey,
    connection: *const c_char,
    join_cfg: *const u8,
    async_ctx: *mut c_void,
    out_lobby: *mut *mut c_void,
) -> i32 {
    if !mp_ready(handle) {
        return E_PF_NOT_INITIALIZED;
    }
    let conn = read_cstr(connection);
    let joiner_ek = intern_entity(joiner);
    let member_kv = join_cfg_kv(join_cfg);
    debug_log(&format!(
        "JoinLobby conn={conn} member_props={}",
        member_kv.len()
    ));
    let mut lobby = match post_join(&conn, &read_cstr(joiner_ek.id), &member_kv) {
        Ok(l) => l,
        Err(code) => {
            log_line(&format!("JoinLobby failed conn={conn} code=0x{code:08X}"));
            if !out_lobby.is_null() {
                *out_lobby = ptr::null_mut();
            }
            return code;
        }
    };
    debug_log(&format!(
        "JoinLobby lobby id={} search_keys=[{}] lobby_keys=[{}]",
        lobby.id.to_string_lossy(),
        map_preview(&lobby.search, 500),
        map_preview(&lobby.props, 300)
    ));
    let mid = read_cstr(joiner_ek.id);
    if !lobby.members.iter().any(|m| read_cstr(m.id) == mid) {
        lobby.members.push(joiner_ek);
    }
    let mut mp = HashMap::new();
    apply_props(&mut mp, member_kv);
    ensure_member_props(&mut mp, &mid);
    lobby.member_props.insert(mid, mp);
    let lp = Box::into_raw(lobby);
    if !out_lobby.is_null() {
        *out_lobby = lp as *mut c_void;
    }
    with_mp(
        |m| {
            m.lobbies.push(lp);
            if lobby_ready_for_guest(&*lp) {
                emit_join_completed(m, lp, joiner_ek, async_ctx);
            } else {
                debug_log("JoinLobby waiting for network_descriptor");
                m.pending_joins.push(PendingJoin {
                    lobby: lp,
                    joiner: joiner_ek,
                    async_ctx,
                    emitted: false,
                });
            }
        },
        (),
    );
    S_OK
}

#[repr(C)]
struct SearchResult {
    lobby_id: *const c_char,
    connection: *const c_char,
    owner: *const EntityKey,
    max_member: u32,
    current_member: u32,
    search_count: u32,
    _pad0: u32,
    search_keys: *const *const c_char,
    search_vals: *const *const c_char,
    friend_count: u32,
    _pad1: u32,
    friends: *const EntityKey,
    membership_lock: i32,
    _pad2: u32,
}

const _: () = assert!(std::mem::size_of::<SearchResult>() == 0x50);

/// The genuine `PFLobbySearchFriendsFilter` ABI (PlayFabMultiplayer.h): two bools, then the
/// Xbox friends token pointer. The bools are read as `u8` so a non-0/1 byte from the exe can
/// never be UB.
#[repr(C)]
struct LobbySearchFriendsFilterRaw {
    include_steam_friends: u8,
    include_facebook_friends: u8,
    include_xbox_friends_token: *const c_char,
}

/// The genuine `PFLobbySearchConfiguration` ABI. Verified against the real
/// PlayFabMultiplayerWin.dll.ms 1.8.2506.05002 (its FindLobbies path reads +0x00 friendsFilter
/// pointer, +0x08 filterString, +0x10 sortString, +0x18 `const uint32_t*` result count) and
/// against the exe's call site (0x143B51ABE/0x143B51AD0 writes exactly those four slots).
#[repr(C)]
struct LobbySearchConfigurationRaw {
    friends_filter: *const LobbySearchFriendsFilterRaw,
    filter_string: *const c_char,
    sort_string: *const c_char,
    client_search_result_count: *const u32,
}

#[no_mangle]
pub unsafe extern "C" fn PFMultiplayerFindLobbies(
    handle: *mut c_void,
    _entity: *const EntityKey,
    search_config: *const c_void,
    async_ctx: *mut c_void,
) -> i32 {
    if !mp_ready(handle) {
        return E_PF_NOT_INITIALIZED;
    }
    let cfg = if search_config.is_null() {
        None
    } else {
        Some(&*(search_config as *const LobbySearchConfigurationRaw))
    };
    // Forward the title's search configuration verbatim; the broker evaluates it. The filter
    // string is never reordered, trimmed or rewritten here.
    let filter = cfg.map(|c| read_cstr(c.filter_string)).unwrap_or_default();
    let sort = cfg.map(|c| read_cstr(c.sort_string)).unwrap_or_default();
    let count = cfg.and_then(|c| {
        if c.client_search_result_count.is_null() {
            None
        } else {
            Some(*c.client_search_result_count)
        }
    });
    let friends = cfg.and_then(|c| {
        if c.friends_filter.is_null() {
            None
        } else {
            Some(&*c.friends_filter)
        }
    });
    let friends_log = friends
        .map(|f| {
            format!(
                "steam={} facebook={} xbox_token_len={}",
                f.include_steam_friends != 0,
                f.include_facebook_friends != 0,
                read_cstr(f.include_xbox_friends_token).chars().count()
            )
        })
        .unwrap_or_else(|| "none".to_string());
    // Log the raw filter once per search so a future mismatch is visible in the log.
    debug_log(&format!(
        "FindLobbies filter=\"{}\" sort=\"{}\" count={} friends={}",
        preview_str(&filter, 600),
        preview_str(&sort, 100),
        count
            .map(|c| c.to_string())
            .unwrap_or_else(|| "none".to_string()),
        friends_log
    ));
    let eid = entity_id_of(_entity);
    let mut body = String::from("{");
    let mut sep = "";
    if !eid.is_empty() {
        body.push_str(&format!("{sep}\"EntityId\":\"{}\"", json_escape(&eid)));
        sep = ",";
    }
    if !filter.is_empty() {
        body.push_str(&format!("{sep}\"Filter\":\"{}\"", json_escape(&filter)));
        sep = ",";
    }
    if !sort.is_empty() {
        body.push_str(&format!("{sep}\"Sort\":\"{}\"", json_escape(&sort)));
        sep = ",";
    }
    if let Some(c) = count {
        body.push_str(&format!("{sep}\"ClientSearchResultCount\":{c}"));
        sep = ",";
    }
    if let Some(f) = friends {
        body.push_str(&format!(
            "{sep}\"FriendsFilter\":{{\"IncludeSteamFriends\":{},\"IncludeFacebookFriends\":{},\"IncludeXboxFriendsToken\":\"{}\"}}",
            f.include_steam_friends != 0,
            f.include_facebook_friends != 0,
            json_escape(&read_cstr(f.include_xbox_friends_token))
        ));
    }
    body.push('}');
    let (status, text) =
        http_json_status("POST", "/Lobby/FindLobbies", &body).unwrap_or((0, String::new()));
    let rows: Vec<String> = if status == 200 {
        json_arr_objects(&text, "Lobbies")
            .into_iter()
            .filter(|row| find_row_live(row))
            .collect()
    } else {
        Vec::new()
    };
    // The genuine FindLobbies is asynchronous: it returns S_OK and reports the service
    // failure in FindLobbiesCompleted.result. Keep that shape instead of faking a
    // zero-result success when the broker is down or refuses.
    let result_code = if status == 200 {
        0
    } else {
        broker_error_code(status, &text)
    };
    debug_log(&format!("FindLobbies n={}", rows.len()));
    for row in rows.iter().take(4) {
        let sd = json_obj(row, "SearchData");
        let mut keys: Vec<String> = sd
            .iter()
            .map(|(k, v)| format!("{k}={}", preview_str(v, 48)))
            .collect();
        keys.sort();
        debug_log(&format!(
            "FindLobbies row {} search=[{}]",
            json_str(row, "LobbyId").unwrap_or_default(),
            preview_str(&keys.join(", "), 500)
        ));
    }
    with_mp(
        |m| {
            let n = rows.len().min(16);
            let results = if n == 0 {
                ptr::null_mut()
            } else {
                let layout = std::alloc::Layout::array::<SearchResult>(n).unwrap();
                std::alloc::alloc_zeroed(layout) as *mut SearchResult
            };
            for (i, row) in rows.iter().take(n).enumerate() {
                let id = json_str(row, "LobbyId").unwrap_or_default();
                let conn = json_str(row, "ConnectionString").unwrap_or_default();
                let max_member = json_max_players(row);
                let cur: u32 = json_str(row, "CurrentPlayers")
                    .and_then(|s| s.parse().ok())
                    .or_else(|| {
                        row.split("\"CurrentPlayers\":")
                            .nth(1)
                            .and_then(|s| {
                                s.chars()
                                    .take_while(|c| c.is_ascii_digit())
                                    .collect::<String>()
                                    .parse()
                                    .ok()
                            })
                    })
                    .unwrap_or(1)
                    .clamp(1, max_member);
                let search = json_obj(row, "SearchData");
                let (search_count, search_keys, search_vals) = intern_kv_arrays(&search);
                let r = SearchResult {
                    lobby_id: intern(&id),
                    connection: intern(&conn),
                    owner: intern_owner_key(row),
                    max_member,
                    current_member: cur,
                    search_count,
                    _pad0: 0,
                    search_keys,
                    search_vals,
                    friend_count: 0,
                    _pad1: 0,
                    friends: ptr::null(),
                    membership_lock: 0,
                    _pad2: 0,
                };
                ptr::write(results.add(i), r);
            }
            let sc = alloc_sc(0x38, 12);
            if !sc.is_null() {
                ptr::write_unaligned(sc.add(4) as *mut i32, result_code);
                let searcher = intern_entity(_entity);
                ptr::write_unaligned(sc.add(8) as *mut EntityKey, searcher);
                ptr::write_unaligned(sc.add(0x18) as *mut *mut c_void, async_ctx);
                ptr::write_unaligned(sc.add(0x20) as *mut u32, n as u32);
                ptr::write_unaligned(sc.add(0x28) as *mut *mut SearchResult, results);
                queue(m, sc);
            }
        },
        (),
    );
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFMultiplayerStartProcessingLobbyStateChanges(
    handle: *mut c_void,
    count: *mut u32,
    changes: *mut *mut *mut u8,
) -> i32 {
    if count.is_null() || changes.is_null() {
        return E_FAIL;
    }
    if !mp_ready(handle) {
        *count = 0;
        *changes = ptr::null_mut();
        return E_PF_NOT_INITIALIZED;
    }
    with_mp(
        |m| {
            Q_STARTS.fetch_add(1, Ordering::Relaxed);
            if m.last_poll.elapsed() > Duration::from_millis(250) {
                m.last_poll = Instant::now();
                let lps: Vec<*mut Lobby> = m.lobbies.clone();
                for lp in lps {
                    // Two users: type-7 LobbyUpdated fires only for a real delta, while
                    // type-10 (PF-03) fires on the `gone` flag itself, because refresh_lobby returns
                    // gone=true from an early return that carries no delta (OC-5).
                    let (departed, gone, delta) = refresh_lobby(&mut *lp);
                    for eid in departed {
                        // PFLobbyMemberRemovedStateChange: result@+4, lobby@+8, member@+0x10,
                        // reason@+0x20. Exe handler 0x143B48360 reads +8 and +0x10 (verified).
                        let sc = alloc_sc(0x28, 4);
                        if sc.is_null() {
                            break;
                        }
                        ptr::write_unaligned(sc.add(4) as *mut i32, 0);
                        ptr::write_unaligned(sc.add(8) as *mut *mut Lobby, lp);
                        let ek = EntityKey {
                            id: intern(&eid),
                            type_: intern("title_player_account"),
                        };
                        ptr::write_unaligned(sc.add(0x10) as *mut EntityKey, ek);
                        ptr::write_unaligned(sc.add(0x20) as *mut u32, 0);
                        queue(m, sc);
                        // Allow a later re-join to be announced again.
                        (*lp).announced.remove(&eid);
                        debug_log(&format!("MemberRemoved entity={eid}"));
                    }
                    // PF-03 re-fix: `refresh_lobby` returns gone=true from its early non-200 return,
                    // which carries no delta — so gating this on a change made the emission
                    // unreachable (ORDINAL_CHAIN_AUDIT OC-5). Emit on `gone` itself; the retain
                    // below guarantees it fires at most once per lobby.
                    if gone {
                        // PFLobbyDisconnectedStateChange: lobby@+8 (the exe's handler reads only
                        // this, then clears its cached PFLobbyHandle at ctx+0x1d0).
                        (*lp).left = true;
                        let sc = alloc_sc(0x18, 10);
                        if !sc.is_null() {
                            ptr::write_unaligned(sc.add(4) as *mut i32, 0);
                            ptr::write_unaligned(sc.add(8) as *mut *mut Lobby, lp);
                            queue(m, sc);
                        }
                        m.lobbies.retain(|p| *p != lp);
                        log_line(&format!(
                            "LobbyDisconnected id={} (broker returned non-200); queued type 10, stopped polling",
                            (*lp).id.to_string_lossy()
                        ));
                    }
                    let pending = m
                        .pending_joins
                        .iter()
                        .any(|pj| pj.lobby == lp && !pj.emitted);
                    // While a join is pending, the join-completion path owns the delivery
                    // order (MemberAdded -> Updated -> JoinLobbyCompleted); the poll must not
                    // race a type-7 in front of it. A disconnected lobby gets its type-10
                    // instead of an Updated.
                    if !pending {
                        // The exe's Updated handler gates on the cached lobby handle
                        // (ctx+0x1d0 == SC+8); MemberAdded seeds it, so members are announced
                        // first (idempotent via `announced`).
                        announce_lobby_members(m, lp);
                        if !gone {
                            emit_updated(m, lp, delta, false);
                        }
                    }
                }
                let mut ready = Vec::new();
                for (i, pj) in m.pending_joins.iter().enumerate() {
                    if !pj.emitted && lobby_ready_for_guest(&*pj.lobby) {
                        ready.push(i);
                    }
                }
                for i in ready.into_iter().rev() {
                    let pj = m.pending_joins.remove(i);
                    emit_join_completed(m, pj.lobby, pj.joiner, pj.async_ctx);
                }
            }
            // Only build a batch when nothing is outstanding: if the title calls Start again
            // before Finish, `in_flight` still belongs to it, so leave it untouched and re-return
            // it below. Queued changes stay in `pending` for the next real batch.
            if !m.batch_outstanding {
            m.in_flight.clear();
            while let Some(p) = m.pending.pop_front() {
                if BATCH_CAP_ENABLED && m.in_flight.len() >= BATCH_CAP {
                    // CHANGE 2(a), PF-15/D4: the cap is off by default because the SDK returns the
                    // whole batch. The old loop dropped the change it had just popped; when the cap
                    // is switched back on, requeue it instead and say so.
                    log_throttled(
                        "in_flight_cap",
                        &format!(
                            "pfqueue batch_cap hit cap={BATCH_CAP} pending={} (change stays queued)",
                            m.pending.len() + 1
                        ),
                    );
                    m.pending.push_front(p);
                    break;
                }
                m.in_flight.push(p);
            }
                m.batch_outstanding = !m.in_flight.is_empty();
                if m.batch_outstanding {
                    // CHANGE 1b: cumulative nonzero-batch count for the hb summary.
                    Q_BATCHES_NONZERO.fetch_add(1, Ordering::Relaxed);
                }
                if m.in_flight.len() > BATCH_LARGE {
                    // Throughput watch (PF-15): a batch this large is reported even though the
                    // whole-batch contract made it legal, so a real backlog cannot hide in silence.
                    log_throttled(
                        "batch_large",
                        &format!(
                            "pfqueue batch_large n={} pending={}",
                            m.in_flight.len(),
                            m.pending.len()
                        ),
                    );
                }
            } else {
                // CHANGE 2(c): batch_outstanding must never stay true silently. The cumulative
                // repeat count is part of the line, so a latch is visible even through the throttle.
                let repeats = Q_LATCHED.fetch_add(1, Ordering::Relaxed) + 1;
                log_throttled(
                    "batch_outstanding",
                    &format!(
                        "pfqueue start LATCHED (Start called while a batch is outstanding; re-returning it unchanged) n={} pending={} inflight={} outstanding=1 repeat_calls={repeats}",
                        m.in_flight.len(),
                        m.pending.len(),
                        m.in_flight.len()
                    ),
                );
            }
            *count = m.in_flight.len() as u32;
            *changes = if m.in_flight.is_empty() {
                ptr::null_mut()
            } else {
                m.in_flight.as_mut_ptr()
            };
            probe_snapshot(m);
            // CHANGE 1/1b: the state line still reports the batch Start hands out and the depth
            // it leaves behind, including n=0, but only when the tuple moved or the ~5 s heartbeat
            // is due. The old every-call line formatted ~12k identical n=0 pumps and charged the
            // game's tick thread an allocation for each.
            let pending = m.pending.len();
            let inflight = m.in_flight.len();
            let mode = m
                .start_state
                .due((*count as usize, pending, inflight, m.batch_outstanding));
            // Verbose trace: per-change detail is debug-only; the 30 s state heartbeat keeps a
            // stuck tuple visible by default (and `pfqueue hb` carries the cumulative counters).
            if mode != 0 && debug_enabled() {
                log_line(&format!(
                    "pfqueue start n={} pending={pending} inflight={inflight} outstanding={}{}",
                    *count,
                    m.batch_outstanding as u8,
                    if mode == 1 { " changed=1" } else { " hb=1" }
                ));
            }
        },
        (),
    );
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFMultiplayerFinishProcessingLobbyStateChanges(
    handle: *mut c_void,
    count: u32,
    changes: *mut *mut u8,
) -> i32 {
    // Reclaim the batch: the title has returned it, so this is the point at which the library may
    // release the array and its changes, and at which `in_flight` becomes reusable.
    //
    // Genuine PFMultiplayerFinishProcessingLobbyStateChanges validates the handle first and
    // returns 0x89236400 for a null/foreign one.
    if !mp_ready(handle) {
        return E_PF_NOT_INITIALIZED;
    }
    // CHANGE 2(b), CRASH_8E8 §3.4: Finish used to clear `in_flight` unconditionally and with no
    // guard, so a re-entrant or early Finish mutated the array while the exe could still be
    // iterating it. The contract is exact — Finish returns the same count/pointer pair Start handed
    // out — so anything else is logged loudly instead of being reclaimed silently. A refused Finish
    // leaves `batch_outstanding` set on purpose: the repeated-Start LATCHED line below then reports
    // the latch with a counter, so this can never decay into the silent stall of FIX_BACKLOG §8.1.
    with_mp(
        |m| {
            Q_FINISHES.fetch_add(1, Ordering::Relaxed);
            let handed = m.in_flight.len();
            let handed_ptr = m.in_flight.as_mut_ptr();
            if !m.batch_outstanding {
                if count == 0 {
                    // Normal empty pump: Start handed out nothing, so there is nothing to reclaim.
                    // CHANGE 1b: same change gate as Start; identical empty pumps are silent
                    // between the ~5 s heartbeats.
                    let pending = m.pending.len();
                    let mode = m.finish_state.due((0, pending, 0, false));
                    if mode != 0 && debug_enabled() {
                        log_line(&format!(
                            "pfqueue finish count=0 reclaimed=0 pending={pending} inflight=0 outstanding=0{}",
                            if mode == 1 { " changed=1" } else { " hb=1" }
                        ));
                    }
                } else {
                    let strays = Q_STRAY.fetch_add(1, Ordering::Relaxed) + 1;
                    log_line(&format!(
                        "pfqueue finish STRAY-OR-REENTRANT count={count} reclaimed=0 pending={} inflight={handed} outstanding=0 ptr_match={} strays={strays}",
                        m.pending.len(),
                        (changes == handed_ptr) as u8
                    ));
                }
            } else if count as usize != handed || changes != handed_ptr {
                let mismatches = Q_MISMATCH.fetch_add(1, Ordering::Relaxed) + 1;
                log_line(&format!(
                    "pfqueue finish REFUSED count={count} expected={handed} reclaimed=0 pending={} outstanding=1 ptr_match={} mismatches={mismatches}",
                    m.pending.len(),
                    (changes == handed_ptr) as u8
                ));
            } else {
                let n = m.in_flight.len();
                m.in_flight.clear();
                m.batch_outstanding = false;
                Q_RECLAIMED.fetch_add(n as u64, Ordering::Relaxed);
                // CHANGE 1b: reclaimed>0 is a tuple change, so a real reclaim always logs.
                let pending = m.pending.len();
                let mode = m.finish_state.due((n, pending, 0, false));
                if mode != 0 && debug_enabled() {
                    log_line(&format!(
                        "pfqueue finish reclaimed={n} pending={pending} inflight=0 outstanding=0{}",
                        if mode == 1 { " changed=1" } else { " hb=1" }
                    ));
                }
            }
            probe_snapshot(m);
        },
        (),
    );
    let _ = handle;
    S_OK
}

// Original SDK ABI: `const char* PFMultiplayerGetErrorMessage(HRESULT)` — one argument in
// ECX, pointer returned in RAX. Both exe call sites (0x143b4bae9, 0x14025d5f8) leave RDX
// containing either the state-change pointer or stale caller-saved garbage, so the old 2-arg
// form's write through `out` corrupted a change or wrote to a wild address.
#[no_mangle]
pub unsafe extern "C" fn PFMultiplayerGetErrorMessage(hr: i32) -> *const c_char {
    let _ = hr;
    ERR.get_or_init(|| CString::new("lan playfab multiplayer stub").unwrap())
        .as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn PFLobbyGetLobbyId(lobby: *mut c_void, out: *mut *const c_char) -> i32 {
    if lobby.is_null() || out.is_null() {
        return E_FAIL;
    }
    let l = &*(lobby as *mut Lobby);
    if l.pending {
        return E_PF_OBJECT_STILL_PENDING;
    }
    *out = l.id.as_ptr();
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFLobbyGetConnectionString(lobby: *mut c_void, out: *mut *const c_char) -> i32 {
    if lobby.is_null() || out.is_null() {
        return E_FAIL;
    }
    let l = &*(lobby as *mut Lobby);
    if l.pending {
        return E_PF_OBJECT_STILL_PENDING;
    }
    *out = l.connection.as_ptr();
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFLobbyGetLobbyProperty(
    lobby: *mut c_void,
    key: *const c_char,
    out: *mut *const c_char,
) -> i32 {
    if lobby.is_null() || out.is_null() {
        return E_FAIL;
    }
    let k = read_cstr(key);
    let l = &*(lobby as *mut Lobby);
    let mut val = l
        .props
        .get(&k)
        .map(|c| c.as_ptr())
        .unwrap_or(ptr::null());
    // A real service returns whatever the title stored, placeholder included (genuine Party even
    // has a dedicated "network descriptor is a placeholder" error; see MS_DLL_GROUND_TRUTH §4.4).
    // The shim's own join gate refuses to complete a join until the stored descriptor is real
    // (`lobby_ready_for_guest`), so the only window in which a placeholder must not be handed to
    // the title is before that completion (`l.pending`); afterwards the stored value is real.
    if k == "network_descriptor" && !val.is_null() && l.pending {
        let v = read_cstr(val);
        if !is_real_network_descriptor(&v) {
            val = ptr::null();
        }
    }
    *out = val;
    if (*out).is_null()
        || k == "network_descriptor"
        || k == "invitation_identifier"
        || k == "network_id"
        // OC-4: a successful read of id_container was invisible, so prior notes concluded "the
        // exe never reads it" from a log that could not show it either way. Log it explicitly.
        || k == "id_container"
    {
        let shown = if (*out).is_null() {
            "null".into()
        } else {
            let v = read_cstr(*out);
            if v.len() > 96 {
                format!("{}...", &v[..96])
            } else {
                v
            }
        };
        debug_log(&format!("GetLobbyProperty {k}={shown}"));
    }
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFLobbyGetSearchProperty(
    lobby: *mut c_void,
    key: *const c_char,
    out: *mut *const c_char,
) -> i32 {
    if lobby.is_null() || out.is_null() {
        return E_FAIL;
    }
    let k = read_cstr(key);
    let l = &*(lobby as *mut Lobby);
    *out = l
        .search
        .get(&k)
        .map(|c| c.as_ptr())
        .unwrap_or(ptr::null());
    let shown = if (*out).is_null() {
        "null".to_string()
    } else {
        let v = read_cstr(*out);
        if v.is_empty() {
            "empty".to_string()
        } else {
            preview_str(&v, 96)
        }
    };
    // The exe polls search properties every frame; logging every call buried the log (1,134 of
    // 1,165 lines in one run were `number_key9=0`). Log only when this key's value changes.
    let changed = {
        let mut guard = SEARCH_PROP_LOG
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.insert(k.clone(), shown.clone()) != Some(shown.clone())
    };
    if changed {
        debug_log(&format!("GetSearchProperty {k}={shown}"));
    }
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFLobbyGetMemberProperty(
    lobby: *mut c_void,
    member: *const EntityKey,
    key: *const c_char,
    out: *mut *const c_char,
) -> i32 {
    if lobby.is_null() || out.is_null() {
        return E_FAIL;
    }
    let k = read_cstr(key);
    let l = &*(lobby as *mut Lobby);
    if member.is_null() {
        return E_PF_ENTITY_KEY_MALFORMED;
    }
    let mid = read_cstr((*member).id);
    if l.pending {
        // Pre-completion contract: no member properties are visible yet (S_OK + NULL).
        *out = ptr::null();
        return S_OK;
    }
    if !mid.is_empty()
        && !l.members.iter().any(|m| read_cstr(m.id) == mid)
        && !l.member_props.contains_key(&mid)
    {
        // Genuine d2: "the entity provided wasn't a member of the lobby".
        *out = ptr::null();
        return E_PF_LOBBY_MEMBER_NOT_IN_LOBBY;
    }
    *out = l
        .member_props
        .get(&mid)
        .and_then(|m| m.get(&k))
        .map(|c| c.as_ptr())
        .unwrap_or(ptr::null());
    if (*out).is_null() {
        log_line(&format!("GetMemberProperty miss member={mid} key={k}"));
    } else if k.starts_with("member_platform") {
        debug_log(&format!(
            "GetMemberProperty member={mid} {k}={}",
            read_cstr(*out)
        ));
    }
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFLobbyGetOwner(lobby: *mut c_void, out: *mut *const EntityKey) -> i32 {
    if lobby.is_null() || out.is_null() {
        return E_FAIL;
    }
    let l = &*(lobby as *mut Lobby);
    if l.pending {
        return E_PF_OBJECT_STILL_PENDING;
    }
    if !l.has_owner {
        // A service lobby with migration policy None/Manual is ownerless after the owner
        // leaves; the documented output is S_OK + NULL and the exe null-checks it.
        *out = ptr::null();
        return S_OK;
    }
    *out = &l.owner;
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFLobbyGetMembershipLock(lobby: *mut c_void, out: *mut i32) -> i32 {
    if lobby.is_null() || out.is_null() {
        return E_FAIL;
    }
    let l = &*(lobby as *mut Lobby);
    if l.pending {
        return E_PF_OBJECT_STILL_PENDING;
    }
    *out = l.membership_lock;
    // The exe only reaches this getter when an Updated set membershipLockUpdated
    // (disassembly 0x143B488B1 -> 0x143B488D0 -> lobby+0xD8 at 0x143B488DC), so this
    // line is the end-to-end proof that the lock-population Updated was consumed.
    debug_log(&format!(
        "GetMembershipLock value={} (reached via Updated membershipLockUpdated)",
        *out
    ));
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFLobbyGetMemberConnectionStatus(
    lobby: *mut c_void,
    member: *const EntityKey,
    out: *mut i32,
) -> i32 {
    if out.is_null() {
        return E_FAIL;
    }
    *out = 0; // NotConnected is the pre-completion value
    if lobby.is_null() || member.is_null() {
        return E_FAIL;
    }
    let l = &*(lobby as *mut Lobby);
    if l.pending {
        return S_OK;
    }
    let mid = read_cstr((*member).id);
    if !mid.is_empty()
        && !l.members.iter().any(|m| read_cstr(m.id) == mid)
        && !l.member_props.contains_key(&mid)
    {
        // Genuine PFLobbyGetMemberConnectionStatus d2: member key not in the lobby.
        return E_PF_LOBBY_MEMBER_NOT_IN_LOBBY;
    }
    *out = 1; // Connected
    debug_throttled(
        "member_conn_status",
        &format!("GetMemberConnectionStatus member={mid} value=1"),
    );
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFLobbyPostUpdate(
    lobby: *mut c_void,
    member: *const EntityKey,
    update: *const u8,
    member_update: *const u8,
    async_ctx: *mut c_void,
) -> i32 {
    if lobby.is_null() {
        return E_FAIL;
    }
    if update.is_null() && member_update.is_null() {
        // Genuine d1: "either a lobby update or a member update must be provided".
        return E_PF_LOBBY_EMPTY_UPDATE;
    }
    let l = &mut *(lobby as *mut Lobby);
    if l.left {
        // Genuine d1: "updates can't be queued after disconnecting from the lobby".
        return E_PF_LOBBY_UPDATE_AFTER_DISCONNECT;
    }
    let before = snapshot(l);
    // PFLobbyDataUpdate semantics (PFLobby.h): a null value pointer means "delete this key".
    // The SDK turns those into the service's *ToDelete arrays; `kv_list` used to fold them into
    // empty strings, which left the key stored forever (NUMBER_KEY9 §6).
    let mut search_deletes: Vec<String> = Vec::new();
    let mut lobby_deletes: Vec<String> = Vec::new();
    let mut member_deletes: Vec<String> = Vec::new();
    let mut member_target: Option<String> = None;
    let mut scalars = PostUpdateScalars::default();
    if !update.is_null() {
        // PFLobbyDataUpdate layout (1.8): newOwner@+0, maxMemberCount@+8, accessPolicy@+0x10,
        // membershipLock@+0x18, searchCount@+0x20, searchKeys@+0x28, searchValues@+0x30,
        // lobbyCount@+0x38, lobbyKeys@+0x40, lobbyValues@+0x48. The 1.8.0 DLL has no
        // restrictInvitesToLobbyOwner string (the docs added it later), so +0x50 is not read.
        let new_owner = ptr::read_unaligned(update as *const *const EntityKey);
        if !new_owner.is_null() {
            let oid = read_cstr((*new_owner).id);
            if !oid.is_empty() {
                scalars.owner = Some(EntityKey {
                    id: intern(&oid),
                    type_: intern(&read_cstr((*new_owner).type_)),
                });
            }
        }
        let maxp = ptr::read_unaligned(update.add(8) as *const *const u32);
        if !maxp.is_null() {
            scalars.max_players = Some(ptr::read_unaligned(maxp));
        }
        let accessp = ptr::read_unaligned(update.add(0x10) as *const *const i32);
        if !accessp.is_null() {
            scalars.access_policy = Some(ptr::read_unaligned(accessp).max(0) as u32);
        }
        let lockp = ptr::read_unaligned(update.add(0x18) as *const *const i32);
        if !lockp.is_null() {
            scalars.membership_lock = Some(ptr::read_unaligned(lockp));
        }
        let search_count = ptr::read_unaligned(update.add(0x20) as *const u32);
        let search_keys = ptr::read_unaligned(update.add(0x28) as *const *const *const c_char);
        let search_vals = ptr::read_unaligned(update.add(0x30) as *const *const *const c_char);
        let (search_sets, search_del) = kv_list_split(search_count, search_keys, search_vals);
        apply_props(&mut l.search, search_sets);
        drop_props(&mut l.search, &search_del);
        search_deletes = search_del;
        let lobby_count = ptr::read_unaligned(update.add(0x38) as *const u32);
        let lobby_keys = ptr::read_unaligned(update.add(0x40) as *const *const *const c_char);
        let lobby_vals = ptr::read_unaligned(update.add(0x48) as *const *const *const c_char);
        let (lobby_sets, lobby_del) = kv_list_split(lobby_count, lobby_keys, lobby_vals);
        apply_props(&mut l.props, lobby_sets);
        drop_props(&mut l.props, &lobby_del);
        lobby_deletes = lobby_del;
        if let Some(o) = scalars.owner {
            l.owner = o;
            l.has_owner = true;
        }
        if let Some(m) = scalars.max_players {
            l.max_players = m;
        }
        if let Some(a) = scalars.access_policy {
            l.access_policy = a;
        }
        if let Some(k) = scalars.membership_lock {
            l.membership_lock = k;
        }
        debug_throttled(
            "postupdate_search",
            &format!(
                "PostUpdate search={search_count}(set) del={} lobby={lobby_count}(set) del={}",
                search_deletes.len(),
                lobby_deletes.len()
            ),
        );
        // Which keys the game actually posts is load-bearing: id_container is service-side state
        // the exe expects to be maintained for it, so we need to see whether it is ever posted.
        let mut snames: Vec<String> = Vec::new();
        if !search_keys.is_null() {
            for i in 0..(search_count as usize).min(16) {
                let kp = ptr::read_unaligned(search_keys.add(i));
                if !kp.is_null() {
                    snames.push(read_cstr(kp));
                }
            }
        }
        let mut lnames: Vec<String> = Vec::new();
        if !lobby_keys.is_null() {
            for i in 0..(lobby_count as usize).min(16) {
                let kp = ptr::read_unaligned(lobby_keys.add(i));
                if !kp.is_null() {
                    lnames.push(read_cstr(kp));
                }
            }
        }
        debug_throttled(
            "postupdate_keys",
            &format!(
                "PostUpdate keys search=[{}] lobby=[{}] del_search=[{}] del_lobby=[{}]",
                snames.join(","),
                lnames.join(","),
                search_deletes.join(","),
                lobby_deletes.join(",")
            ),
        );
    }
    // PFLobbyMemberDataUpdate: member* +0, count +8, keys +16, vals +24.
    // This exe's PostUpdate sites pass NULL here (join_cfg is Create/Join); keep parse anyway.
    let mut mid = entity_id_of(member);
    if !member_update.is_null() {
        let mu_member = ptr::read_unaligned(member_update as *const *const EntityKey);
        if !mu_member.is_null() {
            let id = read_cstr((*mu_member).id);
            if !id.is_empty() {
                mid = id;
            }
        }
        let mu_count = ptr::read_unaligned(member_update.add(8) as *const u32);
        let mu_keys = ptr::read_unaligned(member_update.add(16) as *const *const *const c_char);
        let mu_vals = ptr::read_unaligned(member_update.add(24) as *const *const *const c_char);
        let (mu_sets, mu_del) = kv_list_split(mu_count, mu_keys, mu_vals);
        if !mid.is_empty() && (!mu_sets.is_empty() || !mu_del.is_empty()) {
            let entry = l.member_props.entry(mid.clone()).or_insert_with(HashMap::new);
            apply_props(entry, mu_sets);
            drop_props(entry, &mu_del);
            ensure_member_props(entry, &mid);
            member_target = Some(mid.clone());
            member_deletes = mu_del;
            debug_log(&format!(
                "PostUpdate MemberData member={mid} keys={} del={}",
                entry.len(),
                member_deletes.len()
            ));
        }
    }
    debug_throttled(
        "postupdate",
        &format!(
        "PostUpdate id={} props={} desc={} inv={}",
        l.id.to_string_lossy(),
        l.props.len(),
        l.props
            .get("network_descriptor")
            .map(|c| {
                let s = c.to_string_lossy();
                if is_real_network_descriptor(&s) {
                    "LAN1"
                } else {
                    "hidden"
                }
            })
            .unwrap_or("none"),
        l.props.contains_key("invitation_identifier")
    ));
    // The real service applies the update and pushes a PFLobbyUpdatedStateChange back to
    // every member including the author; only then does the title's view reflect it
    // (PFLobbyPostUpdate docs). The shim applies locally for its own getters, so the
    // synthesised echo is emitted here from the difference the local apply produced.
    // A failed post is not echoed: the service did not accept the update, and the next
    // poll will correct the local view instead.
    let posted = post_update(
        l,
        &search_deletes,
        &lobby_deletes,
        member_target.as_deref(),
        &member_deletes,
        &scalars,
    );
    let delta = if posted {
        Some(unsafe { diff_lobby(&before, l) })
    } else {
        None
    };
    with_mp(
        |m| {
            // PFLobbyPostUpdateCompletedStateChange is 0x28: result@+4, lobby@+8,
            // localUser@+0x10, asyncContext@+0x20. (Previously 0x18 with the lobby written at
            // +0x10, which is the localUser slot.) The exe currently reads only result@+4, so
            // this was masked rather than harmful.
            let sc = alloc_sc(0x28, 8);
            if !sc.is_null() {
                ptr::write_unaligned(sc.add(4) as *mut i32, 0);
                ptr::write_unaligned(sc.add(8) as *mut *mut Lobby, lobby as *mut Lobby);
                if !member.is_null() {
                    ptr::write_unaligned(sc.add(0x10) as *mut EntityKey, ptr::read(member));
                }
                ptr::write_unaligned(sc.add(0x20) as *mut *mut c_void, async_ctx);
                queue(m, sc);
            }
            // The Updated follows the completion ("sometime afterwards" in the PostUpdate
            // contract) and carries only what the apply actually changed.
            if let Some(delta) = delta {
                emit_updated(m, l, delta, false);
            }
        },
        (),
    );
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFLobbyLeave(
    lobby: *mut c_void,
    _member: *const EntityKey,
    _async: *mut c_void,
) -> i32 {
    if !lobby.is_null() {
        let id = (*(lobby as *mut Lobby)).id.to_string_lossy().into_owned();
        let mut eid = local_entity_id();
        if eid.is_empty() {
            eid = read_cstr((*(lobby as *mut Lobby)).owner.id);
        }
        let body = if eid.is_empty() {
            format!("{{\"LobbyId\":\"{id}\"}}")
        } else {
            format!(
                "{{\"LobbyId\":\"{id}\",\"EntityId\":\"{}\"}}",
                json_escape(&eid)
            )
        };
        let _ = http_json("POST", "/Lobby/LeaveLobby", &body);
        debug_log(&format!("Leave {id} entity={eid}"));
    }
    with_mp(
        |m| {
            if !lobby.is_null() {
                (*(lobby as *mut Lobby)).left = true;
            }
            m.lobbies.retain(|p| *p as *mut c_void != lobby);
            let sc = alloc_sc(0x18, 6);
            if !sc.is_null() {
                ptr::write_unaligned(sc.add(4) as *mut i32, 0);
                queue(m, sc);
            }
        },
        (),
    );
    S_OK
}

#[no_mangle]
pub unsafe extern "C" fn PFLobbyForceRemoveMember(
    _lobby: *mut c_void,
    _target: *const EntityKey,
    _prevent: u8,
    _async: *mut c_void,
) -> i32 {
    if _target.is_null() || read_cstr((*_target).id).is_empty() {
        return E_PF_ENTITY_KEY_MALFORMED;
    }
    // LAN sessions have no kick/ban path (the exe only calls this for host kicks), and a
    // fabricated removal would desync the broker roster; keep the documented no-op.
    S_OK
}

#[no_mangle]
pub extern "system" fn DllMain(_m: *mut c_void, reason: u32, _r: *mut c_void) -> i32 {
    if reason == 1 {
        debug_log("PlayFabMultiplayerWin.dll LAN stub loaded");
    }
    1
}

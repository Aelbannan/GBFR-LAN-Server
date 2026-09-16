//! LAN replacement for PartyWin.dll (21 IAT exports).
//! Pumps C-API state changes the exe actually switches on. Relays opaque
//! PartyEndpointSendMessage bytes over UDP. Does not speak Azure Party.

#![allow(non_snake_case, clippy::missing_safety_doc)]

use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_void, CString};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

include!("../../common/lan_cfg.rs");

/// Pure sequence/ack/ordering state machine (no Win32 calls, no logging). Shared verbatim with
/// `reliable_test.rs` so the simulated loss/duplication/reordering tests exercise the shipping
/// code. See `src/reliable.rs` for the PartySendMessageOptions semantics implemented here.
mod reliable;
use reliable::RecvOutcome;

const SUCCESS: u32 = 0;
const ERR: u32 = 0x8000_4005;
const DESC_SIZE: usize = 357; // 0x165: 37 + 20 + 300, matching the real PartyNetworkDescriptor
const IDENT_LEN: usize = 37;
const REGION_LEN: usize = 20;
const OPAQUE_LEN: usize = 300;
const SERIALIZED_MAX: usize = 449;
const MAGIC: &[u8; 4] = b"GBFR";
const KIND_HELLO: u8 = 1;
const KIND_MSG: u8 = 2;
const KIND_ACK: u8 = 3;

/// Idle poll interval of the transport thread. An enqueue wakes the thread immediately through
/// the outbox condvar, so this only bounds how quickly unsolicited inbound datagrams are noticed
/// and how often idle timers are re-evaluated. Deliberately a few milliseconds, never a frame.
const TRANSPORT_POLL_MS: u64 = 2;
/// Liveness line cadence (log only; the thread never waits this long). Kept slow because it is
/// a rollup; the transport counters also ride `msg_stats`. Debug mode restores the old 5 s
/// cadence for the integration tests and live diagnosis.
const TRANSPORT_HEARTBEAT_MS: u64 = 30_000;
const DEBUG_HEARTBEAT_MS: u64 = 5_000;

#[inline]
fn heartbeat_ms() -> u64 {
    if debug_enabled() {
        DEBUG_HEARTBEAT_MS
    } else {
        TRANSPORT_HEARTBEAT_MS
    }
}
/// Sends processed per transport cycle. The outbox is unbounded, so a producer burst must not
/// hold the global handle mutex for the whole backlog; the next cycle runs immediately while
/// jobs remain queued.
const OUTBOX_BATCH: usize = 128;
/// Datagrams parsed per `recv_udp` call, so a flooded socket cannot hold the handle mutex for
/// longer than a bounded batch.
const RECV_BATCH: usize = 256;
/// A peer poll in flight longer than this is presumed lost (broker thread death, a result swept
/// while the box was out of `h.networks`, or a re-added box) and is re-armed. Must exceed the
/// worst-case broker task: 1s connect + 3s read, plus `lan_ip`'s 1s probe for a register.
const PEER_POLL_TIMEOUT_MS: u64 = 10_000;
/// Bound on queued broker requests. `Register` tasks are coalesced by (network, entity) first,
/// so this only bites during a sustained outage; the cap drops the oldest `Register`, never a
/// `Leave` or `PollPeers`.
const BROKER_QUEUE_CAP: usize = 512;
/// The cached log handle is dropped and re-opened every this often, so deleting the log mid-run
/// still produces a fresh file on the next line (the old code noticed on the next line; this
/// bounds the delay to the reopen interval).
const LOG_REOPEN_MS: u128 = 5000;

/// Wire header length. v1 was 48 bytes and carried neither the sender's send options nor a
/// sequence number; v2 (56) carried both; v3 (60) adds the cumulative guaranteed-space ack for
/// the real guaranteed-delivery implementation. With `PARTY_RELIABLE = false` the shim keeps the
/// exact v2 wire format it shipped before, so disabling reliability restores the old behaviour.
const HDR_LEN: usize = if reliable::PARTY_RELIABLE { 60 } else { 56 };
const HDR_VERSION: u8 = if reliable::PARTY_RELIABLE { 3 } else { 2 };
/// Offset of the cumulative guaranteed-space ack in the v3 header.
const HDR_ACK_OFF: usize = 56;

/// `PartyMessageReceivedOptions` — describes what the library *actually did* when delivering the
/// message, not what the sender asked for (see the Party transport-options docs).
const RECV_GUARANTEED: u32 = 0x1;
const RECV_SEQUENTIAL: u32 = 0x2;
/// Never set: this shim puts one message in one datagram, so it never reports fragmented delivery.
#[allow(dead_code)]
const RECV_FRAGMENTED: u32 = 0x4;

/// `PartySendMessageOptions` bits we act on.
const SEND_GUARANTEED: u32 = 0x1;
const SEND_SEQUENTIAL: u32 = 0x2;

/// Sequence space for this local endpoint's sequential sends. Per the docs each
/// (local endpoint -> target endpoint) pairing is its own sequence space; we have a single local
/// endpoint, so one monotonic counter is the correct space.
static SEND_SEQ: AtomicU32 = AtomicU32::new(1);

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
        dir.join("party_shim.log").to_string_lossy().into_owned()
    })
    .clone()
}

static LOG_TRUNCATED: AtomicBool = AtomicBool::new(false);
/// Cached append handle. `log_line` used to open and close the file for every line, and it is now
/// on the transport thread's hot path while that thread holds the global handle mutex — every
/// logged send or receive used to lengthen the tick's worst-case lock wait by a full open + close.
static LOG_FILE: OnceLock<Mutex<Option<(std::fs::File, Instant)>>> = OnceLock::new();

/// Rate-limited logging for hot paths (per-call APIs and per-tick loops): at most one line per
/// second per key, reporting how many were suppressed. Keeps the log readable without hiding the
/// signal — the suppressed count shows the rate, so a runaway loop is still visible.
fn throttle_map() -> &'static Mutex<HashMap<String, (Instant, u32)>> {
    static LAST: OnceLock<Mutex<HashMap<String, (Instant, u32)>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

fn log_throttled(key: &str, msg: &str) {
    let mut g = throttle_map().lock().unwrap_or_else(|e| e.into_inner());
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

/// Same cadence and suppression accounting as `log_throttled`, but the line text is built only
/// when the line will actually be emitted — the probe-cost rule (see `probe_tick_due` /
/// `PROBES_SKIPPED`) applied to the delivery path, where a deferral can repeat every tick. `f`
/// receives the number of suppressed calls in the previous second.
fn log_throttled_lazy(key: &str, f: impl FnOnce(u32) -> String) {
    let mut g = throttle_map().lock().unwrap_or_else(|e| e.into_inner());
    let e = g.entry(key.to_string()).or_insert((Instant::now(), 0));
    if e.0.elapsed() >= Duration::from_secs(1) {
        let n = e.1;
        *e = (Instant::now(), 0);
        let msg = f(n);
        if n > 0 {
            log_line(&format!("{msg} [+{n} more in the previous second]"));
        } else {
            log_line(&msg);
        }
    } else {
        e.1 += 1;
    }
}

fn log_line(msg: &str) {
    let line = format!(
        "[{}] {}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        msg
    );
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
            // The file may have been deleted or replaced: re-open on the next line.
            *g = None;
        }
    }
}

/// Verbose-only line: per-message traffic, probes, hex samples, per-batch ledger traces.
/// Off unless lan.ini `[debug] enabled = true` or `GBFR_LAN_DEBUG=1`. `log_line` is reserved
/// for errors and warnings; there is no periodic output when debug is off.
#[inline]
fn debug_log(msg: &str) {
    if debug_enabled() {
        log_line(msg);
    }
}

/// Throttled diagnostics with the same gate as `debug_log`. The message may already be built
/// by the caller; the lazy variant builds it only when the line will be emitted.
#[inline]
fn debug_throttled(key: &str, msg: &str) {
    if debug_enabled() {
        log_throttled(key, msg);
    }
}

#[inline]
fn debug_throttled_lazy(key: &str, f: impl FnOnce(u32) -> String) {
    if debug_enabled() {
        log_throttled_lazy(key, f);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Reliable-delivery bookkeeping (live side)
// ─────────────────────────────────────────────────────────────────────────────

/// Per-mode wire counters for the msg_stats line. Indexed by `reliable::mode_index`:
/// 0 guar-seq, 1 guar-nonseq, 2 best-seq, 3 best-nonseq.
#[derive(Default, Clone, Copy)]
struct RelCounts {
    sent: u64,
    retransmitted: u64,
    acked: u64,
    dup_dropped: u64,
    ooo_dropped: u64,
    ooo_queued: u64,
    evicted: u64,
}

static REL_STATS: OnceLock<Mutex<[RelCounts; 4]>> = OnceLock::new();

fn rel_stats() -> &'static Mutex<[RelCounts; 4]> {
    REL_STATS.get_or_init(|| Mutex::new([RelCounts::default(); 4]))
}

fn rel_bump(mode: usize, f: impl FnOnce(&mut RelCounts)) {
    if let Ok(mut g) = rel_stats().lock() {
        f(&mut g[mode]);
    }
}

fn reliable_stats_note() -> String {
    if !reliable::PARTY_RELIABLE {
        return "reliable=disabled".into();
    }
    let mut parts = Vec::new();
    if let Ok(g) = rel_stats().lock() {
        for (i, c) in g.iter().enumerate() {
            if c.sent
                | c.retransmitted
                | c.acked
                | c.dup_dropped
                | c.ooo_dropped
                | c.ooo_queued
                | c.evicted
                == 0
            {
                continue;
            }
            parts.push(format!(
                "{}(sent={} retransmitted={} acked={} dup-dropped={} out-of-order-dropped={} out-of-order-queued={} evicted={})",
                reliable::mode_name(i),
                c.sent,
                c.retransmitted,
                c.acked,
                c.dup_dropped,
                c.ooo_dropped,
                c.ooo_queued,
                c.evicted,
            ));
        }
    }
    if parts.is_empty() {
        "reliable=none".into()
    } else {
        format!("reliable {}", parts.join(" "))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CHANGE B: probe-pass gate
// ─────────────────────────────────────────────────────────────────────────────

/// One probe pass at most every `PROBE_PASS_MS`. The expensive part of the old per-tick block was
/// building the strings (format! plus member/work/ready list walks) only for log_throttled to
/// discard them. The gate runs before any of that, so skipped ticks allocate and walk nothing.
const PROBE_PASS_MS: u64 = 200;
static LAST_PROBE_PASS_MS: AtomicU64 = AtomicU64::new(0);
static PROBES_RUN: AtomicU64 = AtomicU64::new(0);
static PROBES_SKIPPED: AtomicU64 = AtomicU64::new(0);

fn probe_tick_due(now_ms: u64) -> bool {
    let last = LAST_PROBE_PASS_MS.load(Ordering::Relaxed);
    if last != 0 && now_ms.wrapping_sub(last) < PROBE_PASS_MS {
        PROBES_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    LAST_PROBE_PASS_MS.store(now_ms, Ordering::Relaxed);
    PROBES_RUN.fetch_add(1, Ordering::Relaxed);
    true
}

// ─────────────────────────────────────────────────────────────────────────────
// Thread probe (P1 "can the exe call us from another thread?" / P2 lock containment)
// ─────────────────────────────────────────────────────────────────────────────

/// Every distinct thread id that has entered a Party export, with its call count. The hot path is
/// one `GetCurrentThreadId`, one relaxed counter bump and at most `THREAD_SLOTS` relaxed loads: no
/// lock, no allocation, no string. A new tid is logged exactly once (slot reserved by CAS) and the
/// summary rides the existing `probe_session` line, whose text exists only when that line is
/// actually written (see `thread_probe_note`).
const THREAD_SLOTS: usize = 8;
static THREAD_TIDS: [AtomicU32; THREAD_SLOTS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
static THREAD_COUNTS: [AtomicU64; THREAD_SLOTS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
static THREAD_DISTINCT: AtomicU32 = AtomicU32::new(0);
static THREAD_TOTAL_CALLS: AtomicU64 = AtomicU64::new(0);
static THREAD_OVERFLOW: AtomicBool = AtomicBool::new(false);

/// Called at the top of every Party export, before any early return. Cheap by design; the
/// `format!` is only built on the first call ever seen on this thread.
#[inline]
fn note_party_thread() -> u32 {
    // Option B: the first Party export call starts the transport thread. Never from DllMain.
    ensure_transport_thread();
    // Option A step: the broker thread. Same lazy-first-export start.
    ensure_broker_thread();
    // Solo sampler: same lazy-first-export start, never DllMain (see its own section below).
    ensure_sampler_thread();
    // Event bus: one aligned read on every export call so a single-tick code is never missed.
    note_eventbus();
    let tid = unsafe { GetCurrentThreadId() };
    THREAD_TOTAL_CALLS.fetch_add(1, Ordering::Relaxed);
    let mut empty = None;
    let mut i = 0;
    while i < THREAD_SLOTS {
        let t = THREAD_TIDS[i].load(Ordering::Relaxed);
        if t == tid {
            THREAD_COUNTS[i].fetch_add(1, Ordering::Relaxed);
            return tid;
        }
        if t == 0 && empty.is_none() {
            empty = Some(i);
        }
        i += 1;
    }
    if let Some(i) = empty {
        if THREAD_TIDS[i]
            .compare_exchange(0, tid, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            THREAD_COUNTS[i].store(1, Ordering::Relaxed);
            let n = THREAD_DISTINCT.fetch_add(1, Ordering::Relaxed) + 1;
            debug_log(&format!(
                "party_thread new tid={tid:#010x} distinct={n} calls_total={} — first Party export call on this thread",
                THREAD_TOTAL_CALLS.load(Ordering::Relaxed)
            ));
            return tid;
        }
    } else {
        THREAD_OVERFLOW.store(true, Ordering::Relaxed);
    }
    tid
}

/// Summary for the periodic probe line. Only called after the caller has decided the line will be
/// emitted; nothing (including the `format!`) runs on a gated pass.
fn thread_probe_note() -> String {
    let now = unsafe { GetCurrentThreadId() };
    let mut parts = Vec::new();
    let mut i = 0;
    while i < THREAD_SLOTS {
        let t = THREAD_TIDS[i].load(Ordering::Relaxed);
        if t != 0 {
            parts.push(format!("{t:#x}:{}", THREAD_COUNTS[i].load(Ordering::Relaxed)));
        }
        i += 1;
    }
    if THREAD_OVERFLOW.load(Ordering::Relaxed) {
        parts.push("overflow".into());
    }
    format!(
        "threads={} now={now:#x} [{}]",
        THREAD_DISTINCT.load(Ordering::Relaxed),
        parts.join(" ")
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Event-bus probe — `[[0x147C233C0]+0x108]` read on every export call
// ─────────────────────────────────────────────────────────────────────────────
//
// `DecisionStartQuestOnMulti` writes ToError (0x0CE586D9) / ToSuccess (0xD40BFC86) into this dword
// and the FSM consumes it within a single tick; the 1 Hz probe pass can miss any code that lives
// for less than a second, which is exactly the difference between "the action bailed" and "the
// action never finished". `note_eventbus` runs from `note_party_thread`, i.e. on every call into
// every Party export, and logs on change with a per-value count, so a distinct value can never be
// lost even though repeats are not logged one line each. The table is bounded
// (`EVENTBUS_SLOTS`); a value beyond the cap is still logged (`overflow=1`) rather than dropped.
//
// Cost: one cached base lookup, one `readable`-gated indirection and one u32 read per call; the
// table update is one uncontended mutex over a fixed array. A line is built only on a change;
// `seen`/`missing` accumulate silently in between.
const EVENTBUS_OBJ_RVA: usize = 0x7c233c0; // VA 0x147C233C0 (same object the quest_action probe reads)
const EVENTBUS_CODE_OFF: usize = 0x108;
const EVENTBUS_SLOTS: usize = 16;

#[derive(Clone, Copy, Default)]
struct EventBusSlot {
    value: u32,
    count: u64,
}

struct EventBusTable {
    slots: [EventBusSlot; EVENTBUS_SLOTS],
    distinct: u32,
    last: Option<u32>,
    seen: u64,
    missing: u64,
    overflow: u64,
}

fn eventbus_table() -> &'static Mutex<EventBusTable> {
    static T: OnceLock<Mutex<EventBusTable>> = OnceLock::new();
    T.get_or_init(|| {
        Mutex::new(EventBusTable {
            slots: [EventBusSlot::default(); EVENTBUS_SLOTS],
            distinct: 0,
            last: None,
            seen: 0,
            missing: 0,
            overflow: 0,
        })
    })
}

/// One aligned read of the event dword, or `None` when the object or the field is not readable.
#[inline]
/// # Safety
/// Reads the game's global EventBus object: both the global slot and the object it points at
/// are validated with `readable` before the dereference, and a game update that moves the RVA
/// degrades to `None` rather than a fault.
unsafe fn eventbus_dword() -> Option<u32> {
    let base = exe_base();
    if base == 0 || !readable(base + EVENTBUS_OBJ_RVA, 8) {
        return None;
    }
    let p = ptr::read_unaligned((base + EVENTBUS_OBJ_RVA) as *const usize);
    if p == 0 || !readable(p + EVENTBUS_CODE_OFF, 4) {
        return None;
    }
    Some(ptr::read_unaligned((p + EVENTBUS_CODE_OFF) as *const u32))
}

#[inline]
fn note_eventbus() {
    let Some(v) = (unsafe { eventbus_dword() }) else {
        if let Ok(mut t) = eventbus_table().lock() {
            t.missing += 1;
        }
        return;
    };
    let (changed, first, prev, count, distinct, seen, missing, overflow) = {
        let Ok(mut t) = eventbus_table().lock() else {
            return;
        };
        t.seen += 1;
        let prev = t.last;
        let changed = prev != Some(v);
        let mut first = false;
        let mut count = 0u64;
        let n = t.distinct as usize;
        if let Some(s) = t.slots[..n].iter_mut().find(|s| s.value == v) {
            s.count += 1;
            count = s.count;
        } else if (t.distinct as usize) < EVENTBUS_SLOTS {
            let i = t.distinct as usize;
            t.slots[i] = EventBusSlot { value: v, count: 1 };
            t.distinct += 1;
            first = true;
            count = 1;
        } else {
            t.overflow += 1;
        }
        t.last = Some(v);
        (
            changed,
            first,
            prev,
            count,
            t.distinct,
            t.seen,
            t.missing,
            t.overflow,
        )
    };
    if !changed {
        return;
    }
    // Emitting: only now build the line (probe-cost rule). A first-seen value is marked; the
    // table-full case is visible through `overflow` and the value is still in the line.
    log_line(&format!(
        "solo_eventbus v={v:#010x} prev={} count={count} distinct={distinct} seen={seen} first={} missing={missing} overflow={overflow}",
        prev.map_or("ro".to_string(), |p| format!("{p:#010x}")),
        if first { 1 } else { 0 },
    ));
}

/// Delivery-ledger heartbeat (CHANGE A / backlog D1). Same shape as `probe_tick_due`: the due
/// test runs before any string is built or list is walked
/// and skipped ticks are counted (the `hb_skipped=` term in the ledger line).
const LEDGER_HEARTBEAT_MS: u64 = 30_000;

#[inline]
fn ledger_heartbeat_ms() -> u64 {
    if debug_enabled() {
        DEBUG_HEARTBEAT_MS
    } else {
        LEDGER_HEARTBEAT_MS
    }
}
static LEDGER_HB_MS: AtomicU64 = AtomicU64::new(0);
static LEDGER_HB_SKIPPED: AtomicU64 = AtomicU64::new(0);

fn ledger_hb_due(now_ms: u64) -> bool {
    let last = LEDGER_HB_MS.load(Ordering::Relaxed);
    if last != 0 && now_ms.wrapping_sub(last) < ledger_heartbeat_ms() {
        LEDGER_HB_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    LEDGER_HB_MS.store(now_ms, Ordering::Relaxed);
    true
}

// ─────────────────────────────────────────────────────────────────────────────
// Send-site diagnostics requested by the guaranteed-delivery work
// ─────────────────────────────────────────────────────────────────────────────

/// Log the full PartySendMessageOptions word once per distinct value, decoded with the documented
/// bit names. This answers whether the exe asks only for 0x1 or also for 0x2.
fn log_send_options_once(options: u32) {
    static SEEN: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(Vec::new()));
    let Ok(mut g) = seen.lock() else {
        return;
    };
    if g.contains(&options) {
        return;
    }
    g.push(options);
    let mode = reliable::mode_index(options);
    debug_log(&format!(
        "send options word {options:#010x} decoded: {} (mode={}, {}), first use of this value",
        reliable::decode_options(options),
        reliable::mode_name(mode),
        if reliable::guaranteed(options) {
            "guaranteed: pending+retransmit+ack enabled"
        } else {
            "best-effort: no retransmit"
        }
    ));
}

/// Party documents automatic fragmentation/reassembly; this shim still puts one message in one
/// datagram, so flag the first oversize send instead of silently exceeding a sane MTU.
fn log_big_send_once(len: usize, options: u32) {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    debug_log(&format!(
        "send payload len={len} > 1400 options={options:#010x}: Party documents fragmentation/reassembly but shim sends one datagram per message"
    ));
}

/// Loud, once-per-peer report for a datagram that does not carry the shim's v3 header. There is no
/// legacy fallback: an old or foreign sender is a protocol/version mismatch and its bytes are
/// dropped.
fn log_wire_mismatch(src: &SocketAddr, len: usize, preview: &[u8]) {
    static SEEN: OnceLock<Mutex<HashMap<SocketAddr, u64>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = match seen.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let n = g.entry(*src).or_insert(0);
    *n += 1;
    if *n != 1 {
        return;
    }
    let magic = if preview.len() >= 4 {
        hex_preview(&preview[..4], 4)
    } else {
        format!("len<4 ({len})")
    };
    let version = preview.get(5).copied().unwrap_or(0);
    log_line(&format!(
        "PROTOCOL/VERSION MISMATCH from={src} datagram_len={len} magic=[{magic}] version={version} expected magic=GBFR version={HDR_VERSION}: dropping (no legacy fallback); further mismatches from this peer suppressed"
    ));
}

fn u32le(data: &[u8], off: usize) -> Option<u32> {
    data.get(off..off + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
}

fn hex_preview(data: &[u8], n: usize) -> String {
    data.iter()
        .take(n)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn payload_hints(data: &[u8]) -> String {
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

fn log_hex_all() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("GBFR_PARTY_LOG_HEX")
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

fn should_log_payload(key: &str) -> bool {
    if log_hex_all() {
        return true;
    }
    static SEEN: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    let map = SEEN.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut g) = map.lock() else {
        return true;
    };
    let n = g.entry(key.to_string()).or_insert(0);
    *n += 1;
    *n <= 16 || *n == 50 || *n == 100 || *n % 250 == 0
}

fn bump_msg_stats(dir: &str, opcode: u32) {
    static STATS: OnceLock<Mutex<(u64, u64, HashMap<u32, u64>, HashMap<u32, u64>, Instant)>> =
        OnceLock::new();
    let stats = STATS.get_or_init(|| {
        Mutex::new((0, 0, HashMap::new(), HashMap::new(), Instant::now()))
    });
    let Ok(mut g) = stats.lock() else {
        return;
    };
    if dir == "send" {
        g.0 += 1;
        *g.2.entry(opcode).or_insert(0) += 1;
    } else {
        g.1 += 1;
        *g.3.entry(opcode).or_insert(0) += 1;
    }
    // Rollup only: the per-message lines are debug-gated, so this is the default record of
    // what crossed the wire. Fixed 30 s cadence by default; debug mode keeps the old
    // 5 s-or-every-50-messages behaviour the integration tests and live diagnosis rely on.
    let n = g.0 + g.1;
    let due = if debug_enabled() {
        g.4.elapsed() >= Duration::from_secs(5) || n % 50 == 0
    } else {
        g.4.elapsed() >= Duration::from_secs(30)
    };
    if !due {
        return;
    }
    g.4 = Instant::now();
    fn fmt_ops(m: &HashMap<u32, u64>) -> String {
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by_key(|(op, _)| *op);
        v.iter()
            .map(|(op, c)| format!("{op}:{c}"))
            .collect::<Vec<_>>()
            .join(",")
    }
    debug_log(&format!(
        "msg_stats send={} recv={} send_ops={} recv_ops={} {} probes_run={} probes_skipped={}{}{}",
        g.0,
        g.1,
        fmt_ops(&g.2),
        fmt_ops(&g.3),
        reliable_stats_note(),
        PROBES_RUN.load(Ordering::Relaxed),
        PROBES_SKIPPED.load(Ordering::Relaxed),
        transport_stats_note(),
        broker_stats_note(),
    ));
}

/// Always logs `opcode=` (u32le at payload+0) and `len=`. Hex/hints are sampled
/// unless `GBFR_PARTY_LOG_HEX=1`.
fn log_payload(dir: &str, extra: &str, payload: &[u8]) {
    let op = u32le(payload, 0).unwrap_or(0);
    let sub = u32le(payload, 4).unwrap_or(0);
    if dir == "send" && op == 3 && sub == 5 {
        GUEST_SUB5_SENT.store(true, Ordering::SeqCst);
    }
    // Host send is real emission. Recv is marked in deliver_type21 so buffered
    // UDP (pre type 12) does not look like the exe already applied chara data.
    if dir == "send" && is_chara_snapshot(op, payload.len()) {
        note_snapshot();
    }
    if op == 3 && sub == 7 {
        if dir == "send" {
            SUB7_SENT.store(true, Ordering::SeqCst);
        } else {
            SUB7_RECV.store(true, Ordering::SeqCst);
        }
    }
    // msg_stats carries the default per-opcode picture (30 s rollup); the per-message trace
    // and the hex samples are verbose tiers behind `[debug]`. `GBFR_PARTY_LOG_HEX=1` keeps
    // working standalone: it implies the traffic tier and forces every payload's hex.
    bump_msg_stats(dir, op);
    if !debug_enabled() && !log_hex_all() {
        return;
    }
    // Surface the sub-opcode on the unconditional line for sends op=2/3, using the
    // same +4 u32le parse as the sampled `{dir}_hex` hints (payload_hints).
    let sub_tag = if dir == "send" && matches!(op, 2 | 3) {
        u32le(payload, 4)
            .map(|s| format!(" sub={s}"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    debug_log(&format!(
        "{dir} opcode={op} len={}{sub_tag} {extra}",
        payload.len()
    ));
    let key = format!("{dir}:{op}:{sub}:{}", payload.len());
    if !should_log_payload(&key) {
        return;
    }
    // Q7: op-2 payloads carry the quest id (sub 0x5A, +0x14) and the chara blobs, but we only ever
    // logged their first 16 bytes, which hid exactly the fields the quest-start diagnosis needs.
    let nhex = payload.len().min(72);
    let ent = if op == 3 {
        opcode3_entity(payload)
    } else {
        String::new()
    };
    debug_log(&format!(
        "{dir}_hex opcode={op} len={} {} {}{} hex=[{}]",
        payload.len(),
        extra,
        payload_hints(payload),
        ent,
        hex_preview(payload, nhex)
    ));
}

fn opcode3_entity(payload: &[u8]) -> String {
    if payload.len() <= 0x22 {
        return String::new();
    }
    let raw = &payload[0x22..];
    let s: Vec<u8> = raw.iter().copied().take_while(|b| *b != 0).take(32).collect();
    if s.is_empty() {
        return " ent@+22=empty".into();
    }
    if s.iter().all(|b| b.is_ascii_graphic()) {
        format!(" ent@+22={}", String::from_utf8_lossy(&s))
    } else {
        format!(" ent@+22hex=[{}]", hex_preview(raw, 16))
    }
}

fn log_remote_count(n: usize, eid: &str, uid: u16) {
    debug_log(&format!(
        "EndpointCreated remote uid={uid} entity={eid} remotes={n}"
    ));
    if n == 4 {
        debug_log("WATCH_4 remotes=4 (shipping Party maxUserCount; 4 others + local = 5 mesh users)");
    }
    if n == 5 {
        debug_log("WATCH_4 remotes=5 (beyond shipping 4-user mesh if exe still sends maxUserCount=4)");
    }
}

const MEM_COMMIT: u32 = 0x1000;
const PAGE_GUARD: u32 = 0x100;

#[repr(C)]
struct MemoryBasicInformation {
    base: *mut c_void,
    alloc_base: *mut c_void,
    alloc_protect: u32,
    _pad0: u32,
    region_size: usize,
    state: u32,
    protect: u32,
    type_: u32,
}

fn page_has_read(protect: u32) -> bool {
    if protect & PAGE_GUARD != 0 {
        return false;
    }
    matches!(protect & 0xff, 0x02 | 0x04 | 0x08 | 0x20 | 0x40 | 0x80)
}

/// # Safety
/// `p` may be any value (0, stale, or a live game address); this function performs no
/// dereference itself. It is the validation gate every raw read must call: the whole
/// `p..p+n` range has to sit in one committed, readable region.
unsafe fn readable(p: usize, n: usize) -> bool {
    if p == 0 || n == 0 || p.checked_add(n).is_none() {
        return false;
    }
    let mut info = MemoryBasicInformation {
        base: ptr::null_mut(),
        alloc_base: ptr::null_mut(),
        alloc_protect: 0,
        _pad0: 0,
        region_size: 0,
        state: 0,
        protect: 0,
        type_: 0,
    };
    if VirtualQuery(p as *const c_void, &mut info, std::mem::size_of::<MemoryBasicInformation>()) == 0
    {
        return false;
    }
    if info.state != MEM_COMMIT || !page_has_read(info.protect) {
        return false;
    }
    let start = info.base as usize;
    p >= start && p + n <= start.saturating_add(info.region_size)
}

/// Cached module base for the hot read paths (`note_eventbus`, the solo sampler). The exe image
/// cannot move while the process runs, so one `GetModuleHandleA(NULL)` is enough; the older
/// per-pass probes still look it up live and are deliberately left alone.
fn exe_base() -> usize {
    static BASE: OnceLock<usize> = OnceLock::new();
    *BASE.get_or_init(|| unsafe { GetModuleHandleA(ptr::null()) as usize })
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleFileNameW(module: *mut c_void, buf: *mut u16, size: u32) -> u32;
    fn GetCurrentThreadId() -> u32;
    fn GetModuleHandleA(name: *const u8) -> *mut c_void;
    fn VirtualQuery(addr: *const c_void, info: *mut MemoryBasicInformation, len: usize) -> usize;
    fn RtlCaptureStackBackTrace(
        skip: u32,
        cap: u32,
        buf: *mut *mut c_void,
        hash: *mut u32,
    ) -> u16;
}

#[link(name = "winmm")]
extern "system" {
    /// Raises the timer resolution so `TRANSPORT_POLL_MS` is honoured: Windows rounds a timed wait
    /// up to the ~15.6 ms system tick otherwise, which would quantize the idle poll (and therefore
    /// the RTO/ack timers) to a frame. Called once on the transport thread and never paired with
    /// `timeEndPeriod` — the thread is process-lifetime and the resolution dies with the process.
    fn timeBeginPeriod(period: u32) -> u32;
}

/// DAT_1479f3c58 — matching/Party overlay singleton (not a pointer).
const OVERLAY_GLOBAL_RVA: usize = 0x79f3c58;
/// DAT_147034c60 — 4× 24-byte type ids used by FUN_1428F2130.
const LOOKUP_ID4_RVA: usize = 0x7034c60;
/// DAT_147034d40 — 8× 24-byte type ids, same dispatcher loop.
const LOOKUP_ID8_RVA: usize = 0x7034d40;
static LAST_ID_MASK: AtomicU64 = AtomicU64::new(u64::MAX);

/// The game's expression is `X = [[0x1479F3C58] + 0x18]`: the global slot holds a pointer and
/// +0x18 is read *through* it (`LOBBY_STRUCTURE.md` §5). The old probe read the eight bytes
/// after the slot; `append_overlay_note` still logs that value as `overlay_after` so any earlier
/// conclusion can be re-checked against the corrected `overlay_obj`.
unsafe fn overlay_object() -> usize {
    let base = GetModuleHandleA(ptr::null()) as usize;
    if base == 0 {
        return 0;
    }
    let slot = if readable(base + OVERLAY_GLOBAL_RVA, 8) {
        ptr::read_unaligned((base + OVERLAY_GLOBAL_RVA) as *const usize)
    } else {
        0
    };
    if slot == 0 || !readable(slot + 0x18, 8) {
        return 0;
    }
    ptr::read_unaligned((slot + 0x18) as *const usize)
}

fn id_table_note(base: usize) -> String {
    unsafe {
        let mut mask: u16 = 0;
        let mut filled = 0u32;
        for i in 0..4u32 {
            let p = base + LOOKUP_ID4_RVA + (i as usize) * 0x18;
            if readable(p, 8) && ptr::read_unaligned(p as *const u64) != 0 {
                mask |= 1 << i;
                filled += 1;
            }
        }
        for i in 0..8u32 {
            let p = base + LOOKUP_ID8_RVA + (i as usize) * 0x18;
            if readable(p, 8) && ptr::read_unaligned(p as *const u64) != 0 {
                mask |= 1 << (4 + i);
                filled += 1;
            }
        }
        if LAST_ID_MASK.swap(mask as u64, Ordering::SeqCst) != mask as u64 {
            debug_log(&format!("lookup_ids filled={filled}/12 mask={mask:#06x}"));
        }
        format!(" ids={filled}/12")
    }
}

unsafe fn append_overlay_note(base: usize, _session: usize, note: &mut String) {
    if !readable(base + OVERLAY_GLOBAL_RVA, 8) {
        note.push_str(" overlay unreadable");
        return;
    }
    let slot = ptr::read_unaligned((base + OVERLAY_GLOBAL_RVA) as *const usize);
    let after = if readable(base + OVERLAY_GLOBAL_RVA + 0x18, 8) {
        ptr::read_unaligned((base + OVERLAY_GLOBAL_RVA + 0x18) as *const usize)
    } else {
        0
    };
    let obj = overlay_object();
    note.push_str(&format!(
        " overlay_slot={slot:#x} overlay_after={after:#x} overlay_obj={obj:#x}"
    ));
    note.push_str(&id_table_note(base));
    if readable(base + QUEST_MGR_GLOBAL_RVA, 8) {
        let mgr = ptr::read_unaligned((base + QUEST_MGR_GLOBAL_RVA) as *const usize);
        if mgr != 0 && readable(mgr + 0x6c814, 6) {
            let latch = ptr::read_unaligned((mgr + 0x6c814) as *const u32);
            let b18 = ptr::read_unaligned((mgr + 0x6c818) as *const u16);
            note.push_str(&format!(" 6c814={latch} 6c818={b18:#x}"));
        }
    }
}

fn stub_host() -> (String, u16) {
    let c = lan_cfg();
    (c.host, c.port)
}

fn http_json(method: &str, path: &str, body: &str) -> Option<String> {
    http_json_status(method, path, body).map(|(_status, body)| body)
}

/// Bounded connect. The OS default connect timeout is ~21 s on Windows, so this is capped at 1 s.
/// HTTP callers now run on the broker thread; `lan_ip` still calls this synchronously from the
/// game thread (CreateNewNetwork / SerializeNetworkDescriptor), so an unreachable broker host can
/// still cost the caller up to 1 s there.
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
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

/// Char-boundary-safe truncation for log lines (byte slicing panics on multibyte input).
fn truncate_log(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

fn json_str(blob: &str, key: &str) -> Option<String> {
    let quoted = format!("\"{key}\":\"");
    if let Some(rest) = blob.split(&quoted).nth(1) {
        let end = rest.find('"')?;
        return Some(rest[..end].to_string());
    }
    json_num(blob, key)
}

fn json_num(blob: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":");
    let rest = blob.split(&pat).nth(1)?;
    let rest = rest.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(rest[..end].to_string())
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

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    identifier: [u8; IDENT_LEN],
    region: [u8; REGION_LEN],
    opaque: [u8; OPAQUE_LEN],
}

impl Descriptor {
    fn new(id: &str) -> Self {
        let mut d = Self {
            identifier: [0; IDENT_LEN],
            region: [0; REGION_LEN],
            opaque: [0; OPAQUE_LEN],
        };
        let ib = id.as_bytes();
        let n = ib.len().min(IDENT_LEN - 1);
        d.identifier[..n].copy_from_slice(&ib[..n]);
        d.region[..3].copy_from_slice(b"lan");
        d
    }
    fn id_str(&self) -> String {
        let n = self
            .identifier
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(IDENT_LEN - 1);
        String::from_utf8_lossy(&self.identifier[..n]).into_owned()
    }
}

const _: () = assert!(std::mem::size_of::<Descriptor>() == DESC_SIZE);

struct LocalUser {
    entity: CString,
}

struct Endpoint {
    entity: CString,
    uid: u16,
    network: *mut Network,
    ip: String,
    udp_port: u16,
    /// Highest sequential sequence number delivered on this endpoint. `SequentialDelivery` is
    /// documented to move the sequence forward and never deliver an older message, so this is what
    /// enforces the ordering we advertise. Only used when `PARTY_RELIABLE` is false; the reliable
    /// path keeps its own per-mode state below.
    last_seq: u32,
    seq_seen: bool,
    /// Sender state for this (local endpoint -> remote endpoint) pairing: guaranteed sequence
    /// space, best-effort sequential space and the pending retransmit map.
    tx: reliable::Sender,
    /// Receiver state for this (remote endpoint -> local endpoint) pairing: cumulative ack over the
    /// guaranteed space and the bounded out-of-order buffer.
    rx: reliable::Receiver,
}

struct Network {
    descriptor: Descriptor,
    invitation: CString,
    local_user: *mut LocalUser,
    local_endpoint: *mut Endpoint,
    remotes: HashMap<String, *mut Endpoint>,
    udp: Option<UdpSocket>,
    udp_port: u16,
    connect_completed: bool,
    /// Last /party/join broker registration; refreshed every 10s from
    /// poll_peers so the broker's 30s member expiry never hides the lobby.
    last_register: Instant,
    /// Inbound type-21 frames (entity id, payload, send options, sequence) that arrived before the
    /// remote endpoint was registered or before the exe called PartyNetworkCreateEndpoint.
    /// Replayed in FIFO order once both exist; buffering the send options and sequence lets the
    /// reliable receiver run its ordering/ack state machine at replay time.
    pending_rx: VecDeque<(String, Vec<u8>, u32, u32)>,
    /// Peers first seen on the wire whose game-side member row still has to be checked. The
    /// walk (`member_list_has_entity`) reads live exe memory and must run on the tick thread,
    /// never on the transport thread, so the receive path hands candidates over here (bounded).
    pending_peers: VecDeque<(String, String, u16)>,
    /// Type 12 must wait until the exe has finished type 10 (local endpoint).
    /// Queuing both in one pump made the guest hang inside ProcessParty.
    type10_delivered: bool,
    /// Type 21 is dropped unless the exe has inserted the remote peer
    /// (`FUN_142510b30`). That handler only inserts if the remote entity
    /// already exists in `DAT_147c52ce0`.
    type12_delivered: bool,
    /// P5: when the type-12 hold was first observed for this network (`None` while not holding),
    /// and whether the hold timeout has already been logged. Set/cleared by `note_type12_hold`.
    type12_hold_start: Option<Instant>,
    type12_hold_timeout_logged: bool,
    /// Option B: a HELLO (or a datagram from an unknown remote) arrived and `poll_peers` is due.
    /// The transport thread cannot run the broker round trip itself without turning this change
    /// into Option A, so it raises this flag and the next `StartProcessing` performs the existing
    /// poll. Written and cleared under the global handle mutex.
    poll_requested: bool,
    /// Option A step (broker thread): unique id of this Network box. A peer-poll result carries
    /// its epoch, so a response that arrives after a leave/re-join can never be applied to a
    /// different Network box that happens to reuse the network id.
    poll_epoch: u64,
    /// One peer poll in flight at a time; cleared when its result is consumed (or swept).
    poll_inflight: bool,
    /// When the in-flight poll was dispatched; the watchdog in `poll_peers_request` re-arms it.
    poll_sent_at: Instant,
    /// Sequence of the last peer-poll result applied to this box; older responses are dropped.
    poll_applied_seq: u64,
}

/// CHANGE A (backlog D1): delivery ledger. Cumulative, per-type counts of every state change as
/// it is queued, handed to the title in a batch, and reclaimed by FinishProcessingStateChanges,
/// plus the drain's truncation/deferral/drop counters. Instrumentation only — nothing in the
/// delivery path reads these. All fields are written while the global handle mutex is held.
#[derive(Default)]
struct Ledger {
    queued: [u64; 32],
    handed: [u64; 32],
    reclaimed: [u64; 32],
    /// Drain cycles that handed out a non-empty batch, and the size of the most recent one.
    batches: u64,
    last_batch: u64,
    /// Changes the most recent FinishProcessingStateChanges reclaimed (non-null entries).
    last_reclaimed: u64,
    /// P5 counters: type-21s deferred behind a type-12, drain cap hits, and type-21s dropped at
    /// `PENDING_CAP` so `h.pending` cannot grow without bound.
    deferred21: u64,
    cap_hits: u64,
    dropped: u64,
}

/// P5 bound on `h.pending`. Type-21 payload messages are dropped at this depth; control
/// completions are always queued. 128 matches the receive buffer's cap and the value the audits
/// already record for `h.pending`.
const PENDING_CAP: usize = 128;
/// P5 timeout on the type-12 hold. A healthy hold lasts a tick or two (the type-12 is finished
/// before the next drain); after this, type-21s are delivered rather than deferred, so a type-12
/// that never arrives cannot silence the whole pump. 10 s is far beyond the healthy path.
const TYPE12_HOLD_TIMEOUT_MS: u64 = 10_000;

struct Handle {
    title: CString,
    users: Vec<*mut LocalUser>,
    networks: Vec<*mut Network>,
    pending: VecDeque<*mut u8>,
    in_flight: Vec<*mut u8>,
    /// CHANGE A (D1): per-type queued/handed/reclaimed attribution + drain counters.
    ledger: Ledger,
    /// True while the batch pointed to by `in_flight` has been handed to the title and not yet
    /// returned to FinishProcessingStateChanges. The SDK documents that array as library-allocated
    /// and the changes as valid until Finish, so nothing may rebuild, clear or reuse it in the
    /// meantime — a Start arriving before Finish must re-return the same array unchanged.
    batch_outstanding: bool,
    last_peer_poll: Instant,
}

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
unsafe impl Send for LocalUser {}
unsafe impl Sync for LocalUser {}
unsafe impl Send for Network {}
unsafe impl Sync for Network {}
unsafe impl Send for Endpoint {}
unsafe impl Sync for Endpoint {}

static NEXT_UID: AtomicU16 = AtomicU16::new(1);
static NEXT_SC: AtomicU64 = AtomicU64::new(1);
/// True once this process called PartyCreateNewNetwork (i.e. it is the host).
static IS_HOST: AtomicBool = AtomicBool::new(false);
/// Last probe_session log key so we only print on change.
static LAST_PROBE_KEY: AtomicU64 = AtomicU64::new(u64::MAX);
/// Last time probe_session actually logged. The probe used to be called only when the process held
/// at least one Party network, so the guest (which never completes a connect) produced no probe
/// output at all — exactly the side whose state we needed. It is now called unconditionally and
/// emits a heartbeat at least this often, so the whole 30-70 s failure window is covered rather
/// than the ~1 s of ticks the mesh-start timeline spans.
static LAST_PROBE_LOG_AT: OnceLock<Mutex<Instant>> = OnceLock::new();
static TYPE12_HELD_LOG: AtomicBool = AtomicBool::new(false);
static TYPE12_MEMBER_HELD_LOG: AtomicBool = AtomicBool::new(false);
static GUEST_SUB5_SENT: AtomicBool = AtomicBool::new(false);
static HOST_SNAPSHOT: AtomicBool = AtomicBool::new(false);
static SUB7_SENT: AtomicBool = AtomicBool::new(false);
static SUB7_RECV: AtomicBool = AtomicBool::new(false);
static SUB7_APPLIED: AtomicBool = AtomicBool::new(false);
static NATIVE_CONNECT: AtomicBool = AtomicBool::new(false);
/// Descriptor known (PartyDeserializeNetworkDescriptor succeeded) and the
/// mesh-start leaf has not been attempted yet for this join attempt.
static MESH_TRIGGER_PENDING: AtomicBool = AtomicBool::new(false);
/// The mesh-start leaf was called (or the guard latched mode 3) for this descriptor.
static MESH_TRIGGER_DONE: AtomicBool = AtomicBool::new(false);
/// Remaining StartProcessing ticks that still log the trigger timeline.
static MESH_TRIGGER_TICKS: AtomicU32 = AtomicU32::new(0);
/// Change key for the trigger / refusal / timeline log lines.
static MESH_TRIGGER_KEY: AtomicU64 = AtomicU64::new(u64::MAX);
/// Bitmask of Party exports the exe called since the last descriptor.
static MESH_EXPORTS: AtomicU64 = AtomicU64::new(0);
static TYPE12_MS: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn is_chara_snapshot(op: u32, len: usize) -> bool {
    op == 2 && len >= 600
}

fn note_snapshot() {
    HOST_SNAPSHOT.store(true, Ordering::SeqCst);
    let _ = SNAPSHOT_MS.compare_exchange(0, now_ms(), Ordering::SeqCst, Ordering::SeqCst);
}
static G: OnceLock<Mutex<Option<Box<Handle>>>> = OnceLock::new();
static ERR_MSG: OnceLock<CString> = OnceLock::new();

fn g() -> &'static Mutex<Option<Box<Handle>>> {
    G.get_or_init(|| Mutex::new(None))
}

fn cstr_ptr(s: &CString) -> *const c_char {
    s.as_ptr()
}

fn read_cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0usize;
    unsafe {
        while n < 4096 && *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, n)).into_owned()
    }
}

fn uuid_ident() -> String {
    let n = NEXT_SC.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (t >> 32) as u32,
        ((t >> 16) as u16),
        (t as u16) & 0xfff,
        (n as u16) & 0xfff,
        n.wrapping_mul(0x9e37) as u64 & 0xffff_ffff_ffff
    )
}

/// `[12x1 21x6]` for the nonzero entries of one per-type ledger row. Only called while a ledger
/// line is being emitted.
fn fmt_type_counts(counts: &[u64; 32]) -> String {
    let mut parts = Vec::new();
    for (ty, n) in counts.iter().enumerate() {
        if *n > 0 {
            parts.push(format!("{ty}x{n}"));
        }
    }
    if parts.is_empty() {
        "-".into()
    } else {
        parts.join(" ")
    }
}

/// Read-only summary of every active type-12 hold, for the ledger line.
fn type12_hold_note(h: &Handle) -> String {
    let mut parts = Vec::new();
    for n in h.networks.iter() {
        if n.is_null() {
            continue;
        }
        let n = unsafe { &**n };
        if n.type10_delivered && !n.type12_delivered && !n.remotes.is_empty() {
            parts.push(format!(
                "id={} remotes={} held_ms={}",
                n.descriptor.id_str(),
                n.remotes.len(),
                n.type12_hold_start
                    .map(|t| t.elapsed().as_millis())
                    .unwrap_or(0)
            ));
        }
    }
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(" ")
    }
}

/// The delivery-ledger line. Change events call `ledger_line` directly; the periodic heartbeat
/// and `Finish` call it only after their gate said yes, so the depth and per-type joins are never
/// built for a line that will not be emitted (the probe-cost rule).
fn ledger_text(h: &Handle, tag: &str) -> String {
    format!(
        "delivery ledger[{tag}] pending={} in_flight={} batches={} last_out={} last_reclaimed={} queued=[{}] out=[{}] back=[{}] deferred21={} cap_hits={} dropped={} type12_hold={} hb_skipped={}",
        h.pending.len(),
        h.in_flight.len(),
        h.ledger.batches,
        h.ledger.last_batch,
        h.ledger.last_reclaimed,
        fmt_type_counts(&h.ledger.queued),
        fmt_type_counts(&h.ledger.handed),
        fmt_type_counts(&h.ledger.reclaimed),
        h.ledger.deferred21,
        h.ledger.cap_hits,
        h.ledger.dropped,
        type12_hold_note(h),
        LEDGER_HB_SKIPPED.load(Ordering::Relaxed),
    )
}

fn ledger_line(h: &Handle, tag: &str) {
    debug_log(&ledger_text(h, tag));
}

/// P5 bound on `h.pending`: drop a type-21 when the queue is at `PENDING_CAP`, with a
/// rate-limited line. Returns true when the change was dropped. Control completions are never
/// dropped — a missing completion is a contract break the title waits on; type-21 is the one
/// unbounded producer (up to 64 KiB per message), so it is the one that is bounded.
fn pending_cap_drop(h: &mut Handle) -> bool {
    if h.pending.len() < PENDING_CAP {
        return false;
    }
    h.ledger.dropped += 1;
    let depth = h.pending.len();
    let dropped = h.ledger.dropped;
    log_throttled_lazy("pending_cap", |_| {
        format!(
            "h.pending cap {PENDING_CAP} hit: dropped a type-21 (pending={depth} dropped_total={dropped}); control state changes are never dropped"
        )
    });
    true
}

/// P5: the type-12 hold. While a network has remotes but the title has not finished that remote's
/// type-12, the drain normally defers type-21s so the title inserts the Party peer first. This
/// timestamps each hold and clears it when the condition goes away. Returns `(holding,
/// timed_out, oldest_held_ms)`: `holding` is true when a hold is still inside
/// `TYPE12_HOLD_TIMEOUT_MS` (keep deferring), `timed_out` when it has outlived it (deliver the
/// type-21s rather than starve the game of every message). The timeout is logged loudly once per
/// hold.
fn note_type12_hold(h: &mut Handle) -> (bool, bool, u64) {
    let now = Instant::now();
    let mut holding = false;
    let mut timed_out = false;
    let mut oldest_ms = 0u64;
    for net in h.networks.iter_mut() {
        if net.is_null() {
            continue;
        }
        let n = unsafe { &mut **net };
        if !(n.type10_delivered && !n.type12_delivered && !n.remotes.is_empty()) {
            n.type12_hold_start = None;
            n.type12_hold_timeout_logged = false;
            continue;
        }
        let start = *n.type12_hold_start.get_or_insert(now);
        let held_ms = now.duration_since(start).as_millis() as u64;
        oldest_ms = oldest_ms.max(held_ms);
        if held_ms >= TYPE12_HOLD_TIMEOUT_MS {
            timed_out = true;
            if !n.type12_hold_timeout_logged {
                n.type12_hold_timeout_logged = true;
                // Loud, once per hold: from here on the type-21s are delivered despite the
                // missing type-12, so the game stops receiving nothing at all.
                log_line(&format!(
                    "TYPE12 HOLD TIMEOUT id={} held_ms={held_ms} remotes={} timeout_ms={TYPE12_HOLD_TIMEOUT_MS}: delivering type-21 now instead of deferring (P5); type12_delivered never happened",
                    n.descriptor.id_str(),
                    n.remotes.len(),
                ));
            }
        } else {
            holding = true;
        }
    }
    (holding, timed_out, oldest_ms)
}

/// Allocate one zeroed state-change record from Rust's global heap; the first 4 bytes hold the
/// type tag. The block is intentionally leaked: it is handed to the title in a batch and there
/// is currently no reclaim path (see TODO.md). Size comes from the caller's fixed offsets, so
/// the allocation can never be smaller than the writes that follow it.
unsafe fn alloc_sc(size: usize, ty: u32) -> *mut u8 {
    let layout = std::alloc::Layout::from_size_align(size, 8).unwrap();
    let p = std::alloc::alloc_zeroed(layout);
    if !p.is_null() {
        ptr::write_unaligned(p as *mut u32, ty);
    }
    p
}

fn queue_sc(h: &mut Handle, p: *mut u8) {
    if p.is_null() {
        return;
    }
    let ty = unsafe { ptr::read_unaligned(p as *const u32) };
    // P5 bound: only type-21 payload messages are dropped at the cap (see `pending_cap_drop`).
    if ty == 21 && pending_cap_drop(h) {
        return;
    }
    if (ty as usize) < h.ledger.queued.len() {
        h.ledger.queued[ty as usize] += 1;
    }
    h.pending.push_back(p);
    if ty == 12 {
        // D1: the queue half of the type-12 lifecycle, with the live depth. "Queued but never
        // handed out" is the exact shape of the last silent stall.
        ledger_line(h, "type12-queued");
    }
}

fn lan_ip() -> [u8; 4] {
    if let Some(raw) = lan_cfg().advertise_ip.as_deref() {
        if let Ok(ip) = raw.parse::<Ipv4Addr>() {
            if !ip.is_loopback() && !ip.is_unspecified() {
                return ip.octets();
            }
        }
    }
    // Prefer the NIC used to reach the stub when it is not loopback (other PCs).
    let (host, port) = stub_host();
    if let Some(s) = connect_stub(&host, port) {
        if let Ok(SocketAddr::V4(v)) = s.local_addr() {
            if !v.ip().is_loopback() {
                return v.ip().octets();
            }
        }
    }
    // Host talking to a local stub still has to advertise a LAN address for UDP.
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("1.1.1.1:53").is_ok() {
            if let Ok(SocketAddr::V4(v)) = s.local_addr() {
                if !v.ip().is_loopback() {
                    return v.ip().octets();
                }
            }
        }
    }
    [127, 0, 0, 1]
}

fn ip_string(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

fn is_loopback_ip(s: &str) -> bool {
    let s = s.trim();
    s == "127.0.0.1" || s == "::1" || s == "0.0.0.0" || s.starts_with("127.")
}

fn parse_lan1(raw: &str) -> (String, Option<[u8; 4]>) {
    let rest = raw.strip_prefix("LAN1.").unwrap_or(raw);
    let mut parts = rest.split('.');
    let id = parts.next().unwrap_or(rest).to_string();
    let ip = parts.next().and_then(|h| {
        if h.len() != 8 {
            return None;
        }
        let n = u32::from_str_radix(h, 16).ok()?;
        Some([
            (n >> 24) as u8,
            (n >> 16) as u8,
            (n >> 8) as u8,
            n as u8,
        ])
    });
    (id, ip)
}

fn desc_set_advert_ip(d: &mut Descriptor, ip: [u8; 4]) {
    d.opaque[2..6].copy_from_slice(&ip);
}

fn desc_advert_ip(d: &Descriptor) -> Option<[u8; 4]> {
    let ip = [d.opaque[2], d.opaque[3], d.opaque[4], d.opaque[5]];
    if ip == [0, 0, 0, 0] || ip[0] == 127 {
        None
    } else {
        Some(ip)
    }
}

fn rewrite_peer_ip(raw: String, desc: &Descriptor) -> String {
    if !is_loopback_ip(&raw) {
        return raw;
    }
    if let Some(ip) = desc_advert_ip(desc) {
        return ip_string(ip);
    }
    raw
}

fn bind_udp() -> Option<(UdpSocket, u16)> {
    let want = lan_cfg().udp_port;
    let primary = if want == 0 {
        "0.0.0.0:0".to_string()
    } else {
        format!("0.0.0.0:{want}")
    };
    let sock = match UdpSocket::bind(&primary) {
        Ok(s) => s,
        Err(_) if want != 0 => {
            log_line(&format!("bind_udp fallback ephemeral ({want} busy)"));
            UdpSocket::bind("0.0.0.0:0").ok()?
        }
        Err(_) => return None,
    };
    sock.set_nonblocking(true).ok()?;
    let port = sock.local_addr().ok()?.port();
    Some((sock, port))
}

fn register_member(net: &Network, entity: &str) {
    register_member_impl(net, entity, false);
}

/// `quiet` skips the log line: the 10s keep-alive re-register (broker
/// expires members after 30s without a /party/join refresh) would
/// otherwise spam the log. The request itself is built and sent by the broker thread; this only
/// copies the fields it needs and enqueues.
fn register_member_impl(net: &Network, entity: &str, quiet: bool) {
    dispatch_broker(BrokerTask::Register {
        network_id: net.descriptor.id_str(),
        entity: entity.to_string(),
        udp_port: net.udp_port,
        quiet,
    });
}

fn local_entity(n: &Network) -> String {
    if n.local_user.is_null() {
        String::new()
    } else {
        unsafe { (*n.local_user).entity.to_string_lossy().into_owned() }
    }
}

/// Type 12 `FUN_142515340`: network at SC+8 must equal session+0x90 (filled by
/// type 3 from SC+0x180). If we emit EndpointCreated before that copy, the exe
/// returns without inserting the peer and the shim never retries (eid already
/// in `remotes`). Host then never sees the guest → 14C after timeout.
fn ensure_remote(
    h: &mut Handle,
    net: *mut Network,
    eid: &str,
    ip: String,
    udp_port: u16,
) -> bool {
    if net.is_null() || eid.is_empty() {
        return false;
    }
    let n = unsafe { &mut *net };
    if !n.connect_completed {
        return false;
    }
    let local_ent = local_entity(n);
    if eid == local_ent {
        return false;
    }
    // Type 12 must not race type 10: session+0x90 is filled from type 3,
    // type 10 stores the local endpoint. Emitting type 12 in the same pump
    // as type 10 hung the guest inside ProcessParty. Wait until the exe
    // has finished the type-10 SC.
    if n.local_endpoint.is_null() || !n.type10_delivered {
        if !TYPE12_HELD_LOG.swap(true, Ordering::Relaxed) {
            debug_log(&format!(
                "ensure_remote hold type12 entity={eid} until type10 delivered"
            ));
        }
        return false;
    }
    // Fast path first: a peer that is already registered needs neither the game's member-list walk
    // nor its insert gate. That walk reads the exe's live member list, and the receive path now
    // runs on the transport thread, so only genuinely new peers may touch it.
    if let Some(&ep) = n.remotes.get(eid) {
        unsafe {
            if !ep.is_null() {
                let e = &mut *ep;
                if udp_port != 0 && (e.ip != ip || e.udp_port != udp_port) {
                    // Adopt any real (ip, port) change, not just the initial
                    // fill-in: a guest that rebound to an ephemeral port
                    // (fixed port still busy) must stay reachable.
                    if e.udp_port != 0 && !is_loopback_ip(&e.ip) {
                        debug_log(&format!(
                            "remote endpoint moved entity={eid} old={}:{} new={ip}:{udp_port}",
                            e.ip, e.udp_port
                        ));
                    }
                    e.ip = ip;
                    e.udp_port = udp_port;
                }
            }
        }
        return false;
    }
    // Matching poll waits for a work item per lobby member. After native
    // Connect, FUN_140260b30 inserts that row — hold type 12 until it exists.
    // Plant only on skip-connect (exe never ConnectToNetwork).
    if !unsafe { member_list_has_entity(eid) } {
        if !TYPE12_MEMBER_HELD_LOG.swap(true, Ordering::Relaxed) {
            debug_log(&format!(
                "ensure_remote hold type12 entity={eid} until native FUN_140260b30 insert"
            ));
        }
        return false;
    }
    if n.remotes.len() >= 8 {
        log_line(&format!(
            "WATCH_4 poll skip entity={eid} remotes={} (shim remote cap 8)",
            n.remotes.len()
        ));
        return false;
    }
    let uid = NEXT_UID.fetch_add(1, Ordering::Relaxed);
    let boxed = Box::new(Endpoint {
        entity: CString::new(eid.to_string()).unwrap_or_else(|_| CString::new("x").unwrap()),
        uid,
        network: net,
        ip: ip.clone(),
        udp_port,
        last_seq: 0,
        seq_seen: false,
        tx: reliable::Sender::new(),
        rx: reliable::Receiver::new(),
    });
    let ep = Box::into_raw(boxed);
    n.remotes.insert(eid.to_string(), ep);
    unsafe { send_transport_control(net, KIND_HELLO) };
    let sc = unsafe { alloc_sc(0x20, 12) };
    if !sc.is_null() {
        unsafe {
            ptr::write_unaligned(sc.add(8) as *mut *mut Network, net);
            ptr::write_unaligned(sc.add(0x10) as *mut *mut Endpoint, ep);
        }
        queue_sc(h, sc);
        debug_log(&format!(
            "EndpointCreated remote entity={eid} uid={uid} ip={ip}:{udp_port}"
        ));
        log_remote_count(n.remotes.len(), eid, uid);
    }
    // Do not replay type 21 in the same pump as type 12: the exe inserts the
    // Party peer during FinishProcessing of type 12. flush_pending_rx waits
    // for `type12_delivered`.
    true
}

/// Ask the broker thread for the current member list, and refresh the 10s keep-alive. No I/O
/// here: a few string copies and one enqueue, so the tick never blocks on the broker.
fn poll_peers_request(net: *mut Network) {
    if net.is_null() {
        return;
    }
    let n = unsafe { &mut *net };
    if !n.connect_completed {
        return;
    }
    // Keep-alive: broker drops members not re-joined within 30s, which hides
    // the lobby from FindLobbies (lobby_owner_in_party). Refresh every 10s.
    if n.last_register.elapsed() >= Duration::from_secs(10) {
        n.last_register = Instant::now();
        let ent = local_entity(n);
        if !ent.is_empty() {
            register_member_impl(n, &ent, true);
        }
    }
    if n.poll_inflight {
        // Watchdog: a swept result (box re-added by AuthenticateLocalUser) or a broker task
        // that never returned must not latch the poll off forever.
        if n.poll_sent_at.elapsed() < Duration::from_millis(PEER_POLL_TIMEOUT_MS) {
            return;
        }
        n.poll_inflight = false;
        log_throttled("poll_watchdog", "peer poll exceeded 10s; re-arming");
    }
    n.poll_inflight = true;
    n.poll_sent_at = Instant::now();
    let seq = NEXT_POLL_SEQ.fetch_add(1, Ordering::Relaxed);
    dispatch_broker(BrokerTask::PollPeers {
        network_id: n.descriptor.id_str(),
        epoch: n.poll_epoch,
        seq,
    });
}

/// Apply a completed poll on the tick, under the global handle mutex (the result mutates the
/// remote set, so it must not be applied from the broker thread). The epoch names the Network box
/// that asked, so a result that arrives after a leave/re-join can never be applied to a different
/// box; the sequence drops an older response.
fn poll_peers_apply(h: &mut Handle, net: *mut Network) {
    if net.is_null() {
        return;
    }
    let n = unsafe { &mut *net };
    if !n.connect_completed {
        return;
    }
    let Some(res) = take_peer_result(n.poll_epoch) else {
        return;
    };
    n.poll_inflight = false;
    // Belt-and-braces: with one poll in flight per box this cannot trigger, but it keeps a
    // duplicated/late result from regressing the applied generation if that ever changes.
    if res.seq <= n.poll_applied_seq {
        return;
    }
    n.poll_applied_seq = res.seq;
    for m in res.members {
        let ip = rewrite_peer_ip(m.ip, &n.descriptor);
        ensure_remote(h, net, &m.entity, ip, m.udp_port);
    }
}

/// Queue a type-21 EndpointMessageReceived state change for `payload` from
/// remote `ep`, delivered to the local endpoint of `net`. Layout matches what
/// the exe's state-change switch reads (see the recv loop for field offsets).
/// # Safety
/// Builds a type-21 state change in a fresh zeroed allocation and copies the payload into it.
/// `h`/`net`/`ep` are live Box-owned objects owned by the global state (hold the handle mutex
/// while calling); offsets match the exe's handler. The allocation is intentionally leaked
/// until FinishProcessing (see TODO.md).
unsafe fn deliver_type21(
    h: &mut Handle,
    net: *mut Network,
    ep: *mut Endpoint,
    payload: &[u8],
    recv_opts: u32,
) {
    // P5: enforce the queue bound before allocating up to 64 KiB of payload that `queue_sc`
    // would only drop. `queue_sc` keeps its own check so every producer stays bounded.
    if pending_cap_drop(h) {
        return;
    }
    let sc = alloc_sc(0x40 + payload.len(), 21);
    if sc.is_null() {
        return;
    }
    ptr::write_unaligned(sc.add(8) as *mut *mut Network, net);
    ptr::write_unaligned(sc.add(0x10) as *mut *mut Endpoint, ep);
    ptr::write_unaligned(sc.add(0x18) as *mut u32, 1);
    let recv_slot = sc.add(0x38) as *mut *mut Endpoint;
    ptr::write(recv_slot, (*net).local_endpoint);
    ptr::write_unaligned(sc.add(0x20) as *mut *mut Endpoint, recv_slot as *mut Endpoint);
    ptr::write_unaligned(sc.add(0x28) as *mut u32, recv_opts);
    ptr::write_unaligned(sc.add(0x2C) as *mut u32, payload.len() as u32);
    let dest = sc.add(0x40);
    ptr::copy_nonoverlapping(payload.as_ptr(), dest, payload.len());
    ptr::write_unaligned(sc.add(0x30) as *mut *mut u8, dest);
    queue_sc(h, sc);
    let op = u32le(payload, 0).unwrap_or(0);
    if is_chara_snapshot(op, payload.len()) {
        note_snapshot();
        debug_log(&format!("snapshot_delivered opcode={op} len={}", payload.len()));
    }
}

/// Replay buffered inbound frames once the local endpoint exists and the
/// sender's remote endpoint is registered. Stops at the first entry whose
/// remote endpoint is still unknown to preserve the game's RPC ordering.
unsafe fn flush_pending_rx(h: &mut Handle, net: *mut Network) {
    if net.is_null() {
        return;
    }
    let mut count = 0usize;
    loop {
        let n = &mut *net;
        if n.local_endpoint.is_null() || !n.type12_delivered {
            break;
        }
        let Some((ent, _, _, _)) = n.pending_rx.front() else {
            break;
        };
        let ep = n.remotes.get(ent).copied().unwrap_or(ptr::null_mut());
        if ep.is_null() {
            // Remote endpoint not registered yet; do not reorder past it.
            break;
        }
        let (_, payload, send_opts, seq) = n.pending_rx.pop_front().unwrap();
        if reliable::PARTY_RELIABLE {
            deliver_reliable(h, net, ep, seq, send_opts, payload);
        } else {
            deliver_type21(h, net, ep, &payload, recv_opts_for(send_opts));
        }
        count += 1;
    }
    if count > 0 {
        debug_log(&format!("recv replay n={count}"));
    }
}

/// `PartyMessageReceivedOptions` from what we actually provided. Under `PARTY_RELIABLE` a
/// guaranteed message really is retransmitted until acked and sequential messages really are
/// ordered, so those bits are truthful. Every message is a single datagram, so `FRAGMENTED` is
/// never set.
fn recv_opts_for(send_opts: u32) -> u32 {
    if reliable::PARTY_RELIABLE {
        send_opts & (RECV_GUARANTEED | RECV_SEQUENTIAL)
    } else if send_opts & SEND_SEQUENTIAL != 0 {
        RECV_SEQUENTIAL
    } else {
        0
    }
}

/// Run one inbound message through the reliable receiver state machine and hand every payload it
/// releases to the game, in order. Returns true when at least one payload was delivered.
unsafe fn deliver_reliable(
    h: &mut Handle,
    net: *mut Network,
    ep: *mut Endpoint,
    seq: u32,
    send_opts: u32,
    payload: Vec<u8>,
) -> bool {
    let e = &mut *ep;
    let mode = reliable::mode_index(send_opts);
    let outcome = e.rx.on_message(seq, send_opts, payload, now_ms());
    if let Some(ev) = e.rx.last_evicted {
        rel_bump(mode, |c| c.evicted += 1);
        log_throttled(
            "reliable_evict",
            &format!(
                "reliable out-of-order evict seq={ev} mode={} cap={} (highest queued dropped to stay bounded)",
                reliable::mode_name(mode),
                reliable::OOO_CAP
            ),
        );
    }
    match outcome {
        RecvOutcome::Delivered(msgs) => {
            for m in msgs {
                deliver_type21(h, net, ep, &m.payload, recv_opts_for(m.options));
            }
            true
        }
        RecvOutcome::Queued { seq, expected } => {
            rel_bump(mode, |c| c.ooo_queued += 1);
            log_throttled(
                "recv_gap",
                &format!(
                    "recv gap seq={seq} expected={expected} buffered={} (waiting for retransmit)",
                    e.rx.buffered_len()
                ),
            );
            false
        }
        RecvOutcome::DroppedDuplicate => {
            rel_bump(mode, |c| c.dup_dropped += 1);
            false
        }
        RecvOutcome::DroppedOutOfOrder { seq, high } => {
            rel_bump(mode, |c| c.ooo_dropped += 1);
            log_throttled(
                "recv_ooo_drop",
                &format!(
                    "dropped out-of-order best-effort seq={seq} high={high} (never buffered)"
                ),
            );
            false
        }
    }
}

/// Bounded hand-off for a peer first seen on the wire. See `drain_pending_peers`.
fn queue_pending_peer(net: *mut Network, eid: &str, ip: String, udp_port: u16) {
    if net.is_null() {
        return;
    }
    let n = unsafe { &mut *net };
    if n.pending_peers.iter().any(|(e, _, _)| e == eid) {
        return;
    }
    if n.pending_peers.len() >= 16 {
        log_throttled(
            "pending_peer_cap",
            "pending new-peer queue full (16); dropping a candidate (next datagram retries)",
        );
        return;
    }
    n.pending_peers.push_back((eid.to_string(), ip, udp_port));
}

/// Apply candidates queued by the transport thread's receive path. Runs on the tick, under the
/// global handle mutex, so `ensure_remote`'s member-list walk stays on the game's thread.
fn drain_pending_peers(h: &mut Handle, net: *mut Network) {
    if net.is_null() {
        return;
    }
    loop {
        let next = {
            let n = unsafe { &mut *net };
            n.pending_peers.pop_front()
        };
        match next {
            Some((eid, ip, udp_port)) => {
                ensure_remote(h, net, &eid, ip, udp_port);
            }
            None => break,
        }
    }
}

fn recv_udp(h: &mut Handle) {
    let nets: Vec<*mut Network> = h.networks.clone();
    // Bounded batch: a flooded socket must not hold the global handle mutex across an unbounded
    // drain. Whatever is left stays in the socket buffer for the next cycle.
    let mut budget = RECV_BATCH;
    for net in nets {
        unsafe { flush_pending_rx(h, net) };
        let n = unsafe { &mut *net };
        let Some(sock) = n.udp.as_ref() else {
            continue;
        };
        let mut buf = [0u8; 65536];
        loop {
            if budget == 0 {
                break;
            }
            match sock.recv_from(&mut buf) {
                Ok((len, src)) => {
                    budget -= 1;
                    XPORT_RX.fetch_add(1, Ordering::Relaxed);
                    if len < HDR_LEN || &buf[..4] != MAGIC || buf[5] != HDR_VERSION {
                        // No legacy fallback: a datagram without our magic/version is a protocol
                        // mismatch. Log it once per peer and drop it rather than degrade silently.
                        log_wire_mismatch(&src, len, &buf[..len.min(16)]);
                        continue;
                    }
                    let kind = buf[4];
                    let payload_len = u32::from_le_bytes(buf[44..48].try_into().unwrap()) as usize;
                    if payload_len > 64 * 1024 || HDR_LEN + payload_len > len {
                        log_wire_mismatch(&src, len, &buf[..len.min(16)]);
                        continue;
                    }
                    let send_opts = u32::from_le_bytes(buf[48..52].try_into().unwrap());
                    let seq = u32::from_le_bytes(buf[52..56].try_into().unwrap());
                    let ack = if reliable::PARTY_RELIABLE {
                        u32::from_le_bytes(buf[HDR_ACK_OFF..HDR_ACK_OFF + 4].try_into().unwrap())
                    } else {
                        0
                    };
                    let ent_raw = &buf[24..45];
                    let ent_n = ent_raw.iter().position(|&b| b == 0).unwrap_or(20);
                    let ent = String::from_utf8_lossy(&ent_raw[..ent_n]).into_owned();
                    let (src_ip, src_port) = match src {
                        SocketAddr::V4(v) => (v.ip().to_string(), v.port()),
                        SocketAddr::V6(_) => (String::new(), 0),
                    };
                    if n.connect_completed && !src_ip.is_empty() {
                        if n.remotes.contains_key(&ent) {
                            // Known peer: shim-only address refresh. Never walk exe memory here.
                            ensure_remote(h, net, &ent, src_ip, src_port);
                        } else {
                            // Genuinely new peer: `ensure_remote` would walk the game's live
                            // member list, which must happen on the tick thread (as it did before
                            // the transport thread existed). Hand the candidate over instead.
                            queue_pending_peer(net, &ent, src_ip, src_port);
                        }
                    }
                    // Acks ride on every packet kind (messages, HELLO, standalone ACK).
                    if reliable::PARTY_RELIABLE && ack != 0 {
                        let ep = n.remotes.get(&ent).copied().unwrap_or(ptr::null_mut());
                        if !ep.is_null() {
                            for (_cseq, copts) in unsafe { (*ep).tx.on_ack(ack) } {
                                rel_bump(reliable::mode_index(copts), |c| c.acked += 1);
                            }
                        }
                    }
                    if kind == KIND_HELLO {
                        if should_log_payload(&format!("hello:{ent}")) {
                            debug_log(&format!("recv hello from={ent} remotes={}", n.remotes.len()));
                        }
                        unsafe { request_peer_poll(net) };
                        continue;
                    }
                    if kind == KIND_ACK {
                        continue;
                    }
                    if kind != KIND_MSG {
                        log_line(&format!("recv unknown kind={kind} from={ent} wire_len={len}"));
                        continue;
                    }
                    let payload = buf[HDR_LEN..HDR_LEN + payload_len].to_vec();
                    if !reliable::PARTY_RELIABLE && send_opts & SEND_GUARANTEED != 0 {
                        // PARTY_RELIABLE=false restores the exact pre-reliability behaviour,
                        // including this diagnostic.
                        log_throttled(
                            "guaranteed_wanted",
                            "sender requested GuaranteedDelivery; not implemented (best-effort only)",
                        );
                    }
                    log_payload(
                        "recv",
                        &format!("type=21 from={ent} remotes={}", n.remotes.len()),
                        &payload,
                    );
                    if n.remotes.get(&ent).copied().unwrap_or(ptr::null_mut()).is_null() {
                        unsafe { request_peer_poll(net) };
                    }
                    let ep = n.remotes.get(&ent).copied().unwrap_or(ptr::null_mut());
                    if !reliable::PARTY_RELIABLE {
                        // Pre-reliability sequential enforcement, verbatim.
                        if !ep.is_null() && send_opts & SEND_SEQUENTIAL != 0 {
                            let e = unsafe { &mut *ep };
                            let older = e.seq_seen
                                && (seq == e.last_seq
                                    || seq.wrapping_sub(e.last_seq) > 0x8000_0000);
                            if older {
                                log_throttled(
                                    "seq_drop",
                                    "dropped an older sequential message (sequence already moved past it)",
                                );
                                continue;
                            }
                            e.last_seq = seq;
                            e.seq_seen = true;
                        }
                    }
                    if ep.is_null() || n.local_endpoint.is_null() || !n.type12_delivered {
                        // Buffer instead of dropping: the host's initial
                        // matching RPC batch can land before the exe calls
                        // PartyNetworkCreateEndpoint, and type 21 is ignored
                        // until type 12 has inserted the Party peer.
                        if n.pending_rx.len() >= 128 {
                            if let Some((old_ent, old_payload, _, _)) = n.pending_rx.pop_front() {
                                debug_log(&format!(
                                    "recv evict pending entity={old_ent} len={} cap=128",
                                    old_payload.len()
                                ));
                            }
                        }
                        debug_log(&format!(
                            "recv buffered opcode pending entity={ent} ep_null={} local_ep_null={} type12={} remotes={} queued={}",
                            ep.is_null(),
                            n.local_endpoint.is_null(),
                            n.type12_delivered,
                            n.remotes.len(),
                            n.pending_rx.len() + 1
                        ));
                        n.pending_rx.push_back((ent, payload, send_opts, seq));
                        continue;
                    }
                    if !n.pending_rx.is_empty() {
                        // Deliverable, but older frames are still queued; keep
                        // FIFO order by appending and flushing from the front.
                        n.pending_rx.push_back((ent, payload, send_opts, seq));
                        unsafe { flush_pending_rx(h, net) };
                        continue;
                    }
                    if reliable::PARTY_RELIABLE {
                        unsafe { deliver_reliable(h, net, ep, seq, send_opts, payload) };
                    } else {
                        unsafe { deliver_type21(h, net, ep, &payload, recv_opts_for(send_opts)) };
                    }
                }
                Err(_) => break,
            }
        }
    }
}

/// Build one wire datagram. Field offsets are stable between v2 and v3; the cumulative ack is
/// only written by the reliable build.
fn wire_packet(
    net_id: &str,
    local_ent: &[u8],
    kind: u8,
    options: u32,
    seq: u32,
    ack: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut pkt = vec![0u8; HDR_LEN + payload.len()];
    pkt[..4].copy_from_slice(MAGIC);
    pkt[4] = kind;
    pkt[5] = HDR_VERSION;
    let ib = net_id.as_bytes();
    pkt[8..8 + ib.len().min(16)].copy_from_slice(&ib[..ib.len().min(16)]);
    let n = local_ent.len().min(20);
    pkt[24..24 + n].copy_from_slice(&local_ent[..n]);
    pkt[44..48].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    pkt[48..52].copy_from_slice(&options.to_le_bytes());
    pkt[52..56].copy_from_slice(&seq.to_le_bytes());
    if reliable::PARTY_RELIABLE {
        pkt[HDR_ACK_OFF..HDR_ACK_OFF + 4].copy_from_slice(&ack.to_le_bytes());
    }
    pkt[HDR_LEN..].copy_from_slice(payload);
    pkt
}

fn ep_addr(e: &Endpoint) -> Option<SocketAddr> {
    if e.udp_port == 0 {
        return None;
    }
    format!("{}:{}", e.ip, e.udp_port).parse::<SocketAddr>().ok()
}

fn send_udp_all(net: &mut Network, kind: u8, payload: &[u8], options: u32) {
    let Some(sock) = net.udp.as_ref() else {
        return;
    };
    let local_ent = if net.local_user.is_null() {
        Vec::new()
    } else {
        unsafe { (*net.local_user).entity.as_bytes().to_vec() }
    };
    let id = net.descriptor.id_str();
    if !reliable::PARTY_RELIABLE {
        // Exact pre-reliability broadcast: one global sequence counter for sequential sends, no
        // acks, one shared packet sent to every remote.
        let seq = if options & SEND_SEQUENTIAL != 0 {
            SEND_SEQ.fetch_add(1, Ordering::Relaxed)
        } else {
            0
        };
        let pkt = wire_packet(&id, &local_ent, kind, options, seq, 0, payload);
        for ep in net.remotes.values() {
            let e = unsafe { &**ep };
            if let Some(addr) = ep_addr(e) {
                udp_send_to(sock, &pkt, addr);
            }
        }
        return;
    }
    let now = now_ms();
    for ep in net.remotes.values_mut() {
        let e = unsafe { &mut **ep };
        let Some(addr) = ep_addr(e) else {
            continue;
        };
        let (seq, ack, out_payload) = if kind == KIND_MSG {
            // Per-pairing sequence spaces: every remote gets its own guaranteed and best-effort
            // sequential counters, so retransmit state and acks never cross endpoints.
            let out = e.tx.send(payload.to_vec(), options, now);
            rel_bump(reliable::mode_index(options), |c| c.sent += 1);
            if let Some((ev_seq, ev_opts)) = out.evicted {
                rel_bump(reliable::mode_index(ev_opts), |c| c.evicted += 1);
                log_throttled(
                    "reliable_evict",
                    &format!(
                        "reliable pending evict seq={ev_seq} mode={} cap={} (unacked message dropped)",
                        reliable::mode_name(reliable::mode_index(ev_opts)),
                        reliable::PENDING_CAP
                    ),
                );
            }
            let ack = e.rx.piggyback_ack();
            (out.seq, ack, out.payload)
        } else {
            // HELLO carries no sequence, but is return traffic, so piggyback the current ack.
            let ack = e.rx.piggyback_ack();
            (0, ack, payload.to_vec())
        };
        let pkt = wire_packet(&id, &local_ent, kind, options, seq, ack, &out_payload);
        udp_send_to(sock, &pkt, addr);
    }
}

/// Per-tick reliable service: retransmit guaranteed messages whose RTO expired and emit forced
/// standalone acks. Runs under the global handle lock from PartyStartProcessingStateChanges, so it
/// needs no additional synchronisation.
fn reliable_service(h: &mut Handle, now: u64) {
    if !reliable::PARTY_RELIABLE {
        return;
    }
    for &netp in h.networks.iter() {
        let n = unsafe { &mut *netp };
        let Some(sock) = n.udp.as_ref() else {
            continue;
        };
        let id = n.descriptor.id_str();
        let local_ent = if n.local_user.is_null() {
            Vec::new()
        } else {
            unsafe { (*n.local_user).entity.as_bytes().to_vec() }
        };
        for &epp in n.remotes.values() {
            let e = unsafe { &mut *epp };
            let Some(addr) = ep_addr(e) else {
                continue;
            };
            for r in e.tx.due(now) {
                if r.gave_up {
                    log_throttled(
                        "retry_gaveup",
                        &format!(
                            "reliable seq={} mode={} exhausted {} retries; no longer retransmitting (kept pending until ack/evict)",
                            r.seq,
                            reliable::mode_name(reliable::mode_index(r.options)),
                            reliable::MAX_RETRIES
                        ),
                    );
                    continue;
                }
                rel_bump(reliable::mode_index(r.options), |c| c.retransmitted += 1);
                if r.first && debug_enabled() {
                    // Per-seq detail; the retransmit totals stay on the msg_stats rollup.
                    log_throttled(
                        "first_retx",
                        &format!(
                            "reliable first retransmit seq={} mode={} retry={} to={addr}",
                            r.seq,
                            reliable::mode_name(reliable::mode_index(r.options)),
                            r.retries
                        ),
                    );
                }
                let ack = e.rx.piggyback_ack();
                let pkt = wire_packet(&id, &local_ent, KIND_MSG, r.options, r.seq, ack, &r.payload);
                udp_send_to(sock, &pkt, addr);
            }
            if let Some(ack) = e.rx.forced_ack(now) {
                let pkt = wire_packet(&id, &local_ent, KIND_ACK, 0, 0, ack, &[]);
                udp_send_to(sock, &pkt, addr);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Option B: dedicated transport thread
// ─────────────────────────────────────────────────────────────────────────────
//
// One thread per process, started lazily from the first Party export (never DllMain, so there is
// no loader-lock hazard), living for the process lifetime and never joined. It is the single
// consumer of the outbox below and the only path that reads from or writes to a UDP socket:
// game-facing exports copy the payload, enqueue and return, and
// PartyStartProcessingStateChanges only drains state changes that are already decoded. RTO
// retransmits and forced standalone acks are produced by this thread's own clock, so they no
// longer depend on how often — or whether — the title calls us.
//
// Scope note: broker HTTP is NOT done here (see the broker thread below). recv_udp raises
// `Network::poll_requested` instead of calling `poll_peers`, and the tick enqueues the poll.

/// One game-facing send. `net`/`ep` point at boxes the shim allocates with `Box::into_raw` and
/// never frees (see `PartyNetworkLeaveNetwork`/`PartyCleanup`), so they stay valid for the process
/// lifetime; every field the thread reads through them is protected by the global handle mutex
/// the thread takes each cycle.
struct OutJob {
    net: *mut Network,
    ep: *mut Endpoint,
    kind: u8,
    target_count: u32,
    options: u32,
    payload: Vec<u8>,
}

// The pointers are to shim-owned boxes that are never freed; the job merely crosses one thread
// boundary.
unsafe impl Send for OutJob {}

struct Outbox {
    q: Mutex<VecDeque<OutJob>>,
    cv: Condvar,
}

static OUTBOX: OnceLock<Outbox> = OnceLock::new();
static TRANSPORT_STARTED: OnceLock<()> = OnceLock::new();
/// True while a live transport consumer exists. If the spawn fails, or the thread dies, the send
/// path and `PartyStartProcessingStateChanges` fall back to the old inline transport instead of
/// queueing work nothing will drain.
static TRANSPORT_UP: AtomicBool = AtomicBool::new(false);

/// Transport-thread observables, reported by `transport_stats_note` on the periodic `msg_stats`
/// line and by the heartbeat. Counted unconditionally so no branch is dead in either build.
static XPORT_WAKEUPS: AtomicU64 = AtomicU64::new(0);
static XPORT_JOBS: AtomicU64 = AtomicU64::new(0);
static XPORT_RX: AtomicU64 = AtomicU64::new(0);
static XPORT_TX: AtomicU64 = AtomicU64::new(0);
static XPORT_HB_MS: AtomicU64 = AtomicU64::new(0);
static XPORT_TID: AtomicU32 = AtomicU32::new(0);
/// Outbox depth and high-water mark, so a producer burst (or a transport thread stalled behind the
/// tick's broker HTTP) is visible in the log instead of being an invisible queue.
static XPORT_OUTBOX_DEPTH: AtomicU64 = AtomicU64::new(0);
static XPORT_OUTBOX_HWM: AtomicU64 = AtomicU64::new(0);

fn outbox() -> &'static Outbox {
    OUTBOX.get_or_init(|| Outbox {
        q: Mutex::new(VecDeque::new()),
        cv: Condvar::new(),
    })
}

/// Test/field escape hatch: `GBFR_PARTY_FORCE_INLINE=1` disables both worker threads so the
/// call-driven fallback paths can be exercised deterministically (and the shim can still run if
/// the threads ever misbehave). Read once per process.
fn force_inline_threads() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("GBFR_PARTY_FORCE_INLINE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Start the transport thread on the first Party export call. `OnceLock::get_or_init` makes late
/// callers wait for the winner, so the thread exists before any send can be enqueued.
fn ensure_transport_thread() {
    if force_inline_threads() {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            debug_log(
                "transport disabled (GBFR_PARTY_FORCE_INLINE); using tick-driven inline transport",
            );
        }
        return;
    }
    TRANSPORT_STARTED.get_or_init(|| {
        match std::thread::Builder::new()
            .name("party-transport".into())
            .spawn(transport_thread_main)
        {
            Ok(_) => TRANSPORT_UP.store(true, Ordering::SeqCst),
            Err(e) => log_line(&format!(
                "transport thread spawn FAILED: {e}; using tick-driven inline transport"
            )),
        }
        ()
    });
}

/// Enqueue one send. The payload is copied here because the caller's buffer is only valid for the
/// duration of the export call that handed it to us.
fn enqueue_out(job: OutJob) {
    let ob = outbox();
    let depth = {
        let mut q = ob.q.lock().unwrap_or_else(|e| e.into_inner());
        q.push_back(job);
        q.len() as u64
    };
    XPORT_OUTBOX_DEPTH.store(depth, Ordering::Relaxed);
    XPORT_OUTBOX_HWM.fetch_max(depth, Ordering::Relaxed);
    ob.cv.notify_one();
}

/// Game-facing message send: enqueue and return. The transport thread performs the UDP send and
/// the diagnostics.
unsafe fn send_transport_msg(
    net: *mut Network,
    ep: *mut Endpoint,
    target_count: u32,
    payload: &[u8],
    options: u32,
) {
    if TRANSPORT_UP.load(Ordering::Acquire) {
        enqueue_out(OutJob {
            net,
            ep,
            kind: KIND_MSG,
            target_count,
            options,
            payload: payload.to_vec(),
        });
        return;
    }
    // No live consumer (spawn failed, or the thread died after `catch_unwind`): run the old
    // inline path, exactly as the pre-threading code did.
    note_send_diag(net, ep, target_count, payload, options);
    send_udp_all(&mut *net, KIND_MSG, payload, options);
}

/// Control datagram (HELLO): no payload, no sequence, no diagnostics; still return traffic, so
/// `send_udp_all` piggybacks the current ack.
unsafe fn send_transport_control(net: *mut Network, kind: u8) {
    if TRANSPORT_UP.load(Ordering::Acquire) {
        enqueue_out(OutJob {
            net,
            ep: ptr::null_mut(),
            kind,
            target_count: 0,
            options: 0,
            payload: Vec::new(),
        });
        return;
    }
    send_udp_all(&mut *net, kind, &[], 0);
}

/// Send-site diagnostics, moved verbatim out of `PartyEndpointSendMessage`. With the thread on
/// they run on the transport thread when the job is processed, still under the global handle
/// mutex.
unsafe fn note_send_diag(
    net: *mut Network,
    ep: *mut Endpoint,
    target_count: u32,
    payload: &[u8],
    options: u32,
) {
    let remotes = (*net).remotes.len();
    if target_count == 4 {
        debug_log(&format!(
            "WATCH_4 send target_count=4 remotes={remotes} len={}",
            payload.len()
        ));
    }
    log_payload(
        "send",
        &format!(
            "targets={target_count} remotes={remotes} from={}",
            (*ep).entity.to_string_lossy()
        ),
        payload,
    );
    // Once per distinct options word: decode exactly what the game asked for. This decides
    // whether the exe relies on ordering (0x2) as well as reliability (0x1).
    log_send_options_once(options);
    if payload.len() > 1400 {
        log_big_send_once(payload.len(), options);
    }
}

/// The thread's single consumer. Runs under the global handle mutex, which is what protects the
/// socket, the remote set and every Sender/Receiver pairing.
unsafe fn process_out_job(j: OutJob) {
    if j.net.is_null() {
        return;
    }
    if j.kind == KIND_MSG && !j.ep.is_null() {
        note_send_diag(j.net, j.ep, j.target_count, &j.payload, j.options);
    }
    send_udp_all(&mut *j.net, j.kind, &j.payload, j.options);
}

/// Drain one bounded batch of the outbox and send it. The caller must hold the global handle
/// mutex (the transport thread and, in fallback mode, the tick both do).
fn service_outbox() {
    let ob = outbox();
    let mut jobs = Vec::new();
    let remaining = {
        let mut q = ob.q.lock().unwrap_or_else(|e| e.into_inner());
        while jobs.len() < OUTBOX_BATCH {
            match q.pop_front() {
                Some(j) => jobs.push(j),
                None => break,
            }
        }
        q.len() as u64
    };
    XPORT_OUTBOX_DEPTH.store(remaining, Ordering::Relaxed);
    if !jobs.is_empty() {
        XPORT_JOBS.fetch_add(jobs.len() as u64, Ordering::Relaxed);
    }
    for j in jobs {
        unsafe { process_out_job(j) };
    }
}

/// Process every send queued for `net` before its socket is torn down, preserving per-network
/// FIFO. Jobs for other networks keep their order. Caller holds the global handle mutex.
unsafe fn drain_outbox_for(net: *mut Network) {
    let ob = outbox();
    let mut jobs = Vec::new();
    {
        let mut q = ob.q.lock().unwrap_or_else(|e| e.into_inner());
        let mut rest: VecDeque<OutJob> = VecDeque::with_capacity(q.len());
        while let Some(j) = q.pop_front() {
            if j.net == net {
                jobs.push(j);
            } else {
                rest.push_back(j);
            }
        }
        *q = rest;
        XPORT_OUTBOX_DEPTH.store(q.len() as u64, Ordering::Relaxed);
    }
    for j in jobs {
        process_out_job(j);
    }
}

/// The one place datagrams leave a socket.
/// fresh sends, retransmits and standalone acks alike.
fn udp_send_to(sock: &UdpSocket, pkt: &[u8], addr: SocketAddr) {
    if sock.send_to(pkt, addr).is_ok() {
        XPORT_TX.fetch_add(1, Ordering::Relaxed);
    }
}

/// The receive-path half of the HELLO-triggered poll. The receive path runs on the transport
/// thread and must not touch the broker queue itself, so it raises a flag; the next
/// `PartyStartProcessingStateChanges` enqueues the poll on the broker thread.
unsafe fn request_peer_poll(net: *mut Network) {
    if !net.is_null() {
        (*net).poll_requested = true;
    }
}

/// How long the transport thread may wait before its next timer: the earliest RTO/forced-ack
/// deadline, clamped to `TRANSPORT_POLL_MS` so unsolicited inbound datagrams are still noticed
/// promptly. `1` when a deadline has already passed, so the caller re-runs the timers at once.
fn next_transport_wait_ms(h: &Handle, now: u64) -> u64 {
    let mut best: Option<u64> = None;
    for &netp in h.networks.iter() {
        if netp.is_null() {
            continue;
        }
        let n = unsafe { &*netp };
        for &epp in n.remotes.values() {
            if epp.is_null() {
                continue;
            }
            let e = unsafe { &*epp };
            for d in [e.tx.next_rto_ms(), e.rx.next_ack_due_ms()]
                .into_iter()
                .flatten()
            {
                best = Some(best.map_or(d, |b: u64| b.min(d)));
            }
        }
    }
    let until = best
        .map(|d| d.saturating_sub(now))
        .unwrap_or(TRANSPORT_POLL_MS);
    until.clamp(1, TRANSPORT_POLL_MS)
}

/// Liveness line: proves the thread is running and shows what it has done, even on a run with no
/// game traffic at all. The due test runs before the string is built.
fn transport_heartbeat(now: u64) {
    let last = XPORT_HB_MS.load(Ordering::Relaxed);
    if last != 0 && now.wrapping_sub(last) < heartbeat_ms() {
        return;
    }
    XPORT_HB_MS.store(now, Ordering::Relaxed);
    debug_log(&format!(
        "transport[heartbeat] tid={:#010x} up={} wakeups={} jobs={} datagrams_rx={} datagrams_tx={} outbox_depth={} outbox_hwm={} poll_ms={TRANSPORT_POLL_MS}",
        XPORT_TID.load(Ordering::Relaxed),
        TRANSPORT_UP.load(Ordering::Relaxed),
        XPORT_WAKEUPS.load(Ordering::Relaxed),
        XPORT_JOBS.load(Ordering::Relaxed),
        XPORT_RX.load(Ordering::Relaxed),
        XPORT_TX.load(Ordering::Relaxed),
        XPORT_OUTBOX_DEPTH.load(Ordering::Relaxed),
        XPORT_OUTBOX_HWM.load(Ordering::Relaxed),
    ));
}

/// Transport-thread counters for the `msg_stats` line, including whether a live consumer exists.
fn transport_stats_note() -> String {
    format!(
        " transport[tid={:#010x} up={} wakeups={} jobs={} datagrams_rx={} datagrams_tx={}]",
        XPORT_TID.load(Ordering::Relaxed),
        TRANSPORT_UP.load(Ordering::Relaxed),
        XPORT_WAKEUPS.load(Ordering::Relaxed),
        XPORT_JOBS.load(Ordering::Relaxed),
        XPORT_RX.load(Ordering::Relaxed),
        XPORT_TX.load(Ordering::Relaxed),
    )
}

/// The dedicated transport thread. Not joined at shutdown; it only ever touches 'static state and
/// the leaked Network/Endpoint boxes, and stops being able to do anything once the handle is gone.
/// A panic is caught so the tick fallback (`TRANSPORT_UP=false`) can take over instead of leaving
/// every future send queued behind a dead consumer.
fn transport_thread_main() {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(transport_loop));
    TRANSPORT_UP.store(false, Ordering::SeqCst);
    if result.is_err() {
        log_line("transport thread PANICKED; falling back to tick-driven inline transport");
    }
}

fn transport_loop() {
    XPORT_TID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    // Idle wakeups must not be quantized to the ~15.6 ms system tick (see timeBeginPeriod above).
    let tbp = unsafe { timeBeginPeriod(1) };
    debug_log(&format!(
        "transport thread started tid={:#010x} poll_ms={TRANSPORT_POLL_MS} timer_res=1ms(tbp={tbp}) owns=udp/send/recv/rto/forced-ack",
        XPORT_TID.load(Ordering::Relaxed)
    ));
    let ob = outbox();
    loop {
        let now = now_ms();
        let mut wait_ms = TRANSPORT_POLL_MS;
        {
            // The same global handle mutex the tick uses: it is what protects the socket, the
            // remote set and every Sender/Receiver pairing.
            let mut g = g().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(h) = g.as_mut() {
                // Single consumer: the outbox is drained here and nowhere else, in bounded
                // batches so one producer burst cannot hold the handle mutex for the backlog.
                service_outbox();
                recv_udp(h);
                reliable_service(h, now);
                wait_ms = next_transport_wait_ms(h, now);
            }
        }
        XPORT_WAKEUPS.fetch_add(1, Ordering::Relaxed);
        transport_heartbeat(now);
        // Wake on enqueue immediately; `wait_ms` only bounds unsolicited inbound and idle timers.
        // The predicate is checked under the outbox mutex, so an enqueue cannot be lost.
        let q = ob.q.lock().unwrap_or_else(|e| e.into_inner());
        if q.is_empty() {
            let _ = ob
                .cv
                .wait_timeout(q, Duration::from_millis(wait_ms))
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Broker HTTP thread — fire-and-forget broker I/O
// ─────────────────────────────────────────────────────────────────────────────
//
// The tick calls PartyStartProcessingStateChanges every frame, and the shim used to run blocking
// broker HTTP inside that call while holding the global handle mutex (1s connect + 3s read
// timeouts). This thread owns that I/O instead. Two classes of request are moved here:
//   * register/leave: the response only feeds logs, nothing is applied to game state — no ordering
//     risk at all beyond FIFO request order, which the single consumer preserves.
//   * peer poll: the response mutates the remote set, so the HTTP and JSON parse run here but the
//     result is stashed and applied by the tick, guarded by a per-Network epoch (a result can only
//     be applied by the exact Network box that asked) and a request sequence (older response
//     dropped).
// The thread never takes the global handle mutex.

enum BrokerTask {
    /// POST /party/join. `quiet` skips the non-error log line (10s keep-alive).
    Register {
        network_id: String,
        entity: String,
        udp_port: u16,
        quiet: bool,
    },
    /// POST /party/leave. The response was always ignored.
    Leave { network_id: String, entity: String },
    /// GET /party/peers — parsed here, applied by the tick.
    PollPeers {
        network_id: String,
        epoch: u64,
        seq: u64,
    },
}

struct PeerEntry {
    entity: String,
    ip: String,
    udp_port: u16,
}

struct PeerPollResult {
    seq: u64,
    members: Vec<PeerEntry>,
}

struct BrokerQ {
    q: Mutex<VecDeque<BrokerTask>>,
    cv: Condvar,
}

static BROKER_Q: OnceLock<BrokerQ> = OnceLock::new();
static BROKER_STARTED: OnceLock<()> = OnceLock::new();
/// True once the broker thread is running; reported by `broker_stats_note`. When false, broker
/// requests run inline on the caller (spawn failure or a thread that died).
static BROKER_UP: AtomicBool = AtomicBool::new(false);
static BROKER_JOBS: AtomicU64 = AtomicU64::new(0);
/// Queued broker requests, for spotting an outage backlog in `broker_stats_note`.
static BROKER_Q_DEPTH: AtomicU64 = AtomicU64::new(0);
/// Number of entries in the peer-poll result map; mirrors the map length (updated under its lock).
static PEER_RESULTS_PENDING: AtomicU64 = AtomicU64::new(0);
static NEXT_POLL_SEQ: AtomicU64 = AtomicU64::new(1);
/// Unique per Network box (never per network id): the epoch a poll result carries home.
static NEXT_NET_EPOCH: AtomicU64 = AtomicU64::new(1);

fn broker_q() -> &'static BrokerQ {
    BROKER_Q.get_or_init(|| BrokerQ {
        q: Mutex::new(VecDeque::new()),
        cv: Condvar::new(),
    })
}

fn peer_poll_results() -> &'static Mutex<HashMap<u64, PeerPollResult>> {
    static R: OnceLock<Mutex<HashMap<u64, PeerPollResult>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ensure_broker_thread() {
    if force_inline_threads() {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            debug_log("broker disabled (GBFR_PARTY_FORCE_INLINE); broker I/O runs inline (blocking)");
        }
        return;
    }
    BROKER_STARTED.get_or_init(|| {
        match std::thread::Builder::new()
            .name("party-broker".into())
            .spawn(broker_thread_main)
        {
            Ok(_) => BROKER_UP.store(true, Ordering::SeqCst),
            Err(e) => log_line(&format!(
                "broker thread spawn FAILED: {e}; broker I/O will run inline (blocking)"
            )),
        }
        ()
    });
}

/// Hand one request to the broker thread. If no live consumer exists (spawn failure or a thread
/// that died), run the queued work and the new task inline — the pre-threading behaviour — so
/// registration/leave/peer discovery keep working at the cost of blocking the caller.
fn dispatch_broker(task: BrokerTask) {
    if !BROKER_UP.load(Ordering::Acquire) {
        // Drain whatever is already queued first, so inline mode preserves FIFO across the
        // handover.
        loop {
            let next = {
                let mut q = broker_q().q.lock().unwrap_or_else(|e| e.into_inner());
                q.pop_front()
            };
            match next {
                Some(t) => {
                    BROKER_JOBS.fetch_add(1, Ordering::Relaxed);
                    broker_run(t);
                }
                None => break,
            }
        }
        BROKER_Q_DEPTH.store(0, Ordering::Relaxed);
        BROKER_JOBS.fetch_add(1, Ordering::Relaxed);
        broker_run(task);
        return;
    }
    let bq = broker_q();
    {
        let mut q = bq.q.lock().unwrap_or_else(|e| e.into_inner());
        match task {
            // Keep-alive register: one task per (network, entity) is enough; the newest wins.
            BrokerTask::Register {
                network_id,
                entity,
                udp_port,
                quiet,
            } => {
                let mut found = false;
                for t in q.iter_mut() {
                    if let BrokerTask::Register {
                        network_id: nid,
                        entity: ent,
                        udp_port: p,
                        quiet: qi,
                    } = t
                    {
                        if *nid == network_id && *ent == entity {
                            *p = udp_port;
                            *qi = quiet;
                            found = true;
                            break;
                        }
                    }
                }
                if !found {
                    if q.len() >= BROKER_QUEUE_CAP {
                        if let Some(pos) =
                            q.iter().position(|t| matches!(t, BrokerTask::Register { .. }))
                        {
                            q.remove(pos);
                        }
                    }
                    q.push_back(BrokerTask::Register {
                        network_id,
                        entity,
                        udp_port,
                        quiet,
                    });
                }
            }
            other => q.push_back(other),
        }
        BROKER_Q_DEPTH.store(q.len() as u64, Ordering::Relaxed);
    }
    bq.cv.notify_one();
}

fn broker_thread_main() {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(broker_loop));
    BROKER_UP.store(false, Ordering::SeqCst);
    if result.is_err() {
        log_line("broker thread PANICKED; broker I/O will run inline (blocking)");
    }
}

fn broker_loop() {
    debug_log(&format!(
        "broker thread started tid={:#010x} owns=/party/join,/party/leave,/party/peers",
        unsafe { GetCurrentThreadId() }
    ));
    let bq = broker_q();
    loop {
        let task = {
            let mut q = bq.q.lock().unwrap_or_else(|e| e.into_inner());
            let t = loop {
                if let Some(t) = q.pop_front() {
                    break t;
                }
                q = bq.cv.wait(q).unwrap_or_else(|e| e.into_inner());
            };
            BROKER_Q_DEPTH.store(q.len() as u64, Ordering::Relaxed);
            t
        };
        BROKER_JOBS.fetch_add(1, Ordering::Relaxed);
        broker_run(task);
    }
}

fn broker_run(task: BrokerTask) {
    match task {
        BrokerTask::Register {
            network_id,
            entity,
            udp_port,
            quiet,
        } => {
            let ip = lan_ip();
            let ip_s = ip_string(ip);
            let body = format!(
                "{{\"network_id\":\"{network_id}\",\"entity_id\":\"{entity}\",\"udp_port\":{udp_port},\"ip\":\"{ip_s}\"}}"
            );
            if !quiet {
                debug_log(&format!(
                    "party register entity={entity} ip={ip_s} udp={udp_port}"
                ));
            }
            // A refused registration used to be discarded silently (`let _ =`), which left the
            // shim believing it was on the mesh while the broker had no row for it.
            match http_json_status("POST", "/party/join", &body) {
                Some((200, _)) => {}
                Some((status, resp)) => log_line(&format!(
                    "party register REJECTED entity={entity} net={network_id} ip={ip_s} udp={udp_port} status={status} body={}",
                    truncate_log(&resp, 300),
                )),
                None => log_line(&format!(
                    "party register FAILED (no broker response) entity={entity} net={network_id} ip={ip_s} udp={udp_port}"
                )),
            }
        }
        BrokerTask::Leave {
            network_id,
            entity,
        } => {
            let body = format!("{{\"network_id\":\"{network_id}\",\"entity_id\":\"{entity}\"}}");
            let _ = http_json("POST", "/party/leave", &body);
        }
        BrokerTask::PollPeers {
            network_id,
            epoch,
            seq,
        } => {
            let text = http_json(
                "GET",
                &format!("/party/peers?network_id={network_id}"),
                "",
            );
            let mut members = Vec::new();
            if let Some(text) = text {
                for obj in json_arr_objects(&text, "members") {
                    let Some(eid) = json_str(&obj, "entity_id") else {
                        continue;
                    };
                    let ip = json_str(&obj, "ip").unwrap_or_else(|| "127.0.0.1".into());
                    let udp_port = json_str(&obj, "udp_port")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    members.push(PeerEntry {
                        entity: eid,
                        ip,
                        udp_port,
                    });
                }
            }
            let map = peer_poll_results();
            let mut g = map.lock().unwrap_or_else(|e| e.into_inner());
            g.insert(epoch, PeerPollResult { seq, members });
            PEER_RESULTS_PENDING.store(g.len() as u64, Ordering::Relaxed);
        }
    }
}

/// Take the result for exactly this Network box, if one has arrived.
fn take_peer_result(epoch: u64) -> Option<PeerPollResult> {
    let map = peer_poll_results();
    let mut g = map.lock().unwrap_or_else(|e| e.into_inner());
    let r = g.remove(&epoch);
    if r.is_some() {
        PEER_RESULTS_PENDING.store(g.len() as u64, Ordering::Relaxed);
    }
    r
}

/// Drop results whose Network box is gone (left or cleaned up while the HTTP was in flight), so
/// the pending counter cannot stay non-zero forever and sweep work stays bounded.
fn drop_stale_peer_results(live: &[u64]) {
    let map = peer_poll_results();
    let mut g = map.lock().unwrap_or_else(|e| e.into_inner());
    let before = g.len();
    g.retain(|epoch, _| live.contains(epoch));
    if g.len() != before {
        PEER_RESULTS_PENDING.store(g.len() as u64, Ordering::Relaxed);
    }
}

fn broker_stats_note() -> String {
    format!(
        " broker[jobs={} up={} queue={}]",
        BROKER_JOBS.load(Ordering::Relaxed),
        BROKER_UP.load(Ordering::Relaxed),
        BROKER_Q_DEPTH.load(Ordering::Relaxed)
    )
}

fn serialize_desc(d: &Descriptor, out: *mut c_char) -> u32 {
    if out.is_null() {
        return ERR;
    }
    let id = d.id_str();
    let ip = desc_advert_ip(d).unwrap_or_else(lan_ip);
    let s = format!(
        "LAN1.{id}.{:02x}{:02x}{:02x}{:02x}",
        ip[0], ip[1], ip[2], ip[3]
    );
    let bytes = s.as_bytes();
    if bytes.len() >= SERIALIZED_MAX {
        return ERR;
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, bytes.len());
        *out.add(bytes.len()) = 0;
    }
    debug_throttled("serialize", &format!("Serialize {s}"));
    SUCCESS
}

fn deserialize_desc(s: *const c_char, out: *mut Descriptor) -> u32 {
    if s.is_null() || out.is_null() {
        return ERR;
    }
    // The real PartyDeserializeNetworkDescriptor zeroes all 0x165 bytes before parsing, so a
    // failed parse never leaves the caller holding a stale descriptor. This matters because the
    // exe ignores our return value and proceeds to read the descriptor it passed in: a stale
    // party+0xa0 could otherwise reach the Connect gate and reconnect to a dead network.
    unsafe { ptr::write_bytes(out as *mut u8, 0, DESC_SIZE) };
    let raw = read_cstr(s);
    // Relink posts lobby key network_descriptor="dummy" before PartySerialize.
    if !raw.starts_with("LAN1.") {
        debug_log(&format!(
            "Deserialize ignore (not LAN1) raw={raw} (out descriptor cleared)"
        ));
        return ERR;
    }
    let (id, ip) = parse_lan1(&raw);
    unsafe {
        *out = Descriptor::new(&id);
        if let Some(ip) = ip {
            desc_set_advert_ip(&mut *out, ip);
        }
    }
    SUCCESS
}

#[no_mangle]
pub extern "C" fn PartySetWorkMode(_thread: i32, _mode: i32) -> u32 {
    note_party_thread();
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyInitialize(title_id: *const c_char, handle: *mut *mut c_void) -> u32 {
    note_party_thread();
    debug_log(&format!(
        "PartyInitialize title_id={:p} handle_out={:p}",
        title_id, handle
    ));
    if handle.is_null() {
        return ERR;
    }
    let title = if title_id.is_null() {
        CString::new("lan").unwrap()
    } else {
        CString::new(read_cstr(title_id)).unwrap_or_else(|_| CString::new("lan").unwrap())
    };
    debug_log(&format!("PartyInitialize title={}", title.to_string_lossy()));
    IS_HOST.store(false, Ordering::SeqCst);
    GUEST_SUB5_SENT.store(false, Ordering::SeqCst);
    HOST_SNAPSHOT.store(false, Ordering::SeqCst);
    SUB7_SENT.store(false, Ordering::SeqCst);
    SUB7_RECV.store(false, Ordering::SeqCst);
    SUB7_APPLIED.store(false, Ordering::SeqCst);
    NATIVE_CONNECT.store(false, Ordering::SeqCst);
    MESH_TRIGGER_PENDING.store(false, Ordering::SeqCst);
    MESH_TRIGGER_DONE.store(false, Ordering::SeqCst);
    MESH_TRIGGER_TICKS.store(0, Ordering::SeqCst);
    MESH_TRIGGER_KEY.store(u64::MAX, Ordering::Relaxed);
    MESH_EXPORTS.store(0, Ordering::Relaxed);
    TYPE12_MS.store(0, Ordering::SeqCst);
    SNAPSHOT_MS.store(0, Ordering::SeqCst);
    TYPE12_HELD_LOG.store(false, Ordering::SeqCst);
    TYPE12_MEMBER_HELD_LOG.store(false, Ordering::SeqCst);
    LAST_PROBE_KEY.store(u64::MAX, Ordering::Relaxed);
    LAST_PROBE_PASS_MS.store(0, Ordering::Relaxed);
    LEDGER_HB_MS.store(0, Ordering::Relaxed);
    LEDGER_HB_SKIPPED.store(0, Ordering::Relaxed);
    let mut boxed = Box::new(Handle {
        title,
        users: Vec::new(),
        networks: Vec::new(),
        pending: VecDeque::new(),
        in_flight: Vec::new(),
        ledger: Ledger::default(),
        batch_outstanding: false,
        last_peer_poll: Instant::now() - Duration::from_secs(10),
    });
    *handle = boxed.as_mut() as *mut Handle as *mut c_void;
    *g().lock().unwrap() = Some(boxed);
    SUCCESS
}

fn with_handle<R>(handle: *mut c_void, f: impl FnOnce(&mut Handle) -> R, default: R) -> R {
    // Recover poison the same way the worker threads do: a panic in a shim call must not turn
    // every later export into a panic across the FFI boundary.
    let mut g = match g().lock() {
        Ok(g) => g,
        Err(e) => {
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                log_line("handle mutex poisoned; continuing with recovered state");
            }
            e.into_inner()
        }
    };
    match g.as_mut() {
        Some(h) => {
            let _ = handle;
            f(h)
        }
        None => default,
    }
}

#[no_mangle]
pub unsafe extern "C" fn PartyCreateLocalUser(
    handle: *mut c_void,
    entity_id: *const c_char,
    _token: *const c_char,
    user: *mut *mut c_void,
) -> u32 {
    note_party_thread();
    if user.is_null() {
        return ERR;
    }
    let ent = read_cstr(entity_id);
    debug_log(&format!("PartyCreateLocalUser entity={ent}"));
    let boxed = Box::new(LocalUser {
        entity: CString::new(ent).unwrap_or_else(|_| CString::new("user").unwrap()),
    });
    let p = Box::into_raw(boxed);
    with_handle(
        handle,
        |h| {
            h.users.push(p);
        },
        (),
    );
    *user = p as *mut c_void;
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyLocalUserGetEntityId(
    user: *mut c_void,
    out: *mut *const c_char,
) -> u32 {
    note_party_thread();
    if user.is_null() || out.is_null() {
        return ERR;
    }
    *out = cstr_ptr(&(*(user as *mut LocalUser)).entity);
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyCreateNewNetwork(
    handle: *mut c_void,
    local_user: *mut c_void,
    _config: *const c_void,
    _region_count: u32,
    _regions: *const c_void,
    _invite_cfg: *const c_void,
    _async: *mut c_void,
    out_desc: *mut Descriptor,
    _out_invite: *mut c_char,
) -> u32 {
    note_party_thread();
    debug_log("PartyCreateNewNetwork");
    note_export(EXP_CREATE_NETWORK);
    IS_HOST.store(true, Ordering::SeqCst);
    if !_config.is_null() {
        // PartyNetworkConfiguration: maxUserCount, maxDeviceCount, ...
        let max_users = ptr::read_unaligned(_config as *const u32);
        let max_devices = ptr::read_unaligned((_config as *const u8).add(4) as *const u32);
        let per_dev = ptr::read_unaligned((_config as *const u8).add(8) as *const u32);
        let per_user = ptr::read_unaligned((_config as *const u8).add(12) as *const u32);
        debug_log(&format!(
            "PartyCreateNewNetwork config maxUserCount={max_users} maxDeviceCount={max_devices} maxUsersPerDevice={per_dev} maxDevicesPerUser={per_user}"
        ));
        if max_users == 4 || max_devices == 4 {
            debug_log("WATCH_4 exe still creating Party mesh with maxUserCount/maxDeviceCount=4");
        }
    } else {
        debug_log("PartyCreateNewNetwork config=null (shim does not enforce maxUserCount)");
    }
    let ident = uuid_ident();
    let invite = CString::new("lan-invite").unwrap();
    let (udp, port) = match bind_udp() {
        Some(v) => v,
        None => {
            log_line("PartyCreateNewNetwork UDP bind failed");
            return ERR;
        }
    };
    let mut desc = Descriptor::new(&ident);
    desc.opaque[0..2].copy_from_slice(&port.to_le_bytes());
    let advert = lan_ip();
    desc_set_advert_ip(&mut desc, advert);
    if !out_desc.is_null() {
        *out_desc = desc;
    }
    let net = Box::new(Network {
        descriptor: desc,
        invitation: invite,
        local_user: local_user as *mut LocalUser,
        local_endpoint: ptr::null_mut(),
        remotes: HashMap::new(),
        udp: Some(udp),
        udp_port: port,
        connect_completed: false,
        last_register: Instant::now(),
        pending_rx: VecDeque::new(),
        pending_peers: VecDeque::new(),
        type10_delivered: false,
        type12_delivered: false,
        type12_hold_start: None,
        type12_hold_timeout_logged: false,
        poll_requested: false,
        poll_epoch: NEXT_NET_EPOCH.fetch_add(1, Ordering::Relaxed),
        poll_inflight: false,
        poll_sent_at: Instant::now(),
        poll_applied_seq: 0,
    });
    let netp = Box::into_raw(net);
    let ent = if (*netp).local_user.is_null() {
        String::new()
    } else {
        (*((*netp).local_user)).entity.to_string_lossy().into_owned()
    };
    if !ent.is_empty() {
        register_member(unsafe { &*netp }, &ent);
    }
    debug_log(&format!(
        "PartyCreateNewNetwork id={ident} udp={port} advert={}",
        ip_string(advert)
    ));
    with_handle(
        handle,
        |h| {
            h.networks.push(netp);
            // type 2 CreateNewNetworkCompleted, invitation pointer at +0x1B0
            let sc = alloc_sc(0x1C0, 2);
            if !sc.is_null() {
                ptr::write_unaligned(sc.add(4) as *mut u32, 0); // result Succeeded
                ptr::write_unaligned(sc.add(0x10) as *mut *mut c_void, local_user);
                ptr::copy_nonoverlapping(
                    (&(*netp).descriptor as *const Descriptor) as *const u8,
                    sc.add(0x48),
                    DESC_SIZE,
                );
                ptr::write_unaligned(
                    sc.add(0x1B0) as *mut *const c_char,
                    (*netp).invitation.as_ptr(),
                );
                queue_sc(h, sc);
            }
        },
        (),
    );
    SUCCESS
}

unsafe fn emit_connect_completed(h: &mut Handle, netp: *mut Network) {
    if netp.is_null() || (*netp).connect_completed {
        return;
    }
    (*netp).connect_completed = true;
    let sc = alloc_sc(0x190, 3);
    if !sc.is_null() {
        ptr::write_unaligned(sc.add(4) as *mut u32, 0);
        ptr::copy_nonoverlapping(
            (&(*netp).descriptor as *const Descriptor) as *const u8,
            sc.add(0xC),
            DESC_SIZE,
        );
        ptr::write_unaligned(sc.add(0x180) as *mut *mut Network, netp);
        queue_sc(h, sc);
    }
    debug_log(&format!(
        "PartyConnectToNetwork id={} udp={} (state_change type=3 follows)",
        (*netp).descriptor.id_str(),
        (*netp).udp_port
    ));
}

/// # Safety
/// `h` and `desc` come from the exported Party entry points while the global handle mutex is
/// held. `desc` was deserialized by the caller; the returned Network is Box-owned by the shim
/// and stays valid until the network is destroyed.
unsafe fn connect_network(h: &mut Handle, desc: Descriptor) -> *mut Network {
    let ident = desc.id_str();
    if let Some(n) = h
        .networks
        .iter()
        .copied()
        .find(|n| (**n).descriptor.id_str() == ident)
    {
        // Host CreateNewNetwork already inserted this mesh. The exe still calls
        // PartyConnectToNetwork from CreateNewNetworkCompleted and waits for type 3.
        // Same-id Connect after auto-connect is a no-op (connect_completed).
        emit_connect_completed(h, n);
        poll_peers_request(n);
        return n;
    }
    let Some((udp, port)) = bind_udp() else {
        log_line(&format!("PartyConnect UDP bind failed id={ident}"));
        return ptr::null_mut();
    };
    let local_user = h.users.last().copied().unwrap_or(ptr::null_mut());
    let net = Box::new(Network {
        descriptor: desc,
        invitation: CString::new("lan-invite").unwrap(),
        local_user,
        local_endpoint: ptr::null_mut(),
        remotes: HashMap::new(),
        udp: Some(udp),
        udp_port: port,
        connect_completed: false,
        last_register: Instant::now(),
        pending_rx: VecDeque::new(),
        pending_peers: VecDeque::new(),
        type10_delivered: false,
        type12_delivered: false,
        type12_hold_start: None,
        type12_hold_timeout_logged: false,
        poll_requested: false,
        poll_epoch: NEXT_NET_EPOCH.fetch_add(1, Ordering::Relaxed),
        poll_inflight: false,
        poll_sent_at: Instant::now(),
        poll_applied_seq: 0,
    });
    let netp = Box::into_raw(net);
    if !local_user.is_null() {
        register_member(&*netp, &(*local_user).entity.to_string_lossy());
    }
    h.networks.push(netp);
    emit_connect_completed(h, netp);
    poll_peers_request(netp);
    send_transport_control(netp, KIND_HELLO);
    netp
}

#[no_mangle]
pub unsafe extern "C" fn PartyConnectToNetwork(
    handle: *mut c_void,
    descriptor: *const Descriptor,
    _async: *mut c_void,
    out_network: *mut *mut c_void,
) -> u32 {
    note_party_thread();
    if descriptor.is_null() {
        return ERR;
    }
    let desc = *descriptor;
    log_throttled(
        "connect_requested",
        &format!("PartyConnectToNetwork requested id={}", desc.id_str()),
    );
    note_export(EXP_CONNECT);
    NATIVE_CONNECT.store(true, Ordering::SeqCst);
    let netp = with_handle(handle, |h| connect_network(h, desc), ptr::null_mut());
    if netp.is_null() {
        return ERR;
    }
    if !out_network.is_null() {
        *out_network = netp as *mut c_void;
    }
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyDeserializeNetworkDescriptor(
    serialized: *const c_char,
    descriptor: *mut Descriptor,
) -> u32 {
    note_party_thread();
    let raw = read_cstr(serialized);
    let r = deserialize_desc(serialized, descriptor);
    if r == SUCCESS {
        debug_log(&format!(
            "Deserialize raw={raw} id={} (waiting for exe FUN_1425137c0 PartyConnectToNetwork)",
            if descriptor.is_null() {
                "?".into()
            } else {
                (*descriptor).id_str()
            }
        ));
        log_connect_gate_inputs();
        // Descriptor known: arm the exe's own mesh-start leaf. PTY-8: do not fire it here — the
        // exe copies the invitation into party+0x208/+0x218 only after this export returns, so a
        // connectable row armed now could start the mesh with an empty invitation and the gate
        // would take the Create branch (split-brain). StartProcessing fires it once
        // party+0x218 != 0. No +0x70 write, no .text patch.
        note_export(EXP_DESERIALIZE);
        MESH_TRIGGER_PENDING.store(true, Ordering::SeqCst);
        MESH_TRIGGER_DONE.store(false, Ordering::SeqCst);
        MESH_TRIGGER_TICKS.store(48, Ordering::SeqCst);
        MESH_TRIGGER_KEY.store(u64::MAX, Ordering::Relaxed);
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn PartySerializeNetworkDescriptor(
    descriptor: *const Descriptor,
    out: *mut c_char,
) -> u32 {
    note_party_thread();
    if descriptor.is_null() {
        return ERR;
    }
    serialize_desc(&*descriptor, out)
}

#[no_mangle]
pub unsafe extern "C" fn PartyNetworkAuthenticateLocalUser(
    network: *mut c_void,
    local_user: *mut c_void,
    _invitation: *const c_char,
    _async: *mut c_void,
) -> u32 {
    note_party_thread();
    log_throttled(
        "authenticate_api",
        "PartyNetworkAuthenticateLocalUser (state_change type=4)",
    );
    note_export(EXP_AUTHENTICATE);
    let ent = if local_user.is_null() {
        String::new()
    } else {
        (*(local_user as *mut LocalUser)).entity.to_string_lossy().into_owned()
    };
    if !ent.is_empty() {
        debug_log(&format!("WATCH_4 AuthenticateLocalUser entity={ent}"));
    }
    with_handle(
        ptr::null_mut(),
        |h| {
            if !h.networks.iter().any(|n| *n as *mut c_void == network) {
                h.networks.push(network as *mut Network);
            }
            // PartyAuthenticateLocalUserCompletedStateChange is 0x30: result@4, errorDetail@8,
            // network@0x10, localUser@0x18, invitationIdentifier@0x20, asyncIdentifier@0x28.
            let sc = alloc_sc(0x30, 4);
            if !sc.is_null() {
                ptr::write_unaligned(sc.add(4) as *mut u32, 0);
                ptr::write_unaligned(sc.add(0x10) as *mut *mut c_void, network);
                ptr::write_unaligned(sc.add(0x18) as *mut *mut c_void, local_user);
                queue_sc(h, sc);
            }
        },
        (),
    );
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyNetworkCreateEndpoint(
    network: *mut c_void,
    local_user: *mut c_void,
    _prop_count: u32,
    _keys: *const *const c_char,
    _values: *const c_void,
    _async: *mut c_void,
    out_endpoint: *mut *mut c_void,
) -> u32 {
    note_party_thread();
    let net = network as *mut Network;
    let uid = NEXT_UID.fetch_add(1, Ordering::Relaxed);
    let ent = if local_user.is_null() {
        CString::new("ep").unwrap()
    } else {
        (* (local_user as *mut LocalUser)).entity.clone()
    };
    debug_log(&format!(
        "CreateEndpoint uid={uid} entity={}",
        ent.to_string_lossy()
    ));
    note_export(EXP_CREATE_ENDPOINT);
    let boxed = Box::new(Endpoint {
        entity: ent,
        uid,
        network: net,
        ip: String::new(),
        udp_port: 0,
        last_seq: 0,
        seq_seen: false,
        tx: reliable::Sender::new(),
        rx: reliable::Receiver::new(),
    });
    let ep = Box::into_raw(boxed);
    if !out_endpoint.is_null() {
        *out_endpoint = ep as *mut c_void;
    }
    with_handle(
        ptr::null_mut(),
        |h| {
            // Set under the handle lock: the transport thread reads these through the same lock.
            if !net.is_null() {
                (*net).local_endpoint = ep;
                (*net).local_user = local_user as *mut LocalUser;
            }
            let sc = alloc_sc(0x30, 10);
            if !sc.is_null() {
                ptr::write_unaligned(sc.add(4) as *mut u32, 0);
                ptr::write_unaligned(sc.add(0x10) as *mut *mut c_void, network);
                ptr::write_unaligned(sc.add(0x18) as *mut *mut c_void, local_user);
                ptr::write_unaligned(sc.add(0x28) as *mut *mut Endpoint, ep);
                queue_sc(h, sc);
            }
            // local_endpoint is now set; type 12 waits until FinishProcessing
            // of this type-10 SC so the exe is not handed 10+12 in one pump.
            if !net.is_null() {
                flush_pending_rx(h, net);
            }
        },
        (),
    );
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyEndpointGetEntityId(
    endpoint: *mut c_void,
    out: *mut *const c_char,
) -> u32 {
    note_party_thread();
    if endpoint.is_null() || out.is_null() {
        return ERR;
    }
    *out = cstr_ptr(&(*(endpoint as *mut Endpoint)).entity);
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyEndpointGetUniqueIdentifier(
    endpoint: *mut c_void,
    out: *mut u16,
) -> u32 {
    note_party_thread();
    if endpoint.is_null() || out.is_null() {
        return ERR;
    }
    *out = (*(endpoint as *mut Endpoint)).uid;
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyEndpointSendMessage(
    local_endpoint: *mut c_void,
    target_count: u32,
    _targets: *const *mut c_void,
    options: u32,
    _queue: *const c_void,
    buffer_count: u32,
    buffers: *const u8,
    _async: *mut c_void,
) -> u32 {
    note_party_thread();
    if local_endpoint.is_null() || buffers.is_null() || buffer_count == 0 || buffer_count > 8 {
        log_line(&format!(
            "PartyEndpointSendMessage refuse local_ep_null={} buffers_null={} buffer_count={buffer_count}",
            local_endpoint.is_null(),
            buffers.is_null()
        ));
        return ERR;
    }
    if target_count > 8 {
        log_line(&format!("PartyEndpointSendMessage refuse target_count={target_count}"));
        return ERR;
    }
    // PartyDataBuffer { void* buffer; u32 size; } possibly 16-byte aligned
    let data_ptr = ptr::read_unaligned(buffers as *const *const u8);
    let data_len = ptr::read_unaligned(buffers.add(std::mem::size_of::<*const u8>()) as *const u32) as usize;
    if data_ptr.is_null() || data_len == 0 || data_len > 64 * 1024 {
        log_line(&format!("PartyEndpointSendMessage refuse len={data_len}"));
        return ERR;
    }
    let payload = std::slice::from_raw_parts(data_ptr, data_len);
    let ep = local_endpoint as *mut Endpoint;
    let net = (*ep).network;
    if net.is_null() {
        log_line("PartyEndpointSendMessage refuse network=null");
        return ERR;
    }
    // Option B: enqueue and return; the transport thread performs the send and the diagnostics.
    send_transport_msg(net, ep, target_count, payload, options);
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyNetworkGetNetworkDescriptor(
    network: *mut c_void,
    out: *mut Descriptor,
) -> u32 {
    note_party_thread();
    if network.is_null() || out.is_null() {
        return ERR;
    }
    *out = (*(network as *mut Network)).descriptor;
    SUCCESS
}

/// DAT_147c52cd0 — net-session object pointer. FUN_140261150 reads `+4`.
const SESSION_GLOBAL_RVA: usize = 0x7c52cd0;
/// FUN_14025fd50 — exe mesh-start leaf. Its continuation vtable phase
/// FUN_140253b50 creates the matching work with `work+0x70 = 1` (0x1402554E0).
/// The leaf's own guards are `*(u32*)session != 3` and `[session+0x28] == 0`;
/// it allocates from the calling thread's TLS async pool (gs:[0x58]).
/// DAT_147c52ce0 — member-list container. FUN_140261150 walks `+0x50..+0x58`.
const MEMBER_LIST_GLOBAL_RVA: usize = 0x7c52ce0;
/// DAT_147c52cd8 — send-object holder; gameplay send requires `+0x38 != 0`.
const SEND_GLOBAL_RVA: usize = 0x7c52cd8;
/// DAT_1471afb30 — opcode 3 sub 5 "ready" list (stride 0x180, keyed by work+8).
const READY_LIST_GLOBAL_RVA: usize = 0x71afb30;
/// DAT_147c48628 — quest manager. FUN_140adc810 writes `+0x6c814=5` at matching
/// start; FUN_1428F9CA0 writes `+0x6c814=0` / `+0x6c818=1` only when
/// FUN_140adc3c0 returns 2 or 3. That is the load-screen latch.
const QUEST_MGR_GLOBAL_RVA: usize = 0x7c48628;
const READY_STRIDE: usize = 0x180;

/// Read-only: FUN_1425137c0 requires DAT_147c52cd8+0x18 chain and a nonempty
/// descriptor at party+0xa0 (Deserialize out-pointer).
unsafe fn log_connect_gate_inputs() {
    let base = GetModuleHandleA(ptr::null()) as usize;
    if base == 0 || !readable(base + SEND_GLOBAL_RVA, 8) {
        log_line("connect-gate inputs: send global unreadable");
        return;
    }
    let send = ptr::read_unaligned((base + SEND_GLOBAL_RVA) as *const usize);
    if send == 0 {
        log_line("connect-gate inputs: DAT_147c52cd8=null");
        return;
    }
    let p18 = if readable(send + 0x18, 8) {
        ptr::read_unaligned((send + 0x18) as *const usize)
    } else {
        0
    };
    let inner = if p18 != 0 && readable(p18 + 0x38, 8) {
        ptr::read_unaligned((p18 + 0x38) as *const usize)
    } else {
        0
    };
    let party = if readable(send + 0x38, 8) {
        ptr::read_unaligned((send + 0x38) as *const usize)
    } else {
        0
    };
    let mut b0 = 0u8;
    let mut inv = 0u64;
    if party != 0 {
        if readable(party + 0xa0, 1) {
            b0 = ptr::read((party + 0xa0) as *const u8);
        }
        if readable(party + 0x218, 8) {
            inv = ptr::read_unaligned((party + 0x218) as *const u64);
        }
    }
    debug_log(&format!(
        "connect-gate inputs send={send:#x} +18={p18:#x} [+18]+38={inner:#x} party+38={party:#x} desc0={b0} inv={inv}"
    ));
}

/// PTY-8: the exe copies the invitation into `party+0x208/+0x218` only after
/// `PartyDeserializeNetworkDescriptor` returns (exe `0x143B465EF..465FF`), so arming the
/// mesh-start leaf from Deserialize could start a mesh with an empty invitation and the gate
/// would take the Create branch. Returns `(party, *party+0x218)` through the same chain
/// `log_connect_gate_inputs` reports; read-only.
unsafe fn connect_gate_party_invitation() -> (usize, u64) {
    let base = GetModuleHandleA(ptr::null()) as usize;
    if base == 0 || !readable(base + SEND_GLOBAL_RVA, 8) {
        return (0, 0);
    }
    let send = ptr::read_unaligned((base + SEND_GLOBAL_RVA) as *const usize);
    if send == 0 || !readable(send + 0x38, 8) {
        return (0, 0);
    }
    let party = ptr::read_unaligned((send + 0x38) as *const usize);
    if party == 0 || !readable(party + 0x218, 8) {
        return (party, 0);
    }
    (party, ptr::read_unaligned((party + 0x218) as *const u64))
}

/// # Safety
/// `obj` is a game-owned std::string-like object (layout at +0x10 len, +0x18 cap). Capacity,
/// length, storage pointer and the bytes themselves are each validated with `readable` before
/// use; any failed check yields an empty string instead of a fault.
unsafe fn read_std_string(obj: usize) -> String {
    if !readable(obj + 0x18, 8) || !readable(obj + 0x10, 8) {
        return String::new();
    }
    let cap = ptr::read_unaligned((obj + 0x18) as *const u64);
    let len = ptr::read_unaligned((obj + 0x10) as *const u64);
    if len == 0 || len > 64 {
        return String::new();
    }
    let p = if cap > 15 {
        if !readable(obj, 8) {
            return String::new();
        }
        ptr::read_unaligned(obj as *const usize)
    } else {
        obj
    };
    if p == 0 || !readable(p, len as usize) {
        return String::new();
    }
    let sl = std::slice::from_raw_parts(p as *const u8, len as usize);
    String::from_utf8_lossy(sl).into_owned()
}

/// # Safety
/// Walks the exe's member-list global (`MEMBER_LIST_GLOBAL_RVA`). Every step is guarded by
/// `readable` and a shape check (end >= begin, stride 0x20, bounded entry count); an
/// unrecognised layout returns `None` instead of reading out of bounds. The RVA is
/// version-specific and must be re-derived after a game update.
unsafe fn member_list_container() -> Option<usize> {
    let base = GetModuleHandleA(ptr::null()) as usize;
    if base == 0 || !readable(base + MEMBER_LIST_GLOBAL_RVA, 8) {
        return None;
    }
    let members = ptr::read_unaligned((base + MEMBER_LIST_GLOBAL_RVA) as *const usize);
    if members == 0 || !readable(members + 0x50, 24) {
        return None;
    }
    Some(members)
}

unsafe fn member_list_has_entity(eid: &str) -> bool {
    let Some(members) = member_list_container() else {
        return false;
    };
    let begin = ptr::read_unaligned((members + 0x50) as *const usize);
    let end = ptr::read_unaligned((members + 0x58) as *const usize);
    if begin == 0 || end < begin || (end - begin) % 0x20 != 0 || (end - begin) > 0x20 * 16 {
        return false;
    }
    if !readable(begin, end - begin) {
        return false;
    }
    let mut e = begin;
    while e < end {
        let inner = if readable(e + 0x18, 8) {
            ptr::read_unaligned((e + 0x18) as *const usize)
        } else {
            0
        };
        let flagobj = if inner != 0 && readable(inner + 0x18, 8) {
            ptr::read_unaligned((inner + 0x18) as *const usize)
        } else {
            0
        };
        if flagobj != 0 && read_std_string(flagobj + 0x38) == eid {
            return true;
        }
        e += 0x20;
    }
    false
}

struct MemberSlot {
    id: i32,
    flag70: u8,
    unique: i32,
    a0: i32,
    rank: i32,
    eid: String,
}

unsafe fn collect_member_slots() -> Vec<MemberSlot> {
    let mut out = Vec::new();
    let Some(members) = member_list_container() else {
        return out;
    };
    let begin = ptr::read_unaligned((members + 0x50) as *const usize);
    let end = ptr::read_unaligned((members + 0x58) as *const usize);
    if begin == 0 || end < begin || (end - begin) % 0x20 != 0 || (end - begin) > 0x20 * 16 {
        return out;
    }
    if !readable(begin, end - begin) {
        return out;
    }
    let mut e = begin;
    while e < end {
        let inner = if readable(e + 0x18, 8) {
            ptr::read_unaligned((e + 0x18) as *const usize)
        } else {
            0
        };
        let work = if inner != 0 && readable(inner + 0x18, 8) {
            ptr::read_unaligned((inner + 0x18) as *const usize)
        } else {
            0
        };
        let flag70 = if work != 0 && readable(work + 0x70, 1) {
            ptr::read_unaligned((work + 0x70) as *const u8)
        } else {
            0xff
        };
        let id = if work != 0 && readable(work + 0x68, 4) {
            ptr::read_unaligned((work + 0x68) as *const i32)
        } else {
            -1
        };
        let unique = if work != 0 && readable(work + 8, 4) {
            ptr::read_unaligned((work + 8) as *const i32)
        } else {
            -1
        };
        let a0 = if work != 0 && readable(work + 0xa0, 4) {
            ptr::read_unaligned((work + 0xa0) as *const i32)
        } else {
            -1
        };
        let rank = if work != 0 && readable(work + 0x6c, 4) {
            ptr::read_unaligned((work + 0x6c) as *const i32)
        } else {
            -1
        };
        let eid = if work != 0 {
            read_std_string(work + 0x38)
        } else {
            String::new()
        };
        out.push(MemberSlot {
            id,
            flag70,
            unique,
            a0,
            rank,
            eid,
        });
        e += 0x20;
    }
    out
}

fn ready_list_note() -> String {
    unsafe {
        let base = GetModuleHandleA(ptr::null()) as usize;
        if base == 0 || !readable(base + READY_LIST_GLOBAL_RVA, 8) {
            return " ready=unreadable".into();
        }
        let obj = ptr::read_unaligned((base + READY_LIST_GLOBAL_RVA) as *const usize);
        if obj == 0 {
            return " ready=null".into();
        }
        if !readable(obj + 0x50, 24) {
            return format!(" ready={obj:#x} +50 unreadable");
        }
        let flag = ptr::read_unaligned((obj + 0x50) as *const u8);
        let begin = ptr::read_unaligned((obj + 0x58) as *const usize);
        let end = ptr::read_unaligned((obj + 0x60) as *const usize);
        if begin == 0 {
            return format!(" ready={obj:#x} fl={flag} n=0");
        }
        if end < begin || (end - begin) % READY_STRIDE != 0 || (end - begin) > READY_STRIDE * 16 {
            return format!(" ready={obj:#x} fl={flag} range_bad {begin:#x}..{end:#x}");
        }
        if !readable(begin, end - begin) {
            return format!(" ready={obj:#x} fl={flag} vec unreadable");
        }
        let n = (end - begin) / READY_STRIDE;
        let mut ids = Vec::new();
        let mut e = begin;
        while e < end {
            let id = ptr::read_unaligned(e as *const i32);
            let st = ptr::read_unaligned((e + 4) as *const i32);
            let fl10 = ptr::read_unaligned((e + 0x10) as *const u8);
            let skip = ptr::read_unaligned((e + 0x40) as *const u8);
            ids.push(format!("{id}:{st}:t{fl10}:s{skip}"));
            e += READY_STRIDE;
        }
        format!(" ready={obj:#x} fl={flag} n={n} [{}]", ids.join("; "))
    }
}

/// Probe the send-gate inputs (read-only).
/// Quest-start guard probe (read-only). `DecisionStartQuestOnMulti` (0x141CB7BA0) reaches
/// `syncStart` only when its guard passes; if it aborts it sends nothing and both peers sit on
/// "connecting" forever with no wire traffic — which is exactly what we observe. Log the guard
/// inputs so we can see which one is blocking. We never write them.
unsafe fn quest_guard_note() {
    // Global at VA 0x147C48628 (RVA 0x7C48628). The guard reads `[questmgr+off]`; it is ambiguous
    // statically whether the object is the global itself or the pointer it holds, so log both.
    const QG_RVA: usize = 0x7C48628;
    let base = GetModuleHandleA(ptr::null()) as usize;
    if base == 0 || !readable(base + QG_RVA, 8) {
        return;
    }
    let g = ptr::read_unaligned((base + QG_RVA) as *const usize);
    let mut s = format!("quest_guard g={g:#x}");
    for (off, name) in [
        (0x1c530usize, "1c530"),
        (0x6c814, "6c814"),
        (0x6c818, "6c818"),
        (0x6c838, "6c838"),
        // 6ccf4: bumped by DecisionStartQuestOnMulti::prepare (capped 0x63). 0 => that action was
        //        NEVER prepared; >0 => it was reached. Read-only, no hook needed (CW-9).
        (0x6ccf4, "6ccf4"),
        // 6ccfc / 1c108: ResolveMatchingQuest leaves only when
        //   clamp(overlay.vtbl+0x128(0) - [questmgr+0x1c108], 1..4) == [questmgr+0x6ccfc]
        // and otherwise fires ToError. A 6ccfc outside 1..4 can never match (CW-4).
        (0x6ccfc, "6ccfc"),
        (0x1c108, "1c108"),
        // N: IsRandomMatching is a pure quest-manager predicate: TRUE iff count != 4 and
        // [questmgr+0x1beb8] == 0, and 1beb8 == 0 is what routes the flow into the
        // GameSessionMatching sub-FSM (non-zero -> ResolveMatchingQuest).
        (0x1beb8, "1beb8"),
        // N: 6c7fc is the matching-result field. GameSessionMatching's actions force it to 0
        // ("matching pending") and issue no network I/O at all — so if it reads 0 we are parked
        // inside that sub-FSM waiting for a local completion. 6cd08 is its matching object.
        (0x6c7fc, "6c7fc"),
        (0x6cd08, "6cd08"),
    ] {
        let d = if readable(base + QG_RVA + off, 4) {
            ptr::read_unaligned((base + QG_RVA + off) as *const u32)
        } else {
            u32::MAX
        };
        let v = if g != 0 && readable(g.wrapping_add(off), 4) {
            ptr::read_unaligned(g.wrapping_add(off) as *const u32)
        } else {
            u32::MAX
        };
        s.push_str(&format!(" {name}=d{d}/v{v}"));
    }
    if readable(base + SESSION_GLOBAL_RVA, 8) {
        let sess = ptr::read_unaligned((base + SESSION_GLOBAL_RVA) as *const usize);
        if sess != 0 && readable(sess + 4, 8) {
            let s4 = ptr::read_unaligned((sess + 4) as *const u32);
            let s4ac = ptr::read_unaligned((sess + 0x4ac) as *const u32);
            s.push_str(&format!(" session+4={s4} +4ac={s4ac}"));
        }
    }
    debug_throttled("quest_guard", &s);
}

/// Quest-start sync-machine probe (read-only). `syncStart` (0x140B14A60) builds a 9-state machine;
/// its controller is at VA 0x147BCD6E8 and its container at VA 0x147BCD6C8. In the last run the
/// machine emitted ten 72-byte `op3` messages (state 1) but never the 496/784-byte ones, so it
/// either stalled after state 1 or was never ticked (Q4: the tick is runtime-registered). Log the
/// state so we can tell which. We never write it.
unsafe fn quest_sync_note() {
    const CONT_RVA: usize = 0x7BCD6C8; // VA 0x147BCD6C8
    const CTRL_RVA: usize = 0x7BCD6E8; // VA 0x147BCD6E8
    let base = GetModuleHandleA(ptr::null()) as usize;
    if base == 0 {
        return;
    }
    let mut s = String::from("quest_sync");
    for (rva, name) in [(CONT_RVA, "cont"), (CTRL_RVA, "ctrl")] {
        if !readable(base + rva, 8) {
            s.push_str(&format!(" {name}=unreadable"));
            continue;
        }
        let p = ptr::read_unaligned((base + rva) as *const usize);
        s.push_str(&format!(" {name}={p:#x}"));
        // The global may hold a pointer to the object or BE the object; log both readings.
        if readable(base + rva, 16) {
            let a = ptr::read_unaligned((base + rva) as *const u32);
            let b = ptr::read_unaligned((base + rva + 4) as *const u32);
            s.push_str(&format!(" ({name}d={a}/{b})"));
        }
        if p != 0 && readable(p, 16) {
            let a = ptr::read_unaligned(p as *const u32);
            let b = ptr::read_unaligned(p.wrapping_add(4) as *const u32);
            let c = ptr::read_unaligned(p.wrapping_add(8) as *const u32);
            s.push_str(&format!(" ({name}v={a}/{b}/{c})"));
        }
    }
    debug_throttled("quest_sync", &s);
}

/// Quest-start guard: the game's notification/error sink (read-only). This is NOT a per-member
/// array — ORDINAL_CHAIN_AUDIT OC-1 established that `[0x147C47850]` holds four bucket vectors and
/// `DecisionStartQuestOnMulti` tests only the LAST 20-byte record of the highest-priority non-empty
/// bucket (count@+0x80/+0x60/+0x40/+0x20 checked in that order, base@+0x70/+0x50/+0x30/+0x10):
///   cmp dword [rec + 0xc], 1  -> bail   (newest notification is a blocking kind)
///   cmp byte  [rec + 0x12], 0 != bail   (caller-supplied flag)
/// So it bails on "the newest pending notification is an error/should-block kind", which is a
/// transient notification state, not a member table. The previous probe read `p + i*20` (bucket
/// headers) and printed noise. We never write this object.
unsafe fn quest_array_note() {
    const ARR_RVA: usize = 0x7C47850; // VA 0x147C47850
    let base = GetModuleHandleA(ptr::null()) as usize;
    if base == 0 || !readable(base + ARR_RVA, 8) {
        return;
    }
    let p = ptr::read_unaligned((base + ARR_RVA) as *const usize);
    if p == 0 {
        debug_throttled("quest_array", "quest_array p=0");
        return;
    }
    // Guard priority order: bucket 4 (base +0x70 / count +0x80) first, then 3, 2, 1.
    let buckets: [(usize, usize); 4] = [(0x70, 0x80), (0x50, 0x60), (0x30, 0x40), (0x10, 0x20)];
    let mut s = format!("quest_array p={p:#x}");
    let mut picked = false;
    for (i, (boff, coff)) in buckets.iter().enumerate() {
        if !readable(p + coff, 4) {
            break;
        }
        let count = ptr::read_unaligned((p + coff) as *const u32) as usize;
        if count == 0 {
            continue;
        }
        let bptr = ptr::read_unaligned((p + boff) as *const usize);
        // Guard reads only the last record of the first non-empty bucket.
        let tag = if picked { "" } else { "(tested)" };
        picked = true;
        if bptr == 0 || !readable(bptr, count * 20) {
            s.push_str(&format!(" b{}={count}{tag} bad_base", 4 - i));
            continue;
        }
        let rec = bptr.wrapping_add((count - 1) * 20);
        let kind = ptr::read_unaligned(rec.wrapping_add(0xc) as *const u32);
        let flag = ptr::read_unaligned(rec.wrapping_add(0x12) as *const u8);
        let code = ptr::read_unaligned(rec.wrapping_add(0) as *const u32);
        s.push_str(&format!(
            " b{}={count}{tag} last[c{code:#x} kind{kind} 12:{flag}]",
            4 - i
        ));
    }
    if !picked {
        s.push_str(" all_buckets_empty");
    }
    debug_throttled("quest_array", &s);
}

/// Quest-action result probe (read-only). `DecisionStartQuestOnMulti` (0x141CB7BA0) writes a status
/// code to `[[0x147C233C0]+0x108]` before returning: 0xce586d9 on the bail path (where it marks the
/// action done at [rsi+0x30]=1, returns success, and sends nothing — hence "connecting" forever),
/// and 0xd40bfc86 when the action is not applicable. Seeing which code is present when a quest is
/// attempted distinguishes "the action ran and bailed" from "the action never ran at all", which
/// have different fixes. We never write it.
unsafe fn quest_action_note() {
    const OBJ_RVA: usize = 0x7C233C0; // VA 0x147C233C0
    let base = GetModuleHandleA(ptr::null()) as usize;
    if base == 0 || !readable(base + OBJ_RVA, 8) {
        return;
    }
    let p = ptr::read_unaligned((base + OBJ_RVA) as *const usize);
    if p == 0 {
        debug_throttled("quest_action", "quest_action p=0");
        return;
    }
    if !readable(p.wrapping_add(0x108), 8) {
        return;
    }
    let code = ptr::read_unaligned(p.wrapping_add(0x108) as *const u32);
    let done = ptr::read_unaligned(p.wrapping_add(0x1d4) as *const u8);
    log_throttled(
        "quest_action",
        &format!("quest_action code={code:#x} done={done}"),
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Async-op probe for `network_error_check_steam` (hash `0xA596AF73`) — the op a 2-player co-op
// quest start parks on (`ASYNC_OP_A596AF73.md` §1 structure table, §6 "cheapest decisive probe").
// Read-only per pass: manager pointer, M1 registration, boot-queue presence, the matching
// vector-A task's state/result, the vector-A `state==2` aggregate, and the vector-B
// pending-notification counts for the hash. Every offset is the report's verified one; nothing is
// inferred. Deliberately NOT read (report does not pin a safe form): the queue entry's
// `std::string` at +8 (SSO vs heap pointer) and the M1 node's name bytes at +0x18 — only the
// node's length at +0x28 is logged — and M3, which the existing quest probe already covers.
// Cost: the line is built only when `log_throttled_lazy` actually emits it (<=1/s); every walk is
// capped; every pointer is `readable`-gated.
const ASYNC_MGR_GLOBAL_RVA: usize = 0x7c22728; // VA 0x147C22728
const ASYNC_QUEUE_BEGIN_RVA: usize = 0x71afbf0; // VA 0x1471AFBF0
const ASYNC_QUEUE_END_RVA: usize = 0x71afbf8; // VA 0x1471AFBF8
const ASYNC_OP_HASH: u32 = 0xA596AF73; // `network_error_check_steam`
const ASYNC_QUEUE_STRIDE: usize = 0x30;
const ASYNC_VA_STRIDE: usize = 0x20;
const ASYNC_VB_STRIDE: usize = 0x48;
const ASYNC_M1_NODE_MAX: usize = 8; // path cap: M1 bucket nodes
const ASYNC_QUEUE_SCAN_MAX: usize = 192; // path cap: boot-queue entries
const ASYNC_VA_SCAN_MAX: usize = 64; // path cap: vector-A elements
const ASYNC_VB_SCAN_MAX: usize = 32; // path cap: vector-B entries

// Read/format helpers shared by the probe lines and the standalone solo sampler. An unreadable
// read is always `None` (never a sentinel a real value could hide behind); formatting only runs
// when the caller has already decided to emit.
/// # Safety
/// These read arbitrary addresses, but every call re-validates through `readable`, so an
/// unreadable or stale pointer yields `None` instead of faulting. They are the sanctioned way
/// to touch game memory in the probe paths.
#[inline]
unsafe fn rd32(p: usize) -> Option<u32> {
    if readable(p, 4) {
        Some(ptr::read_unaligned(p as *const u32))
    } else {
        None
    }
}
#[inline]
unsafe fn rd8(p: usize) -> Option<u8> {
    if readable(p, 1) {
        Some(ptr::read(p as *const u8))
    } else {
        None
    }
}
#[inline]
unsafe fn rdptr(p: usize) -> Option<usize> {
    if readable(p, 8) {
        Some(ptr::read_unaligned(p as *const usize))
    } else {
        None
    }
}
fn s32(v: Option<u32>) -> String {
    match v {
        Some(x) => x.to_string(),
        None => "ro".to_string(),
    }
}
fn s8(v: Option<u8>) -> String {
    match v {
        Some(x) => x.to_string(),
        None => "ro".to_string(),
    }
}
fn sbool(v: Option<bool>) -> String {
    match v {
        Some(true) => "true".to_string(),
        Some(false) => "false".to_string(),
        None => "ro".to_string(),
    }
}
fn sptr(v: Option<usize>) -> String {
    match v {
        Some(x) => format!("{x:#x}"),
        None => "ro".to_string(),
    }
}
fn mix(h: &mut u64, v: Option<u64>) {
    *h ^= v.map_or(u64::MAX, |x| x);
    *h = h.wrapping_mul(0x100_0000_01b3);
}

/// M1 lookup result: `None` = a needed structure was unreadable; otherwise
/// `(present, name_len, truncated)` (`name_len == usize::MAX` = name length unreadable).
type M1Read = Option<(bool, usize, bool)>;
/// Boot-queue scan result: `None` = a needed structure was unreadable; otherwise
/// `(hits, total, first_mode, truncated)` (`first_mode` is `Some` only when `hits > 0`).
type QueueRead = Option<(u32, usize, Option<u32>, bool)>;

/// One snapshot of every `network_error_check_steam` (`0xA596AF73`) field the async probe reads.
/// `asyncload_note` and the solo sampler both format from this, so the two lines can never
/// disagree about a read. Sentinels: `mgr == None` = the manager global itself was unreadable;
/// `mgr == Some(0)` = null; `va_total`/`vb_total == usize::MAX` = unreadable; a nested `None` is
/// the same "ro" the old inline probe printed.
struct AsyncState {
    mgr: Option<usize>,
    m1: M1Read,
    q: QueueRead,
    va_total: usize,
    va_scanned: usize,
    va_read: u32,
    va2: u32,
    va_task: Option<(u32, u32)>,
    va_task_ro: bool,
    vb_total: usize,
    vb_scanned: usize,
    vb1h: u32,
    vb1: u32,
    vb2: u32,
    vb3: u32,
}

/// M1 — registered ops (report §1/P4: buckets at +0x18, mask at +0x30, node key at +0x10).
unsafe fn m1_read(mgr: usize, hash: u32) -> M1Read {
    if !(readable(mgr + 0x18, 8) && readable(mgr + 0x30, 4)) {
        return None;
    }
    let buckets = ptr::read_unaligned((mgr + 0x18) as *const usize);
    let mask = ptr::read_unaligned((mgr + 0x30) as *const u32) as usize;
    if buckets == 0 {
        return Some((false, usize::MAX, false));
    }
    let bp = buckets.wrapping_add((mask & hash as usize).wrapping_mul(8));
    if !readable(bp, 8) {
        return Some((false, usize::MAX, false));
    }
    let sentinel = if readable(mgr + 8, 8) {
        ptr::read_unaligned((mgr + 8) as *const usize)
    } else {
        0
    };
    let mut node = ptr::read_unaligned(bp as *const usize);
    let mut hops = 0usize;
    while node != 0 && node != sentinel && node != mgr + 8 && hops < ASYNC_M1_NODE_MAX {
        if !readable(node + 0x10, 4) {
            return None;
        }
        if ptr::read_unaligned((node + 0x10) as *const u32) == hash {
            let len = if readable(node + 0x28, 8) {
                ptr::read_unaligned((node + 0x28) as *const usize)
            } else {
                usize::MAX
            };
            return Some((true, len, false));
        }
        if !readable(node, 8) {
            return None;
        }
        node = ptr::read_unaligned(node as *const usize);
        hops += 1;
    }
    Some((false, usize::MAX, hops >= ASYNC_M1_NODE_MAX))
}

/// Boot request queue (report §3.1/P6: stride 0x30, mode +0, hash +4).
unsafe fn boot_queue_read(base: usize, hash: u32) -> QueueRead {
    if !readable(base + ASYNC_QUEUE_BEGIN_RVA, 8) || !readable(base + ASYNC_QUEUE_END_RVA, 8) {
        return None;
    }
    let qbeg = ptr::read_unaligned((base + ASYNC_QUEUE_BEGIN_RVA) as *const usize);
    let qend = ptr::read_unaligned((base + ASYNC_QUEUE_END_RVA) as *const usize);
    if qbeg == 0 || qend < qbeg {
        return None;
    }
    let total = (qend - qbeg) / ASYNC_QUEUE_STRIDE;
    let scan = total.min(ASYNC_QUEUE_SCAN_MAX);
    if scan == 0 {
        return Some((0, 0, None, false));
    }
    if !readable(qbeg, scan * ASYNC_QUEUE_STRIDE) {
        return None;
    }
    let mut hits = 0u32;
    let mut mode = None;
    for i in 0..scan {
        let e = qbeg + i * ASYNC_QUEUE_STRIDE;
        let m = ptr::read_unaligned(e as *const u32);
        let h = ptr::read_unaligned((e + 4) as *const u32);
        if h == hash {
            hits += 1;
            if mode.is_none() {
                mode = Some(m);
            }
        }
    }
    Some((hits, total, mode, scan < total))
}

unsafe fn read_async_state() -> AsyncState {
    let mut st = AsyncState {
        mgr: None,
        m1: None,
        q: None,
        va_total: usize::MAX,
        va_scanned: 0,
        va_read: 0,
        va2: 0,
        va_task: None,
        va_task_ro: false,
        vb_total: usize::MAX,
        vb_scanned: 0,
        vb1h: 0,
        vb1: 0,
        vb2: 0,
        vb3: 0,
    };
    let base = exe_base();
    if base == 0 || !readable(base + ASYNC_MGR_GLOBAL_RVA, 8) {
        return st;
    }
    let mgr = ptr::read_unaligned((base + ASYNC_MGR_GLOBAL_RVA) as *const usize);
    st.mgr = Some(mgr);
    if mgr == 0 {
        return st;
    }
    st.m1 = m1_read(mgr, ASYNC_OP_HASH);
    st.q = boot_queue_read(base, ASYNC_OP_HASH);

    // Vector A — loader tasks (report §1/P2: element+0x18 -> task; task +0x110 state,
    // +0x140 result, +0x148 hash). Aggregate counts every readable task's state==2.
    if readable(mgr + 0x40, 16) {
        let abeg = ptr::read_unaligned((mgr + 0x40) as *const usize);
        let aend = ptr::read_unaligned((mgr + 0x48) as *const usize);
        if abeg != 0 && aend >= abeg {
            let total = (aend - abeg) / ASYNC_VA_STRIDE;
            let scan = total.min(ASYNC_VA_SCAN_MAX);
            if scan == 0 {
                st.va_total = 0;
            } else if readable(abeg, scan * ASYNC_VA_STRIDE) {
                st.va_total = total;
                st.va_scanned = scan;
                for i in 0..scan {
                    let el = abeg + i * ASYNC_VA_STRIDE;
                    let task = ptr::read_unaligned((el + 0x18) as *const usize);
                    if task == 0 {
                        continue;
                    }
                    if !readable(task + 0x110, 0x3c) {
                        st.va_task_ro = true;
                        continue;
                    }
                    st.va_read += 1;
                    let state = ptr::read_unaligned((task + 0x110) as *const u32);
                    if state == 2 {
                        st.va2 += 1;
                    }
                    let hash = ptr::read_unaligned((task + 0x148) as *const u32);
                    if hash == ASYNC_OP_HASH && st.va_task.is_none() {
                        let res = ptr::read_unaligned((task + 0x140) as *const u32);
                        st.va_task = Some((state, res));
                    }
                }
            }
        }
    }

    // Vector B — pending-promise/completion queue (report §1/P3: stride 0x48, state +0,
    // hash +4; state 1 = pending marker, 2 = completion notification, 3 = terminator).
    if readable(mgr + 0xd8, 16) {
        let bbeg = ptr::read_unaligned((mgr + 0xd8) as *const usize);
        let bend = ptr::read_unaligned((mgr + 0xe0) as *const usize);
        if bbeg != 0 && bend >= bbeg {
            let total = (bend - bbeg) / ASYNC_VB_STRIDE;
            let scan = total.min(ASYNC_VB_SCAN_MAX);
            if scan == 0 {
                st.vb_total = 0;
            } else if readable(bbeg, scan * ASYNC_VB_STRIDE) {
                st.vb_total = total;
                st.vb_scanned = scan;
                for i in 0..scan {
                    let e = bbeg + i * ASYNC_VB_STRIDE;
                    let s = ptr::read_unaligned(e as *const u32);
                    let hash = ptr::read_unaligned((e + 4) as *const u32);
                    match s {
                        1 => {
                            st.vb1 += 1;
                            if hash == ASYNC_OP_HASH {
                                st.vb1h += 1;
                            }
                        }
                        2 => st.vb2 += 1,
                        3 => st.vb3 += 1,
                        _ => {}
                    }
                }
            }
        }
    }
    st
}

/// One-word reading of the report's §6 interpretation table, derived only from the raw fields
/// (`-` = no case matched, `ro` = a structure needed for the call was unreadable).
fn async_verdict(st: &AsyncState) -> &'static str {
    if st.va_total == usize::MAX || st.vb_total == usize::MAX {
        "ro"
    } else if let Some((2, _)) = st.va_task {
        "completed"
    } else if let Some((1, _)) = st.va_task {
        "loading"
    } else if st.va_task.is_none() && st.q.map_or(false, |q| q.0 > 0) {
        "queued"
    } else if st.va_task.is_none() && matches!(st.m1, Some((true, _, _))) {
        "registered"
    } else if st.va_task.is_none() && st.vb1h > 0 {
        "waited"
    } else if st.va_task.is_none() && matches!(st.m1, Some((false, _, _))) && st.vb1h == 0 {
        "none"
    } else {
        "-"
    }
}

unsafe fn asyncload_note() {
    debug_throttled_lazy("asyncload", |_| {
        let st = read_async_state();
        let mut s = match st.mgr {
            None => return format!("asyncload h={ASYNC_OP_HASH:#010x} mgr=global_ro"),
            Some(0) => return format!("asyncload h={ASYNC_OP_HASH:#010x} mgr=0x0"),
            Some(mgr) => format!("asyncload h={ASYNC_OP_HASH:#010x} mgr={mgr:#x}"),
        };
        // The original inline probe printed `m1trunc=1` before the `m1=` word; keep that order.
        if matches!(st.m1, Some((false, _, true))) {
            s.push_str(" m1trunc=1");
        }
        match st.m1 {
            None => s.push_str(" m1=ro"),
            Some((false, _, _)) => s.push_str(" m1=0"),
            Some((true, usize::MAX, _)) => s.push_str(" m1=1 nm_len=?"),
            Some((true, len, _)) => s.push_str(&format!(" m1=1 nm_len={len}")),
        }
        match st.q {
            None => s.push_str(" q=ro"),
            Some((hits, total, mode, trunc)) => {
                s.push_str(&format!(" q={hits}/{total}"));
                if let Some(m) = mode {
                    s.push_str(&format!(" qmode={m}"));
                }
                if trunc {
                    s.push_str(" qtrunc=1");
                }
            }
        }
        if st.va_total == usize::MAX {
            s.push_str(" va=ro");
        } else {
            s.push_str(&format!(
                " va={}/{} va2={}/{}",
                st.va_scanned, st.va_total, st.va2, st.va_read
            ));
            match st.va_task {
                Some((state, res)) => s.push_str(&format!(" vatask={state}/{res}")),
                None => s.push_str(" vatask=absent"),
            }
            if st.va_task_ro {
                s.push_str(" vatask_ro=1");
            }
            if st.va_scanned < st.va_total {
                s.push_str(" vatrunc=1");
            }
        }
        if st.vb_total == usize::MAX {
            s.push_str(" vb=ro");
        } else {
            s.push_str(&format!(
                " vb1h={} vb1={} vb2={} vb3={} vb={}/{}",
                st.vb1h, st.vb1, st.vb2, st.vb3, st.vb_scanned, st.vb_total
            ));
            if st.vb_scanned < st.vb_total {
                s.push_str(" vbtrunc=1");
            }
        }
        s.push_str(&format!(" v={}", async_verdict(&st)));
        s
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// Mode-3 arming records + service-mediated gate fields (read-only, one probe line).
// Answers the two open items of `MODE3_ARMING.md` (AC) §5 and
// `SERVICE_SURFACE_DIFF.md` (AD) §4.1/§4.4 in a single run:
//   * AC concluded that no screen in our shim-driven flow activates a `LoadAsset` node carrying
//     mode 3, so every mode-3 arming record below should stay 0 for a whole run; a flip both
//     confirms the mechanism and names the call-site family that produced it.
//   * AD established `CanMatchingSetting` = `(6cce8 ? [6c82c] : [6c828]) == (6cce8 ? [6c824] : [6c820])`
//     and that none of these six fields is logged today, so whether the gate is true in our runs
//     is unknown. The raw fields *and* the computed boolean are logged.
// Every address/offset is the report's verified one; nothing is inferred. `IsConnectOnline`
// (AD §4.1, eval VA 0x141D727B0) is read through the exact memory chain its eval reads
// (`[[0x147C52CD0]+0x158]+0x60 & 1`); the eval itself is never called.
// Cost: the raw reads are `readable`-gated and allocation-free, and a 64-bit mix over them decides
// "changed"; the line text is built only when it is emitted — on change (bounded by the
// PROBE_PASS_MS pass gate, so <=5/s) or on the MODE3_GATE_HB_MS heartbeat, never otherwise.
const RM_GLOBAL_RVA: usize = 0x7ab2bb8; // VA 0x147AB2BB8 — resource manager pointer
const RM_ARM3_OFF: usize = 3 * 4 + 0x4e4; // u32: mode-3 arm hold count (AC §5, best record)
const RM_DONE3_OFF: usize = 3 + 0x3e0; // u8: mode-3 queue-pass completed bit (AC §5)
const RM_SKIP3_OFF: usize = 3 + 0x938; // u8: mode-3 one-shot skip latch (AC §5)
const RM_JOBS_A_OFF: usize = 0xdd0; // u32: jobs pending (AC §5)
const RM_JOBS_B_OFF: usize = 0xdd4; // u32: jobs executing (AC §5)
const QM_GLOBAL_RVA: usize = 0x7c48628; // VA 0x147C48628 — quest manager pointer (AD §4.4)
const QM_SEL_OFF: usize = 0x6cce8; // u8: CanMatchingSetting context selector
const QM_OBS0_OFF: usize = 0x6c820; // u32: observed local ordinal, selector 0
const QM_OBS1_OFF: usize = 0x6c824; // u32: observed local ordinal, selector 1
const QM_EXP0_OFF: usize = 0x6c828; // u32: expected local ordinal, selector 0
const QM_EXP1_OFF: usize = 0x6c82c; // u32: expected local ordinal, selector 1
const ISCONNECT_SESSION_OFF: usize = 0x158; // AD §4.1: [[0x147C52CD0]+0x158] -> byte+0x60 & 1
const ISCONNECT_FLAG_OFF: usize = 0x60; //              (session RVA 0x147C52CD0 = SESSION_GLOBAL_RVA)
const MODE3_GATE_HB_MS: u64 = 5000; // heartbeat when the field tuple is unchanged
static MODE3_GATE_KEY: AtomicU64 = AtomicU64::new(u64::MAX);
static MODE3_GATE_AT: AtomicU64 = AtomicU64::new(0);

unsafe fn mode3_gate_note() {
    let base = GetModuleHandleA(ptr::null()) as usize;
    if base == 0 {
        return;
    }

    // (1) Mode-3 arming records. RM = *(u64*)0x147AB2BB8; all fields AC §5-verified.
    let rm = rdptr(base + RM_GLOBAL_RVA);
    let (arm3, done3, skip3, jobs_a, jobs_b) = match rm {
        Some(rm) if rm != 0 => (
            rd32(rm + RM_ARM3_OFF),
            rd8(rm + RM_DONE3_OFF),
            rd8(rm + RM_SKIP3_OFF),
            rd32(rm + RM_JOBS_A_OFF),
            rd32(rm + RM_JOBS_B_OFF),
        ),
        _ => (None, None, None, None, None),
    };

    // (2) Service-mediated gate. qm = *(u64*)0x147C48628 (AD §4.4: `rax=[0x147C48628]`), so the
    // fields are read through the pointer, not from the global itself.
    let qm = rdptr(base + QM_GLOBAL_RVA);
    let (sel, obs0, obs1, exp0, exp1) = match qm {
        Some(qm) if qm != 0 => (
            rd8(qm + QM_SEL_OFF),
            rd32(qm + QM_OBS0_OFF),
            rd32(qm + QM_OBS1_OFF),
            rd32(qm + QM_EXP0_OFF),
            rd32(qm + QM_EXP1_OFF),
        ),
        _ => (None, None, None, None, None),
    };
    // AD headline 5: (6cce8 ? [6c82c] : [6c828]) == (6cce8 ? [6c824] : [6c820]).
    let canmatch = match (sel, obs0, obs1, exp0, exp1) {
        (Some(s), Some(o0), Some(o1), Some(e0), Some(e1)) => {
            Some(if s != 0 { e1 == o1 } else { e0 == o0 })
        }
        _ => None,
    };

    // IsConnectOnline (AD §4.1, cross-checked above): the bytes its eval reads. Its encoding is
    // 2 = true / 3 = false (`xor eax,3`), and it returns false when the inner pointer is null
    // (`test rax,rax; je -> mov eax,3`), which the `Some(0)` arm mirrors; `online` below is the
    // decoded boolean, not the eval's raw 2/3.
    let sess = rdptr(base + SESSION_GLOBAL_RVA);
    let net = match sess {
        Some(s) if s != 0 => rdptr(s + ISCONNECT_SESSION_OFF),
        _ => None,
    };
    let online_raw = match net {
        Some(0) => Some(0), // eval's null-pointer arm: false
        Some(n) => rd8(n + ISCONNECT_FLAG_OFF),
        None => None,
    };
    let online = online_raw.map(|b| b & 1 != 0);

    // Change key over every raw reading; `None` (unreadable) hashes as a distinct value.
    let mut key = 0xcbf2_9ce4_8422_2325u64;
    mix(&mut key, rm.map(|v| v as u64));
    mix(&mut key, arm3.map(u64::from));
    mix(&mut key, done3.map(u64::from));
    mix(&mut key, skip3.map(u64::from));
    mix(&mut key, jobs_a.map(u64::from));
    mix(&mut key, jobs_b.map(u64::from));
    mix(&mut key, qm.map(|v| v as u64));
    mix(&mut key, sel.map(u64::from));
    mix(&mut key, obs0.map(u64::from));
    mix(&mut key, obs1.map(u64::from));
    mix(&mut key, exp0.map(u64::from));
    mix(&mut key, exp1.map(u64::from));
    mix(&mut key, sess.map(|v| v as u64));
    mix(&mut key, net.map(|v| v as u64));
    mix(&mut key, online_raw.map(u64::from));

    let now = now_ms();
    let changed = MODE3_GATE_KEY.swap(key, Ordering::Relaxed) != key;
    let last = MODE3_GATE_AT.load(Ordering::Relaxed);
    let hb_due = last == 0 || now.wrapping_sub(last) >= MODE3_GATE_HB_MS;
    if !changed && !hb_due {
        return;
    }
    MODE3_GATE_AT.store(now, Ordering::Relaxed);

    // Emitting: only now build the line (probe-cost rule).
    debug_log(&format!(
        "mode3gate rm={} arm3={} done3={} skip3={} jobs={}/{} qm={} sel={} obs={}/{} exp={}/{} canmatch={} online={} sess={} net={}",
        sptr(rm),
        s32(arm3),
        s8(done3),
        s8(skip3),
        s32(jobs_a),
        s32(jobs_b),
        sptr(qm),
        s8(sel),
        s32(obs0),
        s32(obs1),
        s32(exp0),
        s32(exp1),
        sbool(canmatch),
        sbool(online),
        sptr(sess),
        sptr(net),
    ));
}

// ─────────────────────────────────────────────────────────────────────────────
// Standalone solo sampler (`solo ...`)
// ─────────────────────────────────────────────────────────────────────────────
//
// Every probe above runs inside `PartyStartProcessingStateChanges`, so a solo quest — the half
// of the A/B where the title never calls Party — is invisible to the whole instrument set. This
// thread samples the game-side fields independently of the title's call rate: one pass every
// `SOLO_SAMPLE_MS`, a line only when the sampled tuple changes, plus a `SOLO_HEARTBEAT_MS`
// liveness line. Started lazily from the first Party export exactly like the transport and broker
// threads (`OnceLock`; never `DllMain`), process-lifetime, never joined. It reads only game memory
// through `readable`-gated pointers and takes no shim lock beyond the log file's, so it cannot
// perturb the transport/broker threading.
//
// Cost per pass (250 ms): seven pointer-gated scalar reads for phase/JoinLobby, six for the
// CanMatchingSetting block, five for the mode-3 records, eight for the quest fields plus
// `6cd08 -> +0x50`, and the shared `read_async_state` (M1 capped at 8 nodes, boot queue at 192
// entries / 9 KB, vector A at 64 tasks, vector B at 32 entries). The change key is built from
// raw `Option<u64>` values; the line — the only allocation — exists only on a change or the 5 s
// heartbeat. A correct solo run therefore emits the same fields with real transitions instead
// of the parked values the tick probes have been showing.
const SOLO_SAMPLE_MS: u64 = 250;
const SOLO_HEARTBEAT_MS: u64 = 5000;
/// VA 0x147C52D20 — main-frame flow object; `[obj+0x34]` gates the session-update call
/// (`SESSION_UPDATE_STALL.md` §1.5, M11).
const SOLO_PHASE_GLOBAL_RVA: usize = 0x7c52d20;
/// VA 0x1471B43D8 — session online object; `[[j]+0x20]+0x1dc` is the `JoinLobby` operation live
/// byte W6/W9 poll (`SESSION_UPDATE_STALL.md` §3, M7).
const SOLO_JOINLOBBY_GLOBAL_RVA: usize = 0x71b43d8;
const SOLO_JOINLOBBY_OBJ_OFF: usize = 0x20;
const SOLO_JOINLOBBY_GATE_OFF: usize = 0x1dc;
/// Quest fields read through the quest-manager pointer — the same list `quest_guard_note`
/// reports (the through-pointer `v` reading). `[[6cd08]+0x50]` is read separately because it
/// needs the MATCH object dereference.
const SOLO_QUEST_OFFS: [(usize, &str); 8] = [
    (0x1c530, "q1c530"),
    (0x6c814, "q6c814"),
    (0x6c838, "q6c838"),
    (0x6ccf4, "q6ccf4"),
    (0x6ccfc, "q6ccfc"),
    (0x1c108, "q1c108"),
    (0x1beb8, "q1beb8"),
    (0x6c7fc, "q6c7fc"),
];
const SOLO_MATCH_OFF: usize = 0x6cd08;
const SOLO_MATCH_RESULT_OFF: usize = 0x50;

static SOLO_STARTED: OnceLock<()> = OnceLock::new();
static SOLO_TID: AtomicU32 = AtomicU32::new(0);
static SOLO_KEY: AtomicU64 = AtomicU64::new(u64::MAX);
static SOLO_AT: AtomicU64 = AtomicU64::new(0);
static SOLO_PASSES: AtomicU64 = AtomicU64::new(0);
static SOLO_EMITTED: AtomicU64 = AtomicU64::new(0);

fn ensure_sampler_thread() {
    if !debug_enabled() {
        return;
    }
    SOLO_STARTED.get_or_init(|| {
        if let Err(e) = std::thread::Builder::new()
            .name("party-solo-sample".into())
            .spawn(solo_sampler_thread_main)
        {
            log_line(&format!(
                "solo sampler thread spawn FAILED: {e}; solo fields will not be sampled"
            ));
        }
        ()
    });
}

fn solo_sampler_thread_main() {
    SOLO_TID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    debug_log(&format!(
        "solo sampler thread started tid={:#010x} period_ms={SOLO_SAMPLE_MS} heartbeat_ms={SOLO_HEARTBEAT_MS} read_only=1",
        SOLO_TID.load(Ordering::Relaxed)
    ));
    loop {
        unsafe { solo_sample_note() };
        std::thread::sleep(Duration::from_millis(SOLO_SAMPLE_MS));
    }
}

unsafe fn solo_sample_note() {
    let base = exe_base();
    if base == 0 {
        return;
    }
    SOLO_PASSES.fetch_add(1, Ordering::Relaxed);

    // Session-update phase: [[0x147C52D20]+0x34].
    let phase = match rdptr(base + SOLO_PHASE_GLOBAL_RVA) {
        Some(p) if p != 0 => rd32(p + 0x34),
        _ => None,
    };
    // JoinLobby live byte: [[0x1471B43D8]+0x20]+0x1dc.
    let jl = rdptr(base + SOLO_JOINLOBBY_GLOBAL_RVA);
    let jl_obj = match jl {
        Some(j) if j != 0 => rdptr(j + SOLO_JOINLOBBY_OBJ_OFF),
        _ => None,
    };
    let jl_gate = match jl_obj {
        Some(l) if l != 0 => rd8(l + SOLO_JOINLOBBY_GATE_OFF),
        _ => None,
    };
    // CanMatchingSetting inputs on the quest manager (AD §4.4).
    let qm = rdptr(base + QM_GLOBAL_RVA);
    let (sel, obs0, obs1, exp0, exp1) = match qm {
        Some(q) if q != 0 => (
            rd8(q + QM_SEL_OFF),
            rd32(q + QM_OBS0_OFF),
            rd32(q + QM_OBS1_OFF),
            rd32(q + QM_EXP0_OFF),
            rd32(q + QM_EXP1_OFF),
        ),
        _ => (None, None, None, None, None),
    };
    let canmatch = match (sel, obs0, obs1, exp0, exp1) {
        (Some(s), Some(o0), Some(o1), Some(e0), Some(e1)) => {
            Some(if s != 0 { e1 == o1 } else { e0 == o0 })
        }
        _ => None,
    };
    // Mode-3 arming records (AC §5).
    let (arm3, done3, skip3) = match rdptr(base + RM_GLOBAL_RVA) {
        Some(rm) if rm != 0 => (
            rd32(rm + RM_ARM3_OFF),
            rd8(rm + RM_DONE3_OFF),
            rd8(rm + RM_SKIP3_OFF),
        ),
        _ => (None, None, None),
    };
    // Quest fields (through the pointer), MATCH and its published result.
    let mut quest: [Option<u32>; 8] = [None; 8];
    if let Some(q) = qm {
        if q != 0 {
            for (i, (off, _)) in SOLO_QUEST_OFFS.iter().enumerate() {
                quest[i] = rd32(q + off);
            }
        }
    }
    let mat = match qm {
        Some(q) if q != 0 => rdptr(q + SOLO_MATCH_OFF),
        _ => None,
    };
    let m50 = match mat {
        Some(m) if m != 0 => rd32(m + SOLO_MATCH_RESULT_OFF),
        _ => None,
    };
    // Async-op record: M1 presence, boot-queue position/mode and the report's verdict word.
    let async_st = read_async_state();
    let verdict = async_verdict(&async_st);

    // Change key over every reading that is printed; unreadable hashes as a distinct value.
    let mut key = 0xcbf2_9ce4_8422_2325u64;
    mix(&mut key, phase.map(u64::from));
    mix(&mut key, jl.map(|v| v as u64));
    mix(&mut key, jl_obj.map(|v| v as u64));
    mix(&mut key, jl_gate.map(u64::from));
    mix(&mut key, sel.map(u64::from));
    mix(&mut key, obs0.map(u64::from));
    mix(&mut key, obs1.map(u64::from));
    mix(&mut key, exp0.map(u64::from));
    mix(&mut key, exp1.map(u64::from));
    mix(&mut key, canmatch.map(u64::from));
    mix(&mut key, arm3.map(u64::from));
    mix(&mut key, done3.map(u64::from));
    mix(&mut key, skip3.map(u64::from));
    mix(&mut key, qm.map(|v| v as u64));
    mix(&mut key, mat.map(|v| v as u64));
    mix(&mut key, m50.map(u64::from));
    for q in quest.iter() {
        mix(&mut key, (*q).map(u64::from));
    }
    mix(&mut key, async_st.mgr.map(|v| v as u64));
    mix(&mut key, async_st.m1.map(|(present, len, tr)| {
        (present as u64) | ((len as u64) << 1) | ((tr as u64) << 33)
    }));
    mix(&mut key, async_st.q.map(|(hits, total, _, _)| {
        (hits as u64) | ((total as u64) << 32)
    }));
    mix(&mut key, async_st.q.and_then(|(_, _, mode, _)| mode).map(u64::from));
    mix(&mut key, async_st.q.map(|(_, _, _, tr)| tr as u64));

    let now = now_ms();
    let changed = SOLO_KEY.swap(key, Ordering::Relaxed) != key;
    let last = SOLO_AT.load(Ordering::Relaxed);
    let hb_due = last == 0 || now.wrapping_sub(last) >= SOLO_HEARTBEAT_MS;
    if !changed && !hb_due {
        return;
    }
    SOLO_AT.store(now, Ordering::Relaxed);
    SOLO_EMITTED.fetch_add(1, Ordering::Relaxed);

    // Emitting: only now build the line (probe-cost rule).
    let m1 = match async_st.m1 {
        None => "ro".to_string(),
        Some((false, _, _)) => "0".to_string(),
        Some((true, _, _)) => "1".to_string(),
    };
    let qs = match async_st.q {
        None => "ro".to_string(),
        Some((hits, total, mode, trunc)) => {
            let mut v = format!("{hits}/{total}");
            if let Some(m) = mode {
                v.push_str(&format!("/m{m}"));
            }
            if trunc {
                v.push_str("/trunc");
            }
            v
        }
    };
    let mut s = format!(
        "solo t={now} tid={:#x} pass={} hb={} upd_phase={} jl_gate={} qmsel={} qm_obs={}/{} qm_exp={}/{} canmatch={} arm3={} done3={} skip3={} m1={}",
        SOLO_TID.load(Ordering::Relaxed),
        SOLO_PASSES.load(Ordering::Relaxed),
        if hb_due { 1 } else { 0 },
        s32(phase),
        s8(jl_gate),
        s8(sel),
        s32(obs0),
        s32(obs1),
        s32(exp0),
        s32(exp1),
        sbool(canmatch),
        s32(arm3),
        s8(done3),
        s8(skip3),
        m1,
    );
    match async_st.m1 {
        Some((true, usize::MAX, _)) => s.push_str(" nm_len=?"),
        Some((true, len, _)) => s.push_str(&format!(" nm_len={len}")),
        _ => {}
    }
    s.push_str(&format!(
        " q={qs} qmode={} v={verdict} mat={} m50={}",
        s32(async_st.q.and_then(|(_, _, mode, _)| mode)),
        sptr(mat),
        s32(m50),
    ));
    for (i, (_, name)) in SOLO_QUEST_OFFS.iter().enumerate() {
        s.push_str(&format!(" {name}={}", s32(quest[i])));
    }
    s.push_str(&format!(" emit={}", SOLO_EMITTED.load(Ordering::Relaxed)));
    debug_log(&s);
}

unsafe fn probe_session() {
    let base = GetModuleHandleA(ptr::null()) as usize;
    if base == 0 {
        return;
    }
    let role = if IS_HOST.load(Ordering::SeqCst) {
        "host"
    } else {
        "guest"
    };
    let mut note = String::from("unreadable");
    let mut state: i32 = -1;
    let mut count: u32 = 0;
    let mut n70: u32 = 0;
    let mut session_ptr = 0usize;

    if readable(base + SESSION_GLOBAL_RVA, 8) {
        let session = ptr::read_unaligned((base + SESSION_GLOBAL_RVA) as *const usize);
        session_ptr = session;
        if session == 0 {
            note = "session=null".into();
        } else if readable(session + 4, 4) {
            state = ptr::read_unaligned((session + 4) as *const i32);
            note = format!("session+4={state}");
            if readable(session + 0x3c0, 8) {
                let p3c0 = ptr::read_unaligned((session + 0x3c0) as *const usize);
                let p480 = if readable(session + 0x480, 8) {
                    ptr::read_unaligned((session + 0x480) as *const usize)
                } else {
                    0
                };
                let task = if readable(session + 0xb8, 8) {
                    ptr::read_unaligned((session + 0xb8) as *const usize)
                } else {
                    0
                };
                let p4a0 = if readable(session + 0x4a0, 8) {
                    ptr::read_unaligned((session + 0x4a0) as *const usize)
                } else {
                    0
                };
                let b4a0 = if p4a0 != 0 && readable(p4a0, 1) {
                    ptr::read(p4a0 as *const u8)
                } else {
                    0
                };
                let mode0 = ptr::read_unaligned(session as *const u32);
                let b4ac = if readable(session + 0x4ac, 1) {
                    ptr::read((session + 0x4ac) as *const u8)
                } else {
                    0xff
                };
                let p28 = if readable(session + 0x28, 8) {
                    ptr::read_unaligned((session + 0x28) as *const usize)
                } else {
                    0
                };
                note.push_str(&format!(
                    " +3c0={p3c0:#x} +480={p480:#x} +4a0={p4a0:#x}/{b4a0} +b8={task:#x} mode0={mode0} +4ac={b4ac} +28={p28:#x} native={}",
                    NATIVE_CONNECT.load(Ordering::SeqCst)
                ));
            }
        } else {
            note = format!("session={session:#x} +4 unreadable");
        }
    }

    let slots = collect_member_slots();
    if slots.is_empty() {
        if readable(base + MEMBER_LIST_GLOBAL_RVA, 8) {
            let members = ptr::read_unaligned((base + MEMBER_LIST_GLOBAL_RVA) as *const usize);
            if members == 0 {
                note.push_str(" members=null");
            } else {
                note.push_str(" members=0");
            }
        } else {
            note.push_str(" member_global unreadable");
        }
    } else {
        count = slots.len() as u32;
        let mut parts = Vec::new();
        for s in &slots {
            if s.flag70 == 1 {
                n70 += 1;
            }
            if s.eid.is_empty() {
                parts.push(format!(
                    "id={} +70={} +8={} +6c={} +a0={}",
                    s.id, s.flag70, s.unique, s.rank, s.a0
                ));
            } else {
                parts.push(format!(
                    "id={} +70={} +8={} +6c={} +a0={} eid={}",
                    s.id, s.flag70, s.unique, s.rank, s.a0, s.eid
                ));
            }
        }
        let flags = parts.join("; ");
        note.push_str(&format!(" members={count} with+70={n70} [{flags}]"));
    }

    note.push_str(&ready_list_note());

    if readable(base + SEND_GLOBAL_RVA, 8) {
        let send_holder = ptr::read_unaligned((base + SEND_GLOBAL_RVA) as *const usize);
        if send_holder == 0 {
            note.push_str(" send=null");
        } else if readable(send_holder + 0x38, 8) {
            let send_obj = ptr::read_unaligned((send_holder + 0x38) as *const usize);
            if send_obj == 0 {
                note.push_str(" send+38=0");
            } else {
                note.push_str(&format!(" send+38={send_obj:#x}"));
            }
        } else {
            note.push_str(" send+38 unreadable");
        }
    }

    append_overlay_note(base, session_ptr, &mut note);

    // Hash the whole note, not its length: the previous key used `note.len()`, so real transitions
    // that kept the same string length (e.g. the `+0xa0` 2->3 walk and the ready-row 2->3 move) were
    // never logged — the probe looked static while the state was changing.
    let key = fnv1a(&note);
    let changed = LAST_PROBE_KEY.swap(key, Ordering::Relaxed) != key;
    let due = {
        let m = LAST_PROBE_LOG_AT.get_or_init(|| Mutex::new(Instant::now()));
        let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
        if g.elapsed() >= Duration::from_secs(10) {
            *g = Instant::now();
            true
        } else {
            false
        }
    };
    if changed || due {
        debug_log(&format!("probe_session {role}: {note} {}", thread_probe_note()));
    }
}

/// TEMPORARY DIAGNOSTIC: when true, StartProcessing hands the title at most one type-21 per
/// batch (the rest stay queued). Used to tell "exe cannot take a burst" apart from
/// "wrong game state for a specific message". Set back to false once answered.
const DIAG_ONE_MSG_PER_BATCH: bool = false;

const EXP_DESERIALIZE: u64 = 1 << 0;
const EXP_CONNECT: u64 = 1 << 1;
const EXP_CREATE_NETWORK: u64 = 1 << 2;
const EXP_START_PROCESSING: u64 = 1 << 3;
const EXP_AUTHENTICATE: u64 = 1 << 4;
const EXP_CREATE_ENDPOINT: u64 = 1 << 5;
const EXP_LEAVE: u64 = 1 << 6;
const EXP_CLEANUP: u64 = 1 << 7;

fn note_export(bit: u64) {
    MESH_EXPORTS.fetch_or(bit, Ordering::Relaxed);
}

fn export_trace() -> String {
    let m = MESH_EXPORTS.load(Ordering::Relaxed);
    let mut v: Vec<&str> = Vec::new();
    for (bit, name) in [
        (EXP_DESERIALIZE, "Deserialize"),
        (EXP_CONNECT, "Connect"),
        (EXP_CREATE_NETWORK, "CreateNewNetwork"),
        (EXP_START_PROCESSING, "StartProcessing"),
        (EXP_AUTHENTICATE, "Authenticate"),
        (EXP_CREATE_ENDPOINT, "CreateEndpoint"),
        (EXP_LEAVE, "LeaveNetwork"),
        (EXP_CLEANUP, "Cleanup"),
    ] {
        if m & bit != 0 {
            v.push(name);
        }
    }
    if v.is_empty() {
        "-".into()
    } else {
        v.join(",")
    }
}

fn fnv1a(s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Read-only summary of the DAT_147c52ce0 rows ProcessParty's queue walk
/// matches against (work `+0x70` is the mesh-start flag, `+0xa0` the state).
unsafe fn work_rows_note() -> String {
    let slots = collect_member_slots();
    if slots.is_empty() {
        return "rows=0".into();
    }
    let parts: Vec<String> = slots
        .iter()
        .map(|s| format!("id={} +8={} +70={} +a0={}", s.id, s.unique, s.flag70, s.a0))
        .collect();
    format!("rows=[{}]", parts.join("; "))
}


/// Read-only summary of the exe's 0x180-stride queue/ready rows (DAT_1471AFB30) — the
/// per-member connect/wait rows ProcessParty's case-4 pairs the work serial against.
unsafe fn queue_rows_note() -> String {
    let base = GetModuleHandleA(ptr::null()) as usize;
    if base == 0 || !readable(base + READY_LIST_GLOBAL_RVA, 8) {
        return "queue=?".into();
    }
    let obj = ptr::read_unaligned((base + READY_LIST_GLOBAL_RVA) as *const usize);
    if obj == 0 || !readable(obj + 0x60, 16) {
        return "queue=?".into();
    }
    let flag = ptr::read_unaligned((obj + 0x50) as *const u8);
    let begin = ptr::read_unaligned((obj + 0x58) as *const usize);
    let end = ptr::read_unaligned((obj + 0x60) as *const usize);
    if begin == 0 || end < begin || (end - begin) % READY_STRIDE != 0 || (end - begin) > READY_STRIDE * 16 {
        return format!(
            "queue={obj:#x} badrange fl={flag} begin={begin:#x} end={end:#x}"
        )
        .into();
    }
    if !readable(begin, end - begin) {
        return format!("queue={obj:#x} unreadable").into();
    }
    let mut ids = Vec::new();
    let mut e = begin;
    while e < end {
        let id = ptr::read_unaligned(e as *const i32);
        let st = ptr::read_unaligned((e + 4) as *const i32);
        let t = ptr::read_unaligned((e + 0x10) as *const u8);
        let skip = ptr::read_unaligned((e + 0x40) as *const u8);
        ids.push(format!("{id}:{st}:t{t}:s{skip}"));
        e += READY_STRIDE;
    }
    format!("queue={obj:#x} fl={flag} [{}]", ids.join("; "))
}

/// One-shot trigger for the exe's mesh-start leaf. Must run on a thread the exe
/// called a shim export on (the leaf uses the caller's TLS async pool).
unsafe fn mesh_trigger_timeline() {
    let left = MESH_TRIGGER_TICKS.load(Ordering::Relaxed);
    if left == 0 {
        return;
    }
    MESH_TRIGGER_TICKS.store(left - 1, Ordering::Relaxed);
    note_export(EXP_START_PROCESSING);
    let base = GetModuleHandleA(ptr::null()) as usize;
    let mut mode0 = 0xffff_ffffu32;
    let mut state4 = 0xffff_ffffu32;
    let mut p28 = 0usize;
    if base != 0 && readable(base + SESSION_GLOBAL_RVA, 8) {
        let session = ptr::read_unaligned((base + SESSION_GLOBAL_RVA) as *const usize);
        if session != 0 && readable(session, 0x30) {
            mode0 = ptr::read_unaligned(session as *const u32);
            state4 = ptr::read_unaligned((session + 4) as *const u32);
            p28 = ptr::read_unaligned((session + 0x28) as *const usize);
        }
    }
    // PTY-8: the invitation field the arming gate now waits on; shown here so the copy landing
    // (or not) is visible in the same timeline as mode0/+28.
    let (party, invitation) = connect_gate_party_invitation();
    let note = format!(
        "mesh-start timeline[{left}] mode0={mode0} +4={state4} +28={p28:#x} party={party:#x} inv={invitation} {} {} exports=[{}]",
        work_rows_note(),
        queue_rows_note(),
        export_trace()
    );
    let key = fnv1a(&note);
    if MESH_TRIGGER_KEY.swap(key, Ordering::Relaxed) != key || left % 8 == 0 {
        debug_log(&note);
    }
}

#[no_mangle]
pub unsafe extern "C" fn PartyStartProcessingStateChanges(
    handle: *mut c_void,
    count: *mut u32,
    changes: *mut *mut *mut u8,
) -> u32 {
    note_party_thread();
    if count.is_null() || changes.is_null() {
        return ERR;
    }
    if MESH_TRIGGER_PENDING.load(Ordering::SeqCst) && !MESH_TRIGGER_DONE.load(Ordering::SeqCst) {
        // PTY-8: only fire once the exe has copied the invitation into party+0x218. Arming from
        // PartyDeserializeNetworkDescriptor fired before that copy, so the gate could take the
        // Create branch with an empty invitation and split-brain the network.
        let (party, invitation) = connect_gate_party_invitation();
        if invitation != 0 {
            // [deleted] the exe mesh-start hack used to be called here; removed because the shim must not drive game internals.
        } else if debug_enabled() {
            debug_throttled_lazy("mesh_arm_wait", |_| {
                format!(
                    "mesh-start armed but party+0x218 is still 0 (party={party:#x}): waiting for the exe invitation copy (PTY-8); not firing from Deserialize"
                )
            });
        }
    }
    mesh_trigger_timeline();
    with_handle(
        handle,
        |h| {
            let now = now_ms();
            // Fail-safe: if the transport thread never started (spawn failure) or has died, run
            // the pre-threading call-driven path so no send can be stranded in the outbox and
            // inbound datagrams/timers still run.
            if !TRANSPORT_UP.load(Ordering::Acquire) {
                service_outbox();
                recv_udp(h);
                reliable_service(h, now);
            }
            let force_announce = h.networks.iter().any(|n| {
                let n = unsafe { &**n };
                (n.type10_delivered && n.remotes.is_empty() && !n.local_endpoint.is_null())
                    || n.poll_requested
            });
            if force_announce || h.last_peer_poll.elapsed() > Duration::from_millis(200) {
                h.last_peer_poll = Instant::now();
                let nets = h.networks.clone();
                for n in nets {
                    if !n.is_null() {
                        unsafe { (*n).poll_requested = false; }
                    }
                    poll_peers_request(n);
                }
            }
            // Peer-poll results are HTTP'd and parsed by the broker thread; they are applied here,
            // on the tick, because they mutate the remote set. The pending counter makes this a
            // single relaxed load on every tick with nothing to apply.
            if PEER_RESULTS_PENDING.load(Ordering::Relaxed) > 0 {
                let nets = h.networks.clone();
                let mut live: Vec<u64> = Vec::new();
                for n in &nets {
                    if !n.is_null() {
                        live.push(unsafe { (**n).poll_epoch });
                    }
                }
                drop_stale_peer_results(&live);
                for n in nets {
                    if !n.is_null() {
                        poll_peers_apply(h, n);
                    }
                }
            }
            // New peers seen on the wire by the transport thread are applied here: the
            // member-list walk must stay on the tick thread.
            if h.networks
                .iter()
                .any(|n| !n.is_null() && unsafe { (**n).pending_peers.len() > 0 })
            {
                let nets = h.networks.clone();
                for n in nets {
                    drain_pending_peers(h, n);
                }
            }
            // CHANGE B: gate the whole probe pass before building any string. The block used to
            // run every frame and only be discarded inside log_throttled, which still paid the
            // format! and member/ready list walks. Now at most one pass per PROBE_PASS_MS; skipped
            // ticks allocate nothing and walk nothing, while the per-key log_throttled cadence
            // (1s) and the exact log content are unchanged.
            if debug_enabled() && probe_tick_due(now) {
                // Unconditional: the guest never holds a network, so gating on that hid the exact
                // side we needed. probe_session throttles itself (on change, or every 10 s).
                probe_session();
                quest_guard_note();
                quest_sync_note();
                quest_array_note();
                quest_action_note();
                // ASYNC_OP_A596AF73.md §6: the network_error_check_steam op the co-op quest start
                // parks on. Keyed separately (`asyncload`), throttled to <=1 line/s, all walks capped.
                asyncload_note();
                // MODE3_ARMING.md §5 + SERVICE_SURFACE_DIFF.md §4.1/§4.4: the mode-3 arming
                // records and the service-mediated CanMatchingSetting/IsConnectOnline gate. Both
                // were unmeasured in every prior run. Change-triggered plus a 5 s heartbeat; the
                // line text is built only when it is actually emitted.
                mode3_gate_note();
                // The load-side readings were only emitted from mesh_trigger_timeline(), which early-
                // returns once MESH_TRIGGER_TICKS hits 0 — i.e. they existed only in the join window,
                // only on the peer that attempted the mesh start. So at a load-screen stall (later,
                // possibly the other peer) there was no `+70`/`+a0` and no ready-row `+0x10` at all.
                // Emit both every pass on both peers; log_throttled caps them at one line/second.
                debug_throttled("rows", &work_rows_note());
                debug_throttled("ready", &queue_rows_note());
            }
            // Reclaim the previous batch. The contract is that each state change is returned to
            // FinishProcessingStateChanges exactly once and the library then reclaims it. We never
            // did: `in_flight` only ever grew and was re-handed out in full on every call, so once
            // it reached the 32-entry cap below, the drain broke immediately on every later call —
            // a permanent, completely silent stop to all state-change delivery (which is what
            // starved the host of the remote type-12 and stalled its member row to -14C).
            // Only build a batch when nothing is outstanding. If the title calls Start again
            // before Finish, `in_flight` still belongs to it: leave it untouched and re-return it
            // below, and leave the queued changes in `pending` for the next real batch.
            if !h.batch_outstanding {
            if !h.in_flight.is_empty() {
                // The title called Start again without Finish for the previous batch. The clear
                // below is the 8.1 reclaim; count it instead of dropping it silently.
                let stale = h.in_flight.len();
                log_throttled_lazy("inflight_stale", |_| {
                    format!("Start reclaimed {stale} change(s) without Finish; title skipped FinishProcessingStateChanges")
                });
            }
            h.in_flight.clear();
            // Type-21 messages are deferred while a type-12 is still owed, because the exe looks
            // the Party peer up during that handler. But the type-12 may sit BEHIND the message in
            // the queue, so defer the message but keep draining, then put the deferred messages
            // back at the front afterwards.
            // P5: the hold is timestamped here (once per drain; the drain itself cannot change it)
            // and `queue_sc` bounds the queue, so a type-12 that never arrives cannot starve the
            // game forever or grow `h.pending` without bound.
            let (type12_holding, type12_timed_out, type12_held_ms) = note_type12_hold(h);
            let mut deferred21: Vec<*mut u8> = Vec::new();
            let mut deferred_for_hold = false;
            while let Some(p) = h.pending.pop_front() {
                if h.in_flight.len() >= 32 {
                    // This cap used to stop delivery silently; make it visible when it bites and
                    // say how many were left queued (backlog 8.2 + D1).
                    h.ledger.cap_hits += 1;
                    let left = h.pending.len() + 1;
                    log_throttled_lazy("in_flight_cap", |_| {
                        format!(
                            "StartProcessing hit the 32-change batch cap: handed_out=32 still_pending={left} (drain truncated)"
                        )
                    });
                    h.pending.push_front(p);
                    break;
                }
                let ty = if p.is_null() {
                    0
                } else {
                    unsafe { ptr::read_unaligned(p as *const u32) }
                };
                // Type 12 must run alone: the exe inserts the Party peer
                // during that handler, and type 21 looks it up immediately.
                if ty == 21 {
                    let has12 = h.in_flight.iter().any(|q| {
                        !q.is_null()
                            && unsafe { ptr::read_unaligned(*q as *const u32) } == 12
                    });
                    // P5: defer only while the type-12 hold is inside its timeout. Once
                    // `note_type12_hold` reports the timeout, the type-21s are delivered rather
                    // than deferred, so a missing type-12 cannot silence the whole pump.
                    if has12 || (type12_holding && !type12_timed_out) {
                        if !has12 {
                            deferred_for_hold = true;
                        }
                        deferred21.push(p);
                        continue;
                    }
                }
                if (ty as usize) < h.ledger.handed.len() {
                    h.ledger.handed[ty as usize] += 1;
                }
                h.in_flight.push(p);
                if ty == 12 {
                    // D1: the hand-out half of the type-12 lifecycle.
                    ledger_line(h, "type12-out");
                    break;
                }
                if ty == 21 && DIAG_ONE_MSG_PER_BATCH {
                    // TEMPORARY DIAGNOSTIC (revert after one run): deliver at most one type-21 per
                    // batch. The host crashed twice at exactly `types=[21x6]`, so this separates
                    // "the exe cannot take a burst of messages in one pump" from "the game state
                    // is wrong for a specific message". Anything not delivered stays in `pending`.
                    break;
                }
            }
            let deferred_count = deferred21.len();
            for p in deferred21.into_iter().rev() {
                h.pending.push_front(p);
            }
            if deferred_count > 0 {
                // P5: rate-limited deferral line with the live queue depth. The text is only
                // built when the line will be emitted (probe-cost rule).
                let count = deferred_count as u64;
                h.ledger.deferred21 += count;
                let depth = h.pending.len();
                let total = h.ledger.deferred21;
                let reason = if deferred_for_hold {
                    "type12-owed"
                } else {
                    "type12-in-this-batch"
                };
                debug_throttled_lazy("type21_defer", |_| {
                    format!(
                        "type-21 deferral pending={depth} deferred={count} deferred_total={total} reason={reason} held_ms={type12_held_ms} timed_out={type12_timed_out} (P5 timeout={TYPE12_HOLD_TIMEOUT_MS}ms cap={PENDING_CAP})"
                    )
                });
            }
            if !h.in_flight.is_empty() {
                h.ledger.batches += 1;
            }
            h.ledger.last_batch = h.in_flight.len() as u64;
            h.batch_outstanding = !h.in_flight.is_empty();
            } else {
                log_throttled(
                    "batch_outstanding",
                    "Start called while a batch is outstanding; re-returning it unchanged",
                );
            }
            *count = h.in_flight.len() as u32;
            if h.in_flight.is_empty() {
                *changes = ptr::null_mut()
            } else {
                *changes = h.in_flight.as_mut_ptr();
                let mut types = [0u32; 32];
                unsafe {
                    for p in &h.in_flight {
                        if p.is_null() {
                            continue;
                        }
                        let ty = ptr::read_unaligned(*p as *const u32);
                        if (ty as usize) < types.len() {
                            types[ty as usize] += 1;
                        }
                        if ty == 4 {
                            log_throttled(
                                "watch4_auth",
                                "WATCH_4 delivering state_change type=4 AuthenticateLocalUserCompleted",
                            );
                        }
                    }
                }
                let mut parts = Vec::new();
                for (ty, n) in types.iter().enumerate() {
                    if *n > 0 {
                        parts.push(format!("{ty}x{n}"));
                    }
                }
                let has_ctrl = types[2] + types[3] + types[4] + types[10] + types[12] + types[13] + types[19] > 0;
                if has_ctrl || should_log_payload("pump") {
                    debug_log(&format!(
                        "PartyStartProcessing n={} types=[{}]",
                        h.in_flight.len(),
                        parts.join(" ")
                    ));
                }
            }
            // CHANGE A/D1 heartbeat: queue depth, the size of the batch just handed out, and the
            // cumulative queued/out/back attribution. The due test runs before `ledger_line`
            // builds or walks anything, so skipped ticks stay allocation-free.
            if ledger_hb_due(now) {
                ledger_line(h, "heartbeat");
            }
        },
        (),
    );
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyFinishProcessingStateChanges(
    handle: *mut c_void,
    count: u32,
    changes: *mut *mut u8,
) -> u32 {
    note_party_thread();
    if changes.is_null() || count == 0 {
        return SUCCESS;
    }
    let n = (count as usize).min(64);
    // Ledger (D1): count what the title actually returned, per type. Collected locally because
    // this loop runs outside the handle lock.
    let mut reclaimed = 0u64;
    let mut back = [0u64; 32];
    let mut saw12 = false;
    // Networks that finished a type-10/type-12 state change. The delivered flags are applied
    // under the global handle mutex below, because the transport thread reads them.
    let mut t10_done: Vec<*mut Network> = Vec::new();
    let mut t12_done: Vec<*mut Network> = Vec::new();
    for i in 0..n {
        let p = *changes.add(i);
        if p.is_null() {
            continue;
        }
        reclaimed += 1;
        let ty = ptr::read_unaligned(p as *const u32);
        if (ty as usize) < back.len() {
            back[ty as usize] += 1;
        }
        if ty == 10 {
            let net = ptr::read_unaligned(p.add(0x10) as *const *mut Network);
            if !net.is_null() {
                t10_done.push(net);
                log_throttled(
                    "type10_done",
                    "type10 finished; will announce remotes next pump",
                );
            }
        }
        if ty == 12 {
            saw12 = true;
            let net = ptr::read_unaligned(p.add(8) as *const *mut Network);
            if !net.is_null() {
                t12_done.push(net);
                let _ = TYPE12_MS.compare_exchange(0, now_ms(), Ordering::SeqCst, Ordering::SeqCst);
                debug_log("type12 finished; will replay recv next pump");
            }
        }
        if ty == 21 {
            let payload = ptr::read_unaligned(p.add(0x30) as *const *const u8);
            let len = ptr::read_unaligned(p.add(0x2C) as *const u32) as usize;
            if !payload.is_null() && len >= 8 && readable(payload as usize, 8) {
                let op = ptr::read_unaligned(payload as *const u32);
                let sub = ptr::read_unaligned(payload.add(4) as *const u32);
                if op == 3 && sub == 7 {
                    SUB7_APPLIED.store(true, Ordering::SeqCst);
                }
                debug_log(&format!("type21 finished opcode={op} sub={sub} len={len}"));
            } else {
                debug_log("type21 finished");
            }
        }
    }
    // Reclaim the batch. The title has returned it, so this is the point at which the library may
    // release the array and its changes — and the point at which `in_flight` becomes reusable.
    with_handle(
        handle,
        |h| {
            // Apply the delivered flags under the lock the transport thread reads them through.
            for net in &t10_done {
                unsafe { (**net).type10_delivered = true };
            }
            for net in &t12_done {
                unsafe { (**net).type12_delivered = true };
            }
            let in_flight_before = h.in_flight.len();
            h.in_flight.clear();
            h.batch_outstanding = false;
            h.ledger.last_reclaimed = reclaimed;
            for (ty, c) in back.iter().enumerate() {
                h.ledger.reclaimed[ty] += *c;
            }
            if saw12 {
                // D1: the third type-12 lifecycle point (queued -> handed out -> reclaimed).
                ledger_line(h, "type12-back");
            } else {
                // Rate-limited: a type-21 burst can finish several batches per second, and the
                // number reclaimed is already carried by `last_reclaimed` in the heartbeat.
                let hr: &Handle = h;
                if debug_enabled() {
                    debug_throttled_lazy("ledger_finish", |_| {
                        format!(
                            "delivery ledger[finish] reclaimed={reclaimed} in_flight_before={in_flight_before} back=[{}] pending={}",
                            fmt_type_counts(&back),
                            hr.pending.len()
                        )
                    });
                }
            }
        },
        (),
    );
    SUCCESS
}

unsafe fn log_leave_stack() {
    let mut frames = [ptr::null_mut::<c_void>(); 10];
    let n = RtlCaptureStackBackTrace(1, 10, frames.as_mut_ptr(), ptr::null_mut());
    let exe = GetModuleHandleA(ptr::null()) as usize;
    let mut s = String::from("LeaveNetwork stack");
    for i in 0..n as usize {
        let p = frames[i] as usize;
        if exe != 0 && p >= exe && p < exe + 0x1000_0000 {
            s.push_str(&format!(" rva={:#x}", p - exe));
        } else {
            s.push_str(&format!(" {:p}", frames[i]));
        }
    }
    debug_log(&s);
}

#[no_mangle]
pub unsafe extern "C" fn PartyNetworkLeaveNetwork(
    network: *mut c_void,
    _async: *mut c_void,
) -> u32 {
    note_party_thread();
    log_leave_stack();
    debug_log("PartyNetworkLeaveNetwork");
    note_export(EXP_LEAVE);
    with_handle(
        ptr::null_mut(),
        |h| {
            // PartyLeaveNetworkCompletedStateChange is 0x20: result@4, errorDetail@8,
            // network@0x10, asyncIdentifier@0x18.
            let sc = alloc_sc(0x20, 19);
            if !sc.is_null() {
                ptr::write_unaligned(sc.add(4) as *mut u32, 0);
                ptr::write_unaligned(sc.add(0x10) as *mut *mut c_void, network);
                queue_sc(h, sc);
            }
            if h.networks.iter().any(|&n| n as *mut c_void == network) {
                let net = network as *mut Network;
                // Tell the broker we left so the host's poll_peers stops
                // seeing this member (it would otherwise linger up to 30s).
                let ent = local_entity(&*net);
                if !ent.is_empty() {
                    // Fire-and-forget: the response was always ignored.
                    dispatch_broker(BrokerTask::Leave {
                        network_id: (*net).descriptor.id_str(),
                        entity: ent,
                    });
                }
                // Deliver anything already queued for this network before the socket goes away.
                // Pre-threading sends were synchronous, so a send immediately before a leave hit
                // the wire; without this it would be dropped by `send_udp_all`'s None check.
                drain_outbox_for(net);
                // Close the UDP socket so the fixed port (e.g. 27015) is free
                // for the next connect in this process. Do NOT free the
                // Network box: the exe still holds raw *mut Network /
                // *mut Endpoint pointers and may call shim APIs with them.
                (*net).udp = None;
                (*net).connect_completed = false;
                // Endpoint destroyed: guaranteed delivery no longer applies to this pairing.
                for ep in (*net).remotes.values_mut() {
                    if !ep.is_null() {
                        (**ep).tx.drop_all();
                        (**ep).rx = reliable::Receiver::new();
                    }
                }
            }
            h.networks.retain(|n| *n as *mut c_void != network);
        },
        (),
    );
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyDestroyLocalUser(
    handle: *mut c_void,
    user: *mut c_void,
    _async: *mut c_void,
) -> u32 {
    note_party_thread();
    with_handle(
        handle,
        |h| h.users.retain(|u| *u as *mut c_void != user),
        (),
    );
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyCleanup(handle: *mut c_void) -> u32 {
    note_party_thread();
    debug_log("PartyCleanup");
    note_export(EXP_CLEANUP);
    let _ = handle;
    if let Ok(mut guard) = g().lock() {
        if let Some(h) = guard.as_mut() {
            // Flush queued sends while every socket is still bound (same reason as leave).
            for &net in &h.networks {
                if !net.is_null() {
                    drain_outbox_for(net);
                }
            }
            // Release every bound UDP socket (frees the fixed port). The
            // Network boxes themselves stay allocated: the exe may still
            // hold raw pointers into them after cleanup.
            for &net in &h.networks {
                if !net.is_null() {
                    (*net).udp = None;
                    (*net).connect_completed = false;
                    // Endpoint destroyed: guaranteed delivery no longer applies.
                    for ep in (*net).remotes.values_mut() {
                        if !ep.is_null() {
                            (**ep).tx.drop_all();
                            (**ep).rx = reliable::Receiver::new();
                        }
                    }
                }
            }
        }
        *guard = None;
    }
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyGetNetworks(
    handle: *mut c_void,
    count: *mut u32,
    networks: *mut *mut *mut c_void,
) -> u32 {
    note_party_thread();
    with_handle(
        handle,
        |h| {
            if !count.is_null() {
                *count = (h.networks.len() as u32).min(8);
            }
            if !networks.is_null() {
                *networks = h.networks.as_mut_ptr() as *mut *mut c_void;
            }
        },
        (),
    );
    SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn PartyGetErrorMessage(error: u32, out: *mut *const c_char) -> u32 {
    note_party_thread();
    if out.is_null() {
        return ERR;
    }
    let msg = ERR_MSG.get_or_init(|| CString::new("lan party stub").unwrap());
    let _ = error;
    *out = msg.as_ptr();
    SUCCESS
}

#[no_mangle]
pub extern "system" fn DllMain(_m: *mut c_void, reason: u32, _r: *mut c_void) -> i32 {
    if reason == 1 {
        debug_log(concat!(
            "PartyWin.dll LAN stub loaded build=",
            env!("BUILD_STAMP")
        ));
    }
    1
}

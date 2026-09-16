//! Pure reliable-delivery logic for the Party transport shim.
//!
//! This file contains no Win32 calls and no logging. It is compiled twice: into the shim itself
//! (`mod reliable;` from `src/lib.rs`) and into `reliable_test.rs`, so the standalone
//! loss/duplication/reordering tests exercise exactly the code PartyWin.dll runs.
//!
//! Semantics implemented (Microsoft Learn, `PartySendMessageOptions`):
//!   * `GuaranteedDelivery` (0x1): the message must arrive at each target endpoint, retransmitting
//!     as needed, unless that endpoint is destroyed. It does not imply ordering.
//!   * `SequentialDelivery` (0x2): orders only messages from this local endpoint to that target
//!     endpoint; each endpoint pairing is its own sequence space.
//!   * Sequential + BestEffort: out-of-order (older) arrivals are dropped. Nothing is buffered and
//!     no message is ever delivered after a newer one.
//!   * Sequential + Guaranteed: out-of-order arrivals are queued in a bounded buffer and delivered
//!     strictly in order.
//!   * Acks: cumulative over the guaranteed sequence space. Receivers piggyback the ack on return
//!     traffic; with the default `RequireTimelyAcknowledgement` (0x0, ignored unless 0x1 is set)
//!     they force a standalone ack after `ACK_DELAY_MS`. `AllowLazyAcknowledgement` (0x10)
//!     suppresses the forced ack.
//!
//! Sequence spaces are per (local endpoint -> target endpoint) pairing. The guaranteed space uses
//! one counter and is the subject of cumulative acks; best-effort sequential messages use a
//! separate counter so a best-effort loss can never stall guaranteed acknowledgement.

#![allow(dead_code)]

use std::collections::BTreeMap;

/// Master switch. `false` restores the pre-reliability behaviour exactly (wire v2, no acks, no
/// retransmits, no reordering state, the old "not implemented" log).
pub const PARTY_RELIABLE: bool = true;

// PartySendMessageOptions bits (documented).
pub const SEND_GUARANTEED: u32 = 0x1;
pub const SEND_SEQUENTIAL: u32 = 0x2;
pub const SEND_DONT_COPY_DATA_BUFFERS: u32 = 0x4;
pub const SEND_ALWAYS_COALESCE_UNTIL_FLUSHED: u32 = 0x8;
pub const SEND_ALLOW_LAZY_ACKNOWLEDGEMENT: u32 = 0x10;

/// Initial retransmission timeout; doubles per retry up to `RTO_MAX_MS`.
pub const RTO_FIRST_MS: u64 = 60;
pub const RTO_MAX_MS: u64 = 500;
/// Retry budget per message. After this the shim stops retransmitting (loudly) but keeps the
/// message pending until it is acked, evicted, or the endpoint is destroyed.
pub const MAX_RETRIES: u32 = 8;
/// Hard cap on the per-pairing pending map; oldest (lowest sequence) is evicted with a loud log.
pub const PENDING_CAP: usize = 4096;
/// Hard cap on the guaranteed out-of-order buffer / duplicate set.
pub const OOO_CAP: usize = 256;
/// Forced standalone ack delay for RequireTimelyAcknowledgement.
pub const ACK_DELAY_MS: u64 = 30;

#[inline]
pub fn guaranteed(options: u32) -> bool {
    options & SEND_GUARANTEED != 0
}

#[inline]
pub fn sequential(options: u32) -> bool {
    options & SEND_SEQUENTIAL != 0
}

/// True when `a` is newer than `b` in the wrapping 32-bit sequence space.
#[inline]
pub fn seq_after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// True when `a` is `<=` `b` in the wrapping 32-bit sequence space.
#[inline]
pub fn seq_le(a: u32, b: u32) -> bool {
    !seq_after(a, b)
}

/// Delivery-mode index: 0 = guaranteed+sequential, 1 = guaranteed+nonsequential,
/// 2 = best-effort+sequential, 3 = best-effort+nonsequential.
pub fn mode_index(options: u32) -> usize {
    match (guaranteed(options), sequential(options)) {
        (true, true) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (false, false) => 3,
    }
}

pub fn mode_name(mode: usize) -> &'static str {
    ["guar-seq", "guar-nonseq", "best-seq", "best-nonseq"][mode]
}

/// Human-readable decode of the full options word, with the documented bit names.
pub fn decode_options(options: u32) -> String {
    let mut bits = Vec::new();
    if guaranteed(options) {
        bits.push("GuaranteedDelivery(0x1)");
    } else {
        bits.push("BestEffortDelivery(0x0)");
    }
    if sequential(options) {
        bits.push("SequentialDelivery(0x2)");
    } else {
        bits.push("NonsequentialDelivery(0x0)");
    }
    if options & SEND_DONT_COPY_DATA_BUFFERS != 0 {
        bits.push("DontCopyDataBuffers(0x4)");
    }
    if options & SEND_ALWAYS_COALESCE_UNTIL_FLUSHED != 0 {
        bits.push("AlwaysCoalesceUntilFlushed(0x8)");
    }
    if options & SEND_ALLOW_LAZY_ACKNOWLEDGEMENT != 0 {
        bits.push("AllowLazyAcknowledgement(0x10)");
    } else {
        bits.push("RequireTimelyAcknowledgement(0x0 default; ignored unless 0x1 is set)");
    }
    format!("{options:#010x} [{}]", bits.join("|"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Sender: per (local endpoint -> target endpoint) pairing.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Pending {
    pub payload: Vec<u8>,
    pub options: u32,
    /// When the message was first sent (kept for diagnostics; RTO uses `last_sent_ms`).
    pub first_sent_ms: u64,
    pub last_sent_ms: u64,
    pub retries: u32,
    pub rto_ms: u64,
    pub gave_up: bool,
}

/// The result of queueing one outgoing message.
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub seq: u32,
    /// Payload to put on the wire now (identical to the payload handed in).
    pub payload: Vec<u8>,
    pub options: u32,
    /// `(seq, options)` of a pending message evicted to stay under `PENDING_CAP`.
    pub evicted: Option<(u32, u32)>,
}

/// One due retransmission. `gave_up` events carry an empty payload and must not be sent.
#[derive(Clone, Debug)]
pub struct Retransmit {
    pub seq: u32,
    pub payload: Vec<u8>,
    pub options: u32,
    pub retries: u32,
    /// True the first time this message is retransmitted (drives the once-per-message log).
    pub first: bool,
    pub gave_up: bool,
}

pub struct Sender {
    /// Next guaranteed-space sequence number (1-based; 0 is reserved for unsequenced messages).
    pub next_gseq: u32,
    /// Next best-effort-sequential sequence number (separate space).
    pub next_bseq: u32,
    pub pending: BTreeMap<u32, Pending>,
}

impl Default for Sender {
    fn default() -> Self {
        Self::new()
    }
}

impl Sender {
    pub fn new() -> Self {
        Self {
            next_gseq: 1,
            next_bseq: 1,
            pending: BTreeMap::new(),
        }
    }

    /// Allocate a sequence (when the mode needs one) and, for guaranteed messages, record the
    /// message for retransmission. `payload` is moved into the result; the pending copy is taken
    /// here.
    pub fn send(&mut self, payload: Vec<u8>, options: u32, now_ms: u64) -> Outgoing {
        if !PARTY_RELIABLE {
            return Outgoing {
                seq: 0,
                payload,
                options,
                evicted: None,
            };
        }
        let mut evicted = None;
        let seq = if guaranteed(options) {
            let seq = self.next_gseq;
            self.next_gseq = self.next_gseq.wrapping_add(1);
            if self.pending.len() >= PENDING_CAP {
                if let Some((&k, v)) = self.pending.iter().next() {
                    // Oldest outstanding message is the lowest sequence.
                    evicted = Some((k, v.options));
                    self.pending.remove(&k);
                }
            }
            self.pending.insert(
                seq,
                Pending {
                    payload: payload.clone(),
                    options,
                    first_sent_ms: now_ms,
                    last_sent_ms: now_ms,
                    retries: 0,
                    rto_ms: RTO_FIRST_MS,
                    gave_up: false,
                },
            );
            seq
        } else if sequential(options) {
            let seq = self.next_bseq;
            self.next_bseq = self.next_bseq.wrapping_add(1);
            seq
        } else {
            0
        };
        Outgoing {
            seq,
            payload,
            options,
            evicted,
        }
    }

    /// Cumulative ack: clear every pending guaranteed message with `seq <= ack`.
    /// Returns `(seq, options)` for each cleared message.
    pub fn on_ack(&mut self, ack: u32) -> Vec<(u32, u32)> {
        let mut cleared = Vec::new();
        loop {
            let Some((&k, _)) = self.pending.iter().next() else {
                break;
            };
            if seq_le(k, ack) {
                if let Some(p) = self.pending.remove(&k) {
                    cleared.push((k, p.options));
                }
            } else {
                break;
            }
        }
        cleared
    }

    /// Earliest `last_sent_ms + rto_ms` over every message still awaiting an ack, or `None` when
    /// nothing is pending (or everything pending has given up). The transport thread uses this to
    /// size its idle wait, and the timer-driven test uses it to advance a clock that is
    /// independent of any `send` call: the RTO deadline exists whether or not the game calls us.
    pub fn next_rto_ms(&self) -> Option<u64> {
        self.pending
            .values()
            .filter(|p| !p.gave_up)
            .map(|p| p.last_sent_ms.saturating_add(p.rto_ms))
            .min()
    }

    /// Messages whose RTO has expired. Retries are incremented and RTO doubled here; the payload
    /// is cloned so the caller can put it straight on the wire. At most one `gave_up` event per
    /// message is produced.
    pub fn due(&mut self, now_ms: u64) -> Vec<Retransmit> {
        let mut out = Vec::new();
        if !PARTY_RELIABLE {
            return out;
        }
        for (&seq, p) in self.pending.iter_mut() {
            if p.gave_up {
                continue;
            }
            if now_ms.saturating_sub(p.last_sent_ms) < p.rto_ms {
                continue;
            }
            if p.retries >= MAX_RETRIES {
                p.gave_up = true;
                out.push(Retransmit {
                    seq,
                    payload: Vec::new(),
                    options: p.options,
                    retries: p.retries,
                    first: false,
                    gave_up: true,
                });
                continue;
            }
            p.retries += 1;
            p.last_sent_ms = now_ms;
            p.rto_ms = p.rto_ms.saturating_mul(2).min(RTO_MAX_MS);
            out.push(Retransmit {
                seq,
                payload: p.payload.clone(),
                options: p.options,
                retries: p.retries,
                first: p.retries == 1,
                gave_up: false,
            });
        }
        out
    }

    /// Endpoint destroyed: guaranteed delivery no longer applies.
    pub fn drop_all(&mut self) -> usize {
        let n = self.pending.len();
        self.pending.clear();
        self.next_gseq = 1;
        self.next_bseq = 1;
        n
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Receiver: per (remote endpoint -> local endpoint) pairing.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct RecvMsg {
    pub payload: Vec<u8>,
    pub options: u32,
}

#[derive(Clone, Debug)]
pub enum RecvOutcome {
    /// Payloads released to the game, in order. May contain more than one entry when a message
    /// filled a gap and previously queued sequential messages were drained behind it.
    Delivered(Vec<RecvMsg>),
    /// Buffered waiting for earlier sequential guaranteed messages.
    Queued { seq: u32, expected: u32 },
    DroppedDuplicate,
    /// Best-effort sequential message older than the high-water sequence (never buffered).
    DroppedOutOfOrder { seq: u32, high: u32 },
}

pub struct Receiver {
    /// Next contiguous guaranteed sequence number. Starts at 1; `ack_value()` is `g_next - 1`.
    pub g_next: u32,
    /// Guaranteed messages received but not yet part of the contiguous prefix. Sequential entries
    /// carry the payload for in-order delivery; nonsequential entries are empty markers used to
    /// drop duplicates and advance the cumulative ack.
    pub g_buf: BTreeMap<u32, (u32, Vec<u8>)>,
    /// High-water sequence for best-effort sequential messages (its own space).
    pub be_last: u32,
    pub be_seen: bool,
    /// When the first un-acked guaranteed message arrived (for the forced standalone ack).
    pub ack_due_at: Option<u64>,
    /// Set by the last `on_message` call when a bounded buffer had to evict an entry.
    pub last_evicted: Option<u32>,
}

impl Default for Receiver {
    fn default() -> Self {
        Self::new()
    }
}

impl Receiver {
    pub fn new() -> Self {
        Self {
            g_next: 1,
            g_buf: BTreeMap::new(),
            be_last: 0,
            be_seen: false,
            ack_due_at: None,
            last_evicted: None,
        }
    }

    pub fn ack_value(&self) -> u32 {
        self.g_next.wrapping_sub(1)
    }

    pub fn buffered_len(&self) -> usize {
        self.g_buf.len()
    }

    /// Feed one decoded data message through the ordering/ack machinery.
    pub fn on_message(
        &mut self,
        seq: u32,
        options: u32,
        payload: Vec<u8>,
        now_ms: u64,
    ) -> RecvOutcome {
        self.last_evicted = None;
        if !PARTY_RELIABLE || seq == 0 {
            // Unsequenced (plain best-effort) or reliability disabled: immediate pass-through.
            return RecvOutcome::Delivered(vec![RecvMsg { payload, options }]);
        }
        if guaranteed(options) {
            // Every guaranteed datagram needs an ack, duplicates included (a lost ack is why the
            // duplicate arrived). Lazy ack senders opt out of the forced standalone ack.
            if options & SEND_ALLOW_LAZY_ACKNOWLEDGEMENT == 0 {
                self.ack_due_at.get_or_insert(now_ms);
            }
            if seq_le(seq, self.g_next.wrapping_sub(1)) || self.g_buf.contains_key(&seq) {
                return RecvOutcome::DroppedDuplicate;
            }
            if sequential(options) {
                if seq == self.g_next {
                    // In the contiguous prefix: deliver it and drain whatever it unblocks.
                    let mut out = Vec::new();
                    if !payload.is_empty() {
                        out.push(RecvMsg { payload, options });
                    }
                    self.g_next = self.g_next.wrapping_add(1);
                    while let Some((opts, p)) = self.g_buf.remove(&self.g_next) {
                        self.g_next = self.g_next.wrapping_add(1);
                        if !p.is_empty() {
                            out.push(RecvMsg {
                                payload: p,
                                options: opts,
                            });
                        }
                    }
                    return RecvOutcome::Delivered(out);
                }
                // Out of order: bounded buffer, newest evicted first so the prefix stays reachable.
                if self.g_buf.len() >= OOO_CAP {
                    if let Some((&k, _)) = self.g_buf.iter().next_back() {
                        self.g_buf.remove(&k);
                        self.last_evicted = Some(k);
                    }
                }
                self.g_buf.insert(seq, (options, payload));
                return RecvOutcome::Queued {
                    seq,
                    expected: self.g_next,
                };
            }
            // Nonsequential guaranteed: immediate pass-through, remember only the sequence as a
            // duplicate marker, and advance the cumulative ack through any contiguous run.
            if self.g_buf.len() >= OOO_CAP {
                if let Some((&k, _)) = self.g_buf.iter().next_back() {
                    self.g_buf.remove(&k);
                    self.last_evicted = Some(k);
                }
            }
            self.g_buf.insert(seq, (options, Vec::new()));
            let mut out = vec![RecvMsg { payload, options }];
            while let Some((opts, p)) = self.g_buf.remove(&self.g_next) {
                self.g_next = self.g_next.wrapping_add(1);
                if !p.is_empty() {
                    out.push(RecvMsg {
                        payload: p,
                        options: opts,
                    });
                }
            }
            return RecvOutcome::Delivered(out);
        }
        if sequential(options) {
            // Best-effort + sequential: high-water only. Older arrivals are dropped and nothing is
            // ever buffered, so a newer message can never follow an older one.
            if self.be_seen && !seq_after(seq, self.be_last) {
                return RecvOutcome::DroppedOutOfOrder {
                    seq,
                    high: self.be_last,
                };
            }
            self.be_seen = true;
            self.be_last = seq;
            return RecvOutcome::Delivered(vec![RecvMsg { payload, options }]);
        }
        RecvOutcome::Delivered(vec![RecvMsg { payload, options }])
    }

    /// Called when a packet is about to be sent back to this peer: return the current cumulative
    /// ack and clear the forced-ack timer (normal piggyback case).
    pub fn piggyback_ack(&mut self) -> u32 {
        self.ack_due_at = None;
        self.ack_value()
    }

    /// `ack_due_at + ACK_DELAY_MS` when a forced standalone ack is owed, else `None`. Same
    /// clock-driven contract as `Sender::next_rto_ms`: no send call is involved in the deadline.
    pub fn next_ack_due_ms(&self) -> Option<u64> {
        self.ack_due_at.map(|t| t.saturating_add(ACK_DELAY_MS))
    }

    /// Return `Some(ack)` when no return packet carried the ack within `ACK_DELAY_MS`.
    pub fn forced_ack(&mut self, now_ms: u64) -> Option<u32> {
        if let Some(t) = self.ack_due_at {
            if now_ms.saturating_sub(t) >= ACK_DELAY_MS {
                self.ack_due_at = None;
                return Some(self.ack_value());
            }
        }
        None
    }
}

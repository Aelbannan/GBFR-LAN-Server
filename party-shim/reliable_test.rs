//! Standalone validation for the pure reliable-delivery logic in `src/reliable.rs`.
//!
//! Build with plain rustc (no cargo, matching the shim's build):
//!     rustc -O -o reliable_test.exe reliable_test.rs
//!     .\reliable_test.exe
//!
//! The simulated link drops, duplicates and reorders datagrams; every assertion runs against the
//! same code PartyWin.dll executes. A retransmit/ordering bug here is indistinguishable from the
//! quest-start hang we are chasing, so this test is mandatory.

#[path = "src/reliable.rs"]
mod reliable;

use reliable::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};

static FAILURES: AtomicU32 = AtomicU32::new(0);

fn check(name: &str, ok: bool, detail: &str) {
    if ok {
        println!("PASS  {name}  {detail}");
    } else {
        println!("FAIL  {name}  {detail}");
        FAILURES.fetch_add(1, Ordering::SeqCst);
    }
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn payload(i: u32) -> Vec<u8> {
    format!("msg-{i:04}").into_bytes()
}

#[derive(Default)]
struct SimResult {
    delivered: Vec<Vec<u8>>,
    retransmits: u32,
    converged: bool,
    pending_left: usize,
    buffered_left: usize,
}

/// Deterministic lossy/duplicating/reordering link between one Sender and one Receiver.
/// `drop_mod`: drop roughly one in every `drop_mod` transmissions (0 = no drops).
/// `dup_mod`: duplicate roughly one in every `dup_mod` transmissions (0 = no dups).
/// `jitter_ms`: maximum one-way delay.
fn schedule(
    map: &mut BTreeMap<u64, Vec<(u32, Vec<u8>)>>,
    rng: &mut Lcg,
    now: u64,
    seq: u32,
    pay: Vec<u8>,
    drop_mod: u64,
    dup_mod: u64,
    jitter_ms: u64,
) {
    if drop_mod != 0 && rng.next() % drop_mod == 0 {
        return;
    }
    let jitter = if jitter_ms == 0 { 0 } else { rng.next() % (jitter_ms + 1) };
    map.entry(now + jitter).or_default().push((seq, pay.clone()));
    if dup_mod != 0 && rng.next() % dup_mod == 0 {
        let jitter2 = if jitter_ms == 0 { 0 } else { rng.next() % (jitter_ms + 1) };
        map.entry(now + jitter + jitter2).or_default().push((seq, pay));
    }
}

fn simulate(
    seed: u64,
    n: u32,
    options: u32,
    drop_mod: u64,
    dup_mod: u64,
    jitter_ms: u64,
) -> SimResult {
    let mut rng = Lcg(seed);
    let mut tx = Sender::new();
    let mut rx = Receiver::new();
    let mut a2b: BTreeMap<u64, Vec<(u32, Vec<u8>)>> = BTreeMap::new();
    let mut b2a: BTreeMap<u64, Vec<u32>> = BTreeMap::new();
    let mut delivered: Vec<Vec<u8>> = Vec::new();
    let mut retransmits = 0u32;
    let mut next_msg = 0u32;
    let mut next_send_at = 0u64;

    let mut now = 0u64;
    while now < 60_000 {
        if next_msg < n && now >= next_send_at {
            let out = tx.send(payload(next_msg), options, now);
            schedule(
                &mut a2b,
                &mut rng,
                now,
                out.seq,
                out.payload,
                drop_mod,
                dup_mod,
                jitter_ms,
            );
            next_msg += 1;
            next_send_at = now + 7;
        }

        for r in tx.due(now) {
            if r.gave_up {
                continue;
            }
            retransmits += 1;
            schedule(
                &mut a2b,
                &mut rng,
                now,
                r.seq,
                r.payload,
                drop_mod,
                dup_mod,
                jitter_ms,
            );
        }

        while let Some((&t, _)) = a2b.first_key_value() {
            if t > now {
                break;
            }
            let pkts = a2b.remove(&t).unwrap();
            for (seq, pay) in pkts {
                match rx.on_message(seq, options, pay, t) {
                    RecvOutcome::Delivered(msgs) => {
                        for m in msgs {
                            delivered.push(m.payload);
                        }
                    }
                    RecvOutcome::Queued { .. } => {}
                    RecvOutcome::DroppedDuplicate => {}
                    RecvOutcome::DroppedOutOfOrder { .. } => {}
                }
                if guaranteed(options) {
                    // Simulate application return traffic for most messages; every 5th message
                    // gets no return packet, exercising the forced standalone ack.
                    if seq % 5 != 0 {
                        let ack = rx.piggyback_ack();
                        let jitter = if jitter_ms == 0 { 0 } else { rng.next() % (jitter_ms + 1) };
                        b2a.entry(t + jitter).or_default().push(ack);
                    }
                }
            }
        }

        if let Some(ack) = rx.forced_ack(now) {
            let jitter = if jitter_ms == 0 { 0 } else { rng.next() % (jitter_ms + 1) };
            b2a.entry(now + jitter).or_default().push(ack);
        }

        while let Some((&t, _)) = b2a.first_key_value() {
            if t > now {
                break;
            }
            for ack in b2a.remove(&t).unwrap() {
                tx.on_ack(ack);
            }
        }

        if next_msg == n
            && a2b.is_empty()
            && b2a.is_empty()
            && tx.pending.is_empty()
            && rx.buffered_len() == 0
        {
            return SimResult {
                delivered,
                retransmits,
                converged: true,
                pending_left: 0,
                buffered_left: 0,
            };
        }
        now += 1;
    }
    SimResult {
        delivered,
        retransmits,
        converged: false,
        pending_left: tx.pending.len(),
        buffered_left: rx.buffered_len(),
    }
}

/// Timer-driven run (Option B). The clock advances in 1 ms steps and is the only driver: after
/// `send_window_ms` no `send` call ever happens again, and every message's first two
/// transmissions are dropped deterministically, so a message can only reach the receiver through
/// an RTO retransmission produced by the sender's timer. This run deliberately has no application
/// return traffic, so every acknowledgement is a forced standalone ack produced by the receiver's
/// timer. Loss/duplication/reordering for everything past the first two attempts still come from
/// the shared link model (`schedule`).
struct TimerSim {
    delivered: Vec<Vec<u8>>,
    retransmits: u32,
    retransmits_after_last_send: u32,
    forced_acks: u32,
    forced_acks_after_last_send: u32,
    send_calls: u32,
    last_send_ms: u64,
    converged: bool,
    pending_left: usize,
    buffered_left: usize,
}

/// One transmission attempt for `simulate_timer_driven`: the first two attempts of each sequence
/// are lost, then the shared link model decides duplication and delay (reordering).
fn transmit_timer(
    map: &mut BTreeMap<u64, Vec<(u32, Vec<u8>)>>,
    attempts: &mut BTreeMap<u32, u32>,
    rng: &mut Lcg,
    now: u64,
    seq: u32,
    pay: Vec<u8>,
    jitter_ms: u64,
    dup_mod: u64,
) {
    let a = attempts.entry(seq).or_insert(0);
    *a += 1;
    if *a <= 2 {
        return; // lost
    }
    schedule(map, rng, now, seq, pay, 0, dup_mod, jitter_ms);
}

fn simulate_timer_driven(
    seed: u64,
    n: u32,
    options: u32,
    send_window_ms: u64,
    jitter_ms: u64,
    dup_mod: u64,
) -> TimerSim {
    let mut rng = Lcg(seed);
    let mut tx = Sender::new();
    let mut rx = Receiver::new();
    let mut a2b: BTreeMap<u64, Vec<(u32, Vec<u8>)>> = BTreeMap::new();
    let mut b2a: BTreeMap<u64, Vec<u32>> = BTreeMap::new();
    let mut attempts: BTreeMap<u32, u32> = BTreeMap::new();
    let mut delivered: Vec<Vec<u8>> = Vec::new();
    let mut retransmits = 0u32;
    let mut retx_times: Vec<u64> = Vec::new();
    let mut forced_acks = 0u32;
    let mut ack_times: Vec<u64> = Vec::new();
    let mut send_calls = 0u32;
    let mut last_send_ms = 0u64;
    let mut next_msg = 0u32;
    let mut now = 0u64;

    while now < 120_000 {
        // All sends happen inside the window; afterwards the outbox is empty for good.
        if now <= send_window_ms && next_msg < n && now % 7 == 0 {
            let out = tx.send(payload(next_msg), options, now);
            send_calls += 1;
            last_send_ms = now;
            transmit_timer(
                &mut a2b,
                &mut attempts,
                &mut rng,
                now,
                out.seq,
                out.payload,
                jitter_ms,
                dup_mod,
            );
            next_msg += 1;
        }
        // The timer, not a send call: these run on every clock step.
        for r in tx.due(now) {
            if r.gave_up {
                continue;
            }
            retransmits += 1;
            retx_times.push(now);
            transmit_timer(
                &mut a2b,
                &mut attempts,
                &mut rng,
                now,
                r.seq,
                r.payload,
                jitter_ms,
                dup_mod,
            );
        }
        while let Some((&t, _)) = a2b.first_key_value() {
            if t > now {
                break;
            }
            for (seq, pay) in a2b.remove(&t).unwrap() {
                if let RecvOutcome::Delivered(msgs) = rx.on_message(seq, options, pay, t) {
                    for m in msgs {
                        delivered.push(m.payload);
                    }
                }
            }
        }
        // No application return traffic at all in this run: every ack is forced by the timer.
        if let Some(ack) = rx.forced_ack(now) {
            forced_acks += 1;
            ack_times.push(now);
            let jitter = if jitter_ms == 0 { 0 } else { rng.next() % (jitter_ms + 1) };
            b2a.entry(now + jitter).or_default().push(ack);
        }
        while let Some((&t, _)) = b2a.first_key_value() {
            if t > now {
                break;
            }
            for ack in b2a.remove(&t).unwrap() {
                tx.on_ack(ack);
            }
        }
        if next_msg == n
            && a2b.is_empty()
            && b2a.is_empty()
            && tx.pending.is_empty()
            && rx.buffered_len() == 0
        {
            return TimerSim {
                delivered,
                retransmits,
                retransmits_after_last_send: retx_times.iter().filter(|t| **t > last_send_ms).count()
                    as u32,
                forced_acks,
                forced_acks_after_last_send: ack_times
                    .iter()
                    .filter(|t| **t > last_send_ms)
                    .count() as u32,
                send_calls,
                last_send_ms,
                converged: true,
                pending_left: 0,
                buffered_left: 0,
            };
        }
        now += 1;
    }
    TimerSim {
        delivered,
        retransmits,
        retransmits_after_last_send: retx_times.iter().filter(|t| **t > last_send_ms).count() as u32,
        forced_acks,
        forced_acks_after_last_send: ack_times
            .iter()
            .filter(|t| **t > last_send_ms)
            .count() as u32,
        send_calls,
        last_send_ms,
        converged: false,
        pending_left: tx.pending.len(),
        buffered_left: rx.buffered_len(),
    }
}

fn main() {
    println!(
        "reliable_test: PARTY_RELIABLE={PARTY_RELIABLE} rto={RTO_FIRST_MS}..{RTO_MAX_MS}ms retries={MAX_RETRIES} ack_delay={ACK_DELAY_MS}ms pending_cap={PENDING_CAP} ooo_cap={OOO_CAP}"
    );

    // 1. Guaranteed + Sequential over a link that drops, duplicates and reorders.
    let r = simulate(0x1234_5678, 200, SEND_GUARANTEED | SEND_SEQUENTIAL, 4, 5, 17);
    let expected: Vec<Vec<u8>> = (0..200).map(payload).collect();
    check(
        "guar-seq converged",
        r.converged,
        &format!(
            "delivered={} retx={} pending={} buffered={}",
            r.delivered.len(),
            r.retransmits,
            r.pending_left,
            r.buffered_left
        ),
    );
    check(
        "guar-seq every message exactly once, in order",
        r.delivered == expected,
        &format!("delivered={} expected={}", r.delivered.len(), expected.len()),
    );
    check(
        "guar-seq retransmission happened",
        r.retransmits > 0,
        &format!("retx={}", r.retransmits),
    );
    check(
        "guar-seq idle link converged to zero pending",
        r.pending_left == 0,
        &format!("pending={}", r.pending_left),
    );
    check(
        "guar-seq out-of-order buffer drained",
        r.buffered_left == 0,
        &format!("buffered={}", r.buffered_left),
    );

    // 2. Guaranteed + Nonsequential: exactly once, ordering not required.
    let r2 = simulate(0xABCD_1234, 200, SEND_GUARANTEED, 3, 6, 23);
    let mut got = r2.delivered.clone();
    got.sort();
    let mut exp = expected.clone();
    exp.sort();
    check(
        "guar-nonseq converged",
        r2.converged,
        &format!(
            "delivered={} retx={} pending={}",
            r2.delivered.len(),
            r2.retransmits,
            r2.pending_left
        ),
    );
    check(
        "guar-nonseq exactly once (order free)",
        got == exp,
        &format!("delivered={} expected={}", got.len(), exp.len()),
    );
    check(
        "guar-nonseq retransmission happened",
        r2.retransmits > 0,
        &format!("retx={}", r2.retransmits),
    );

    // 3. Sequential + BestEffort: older arrivals dropped, never buffered.
    {
        let mut rx = Receiver::new();
        let o1 = rx.on_message(5, SEND_SEQUENTIAL, payload(5), 0);
        let o2 = rx.on_message(3, SEND_SEQUENTIAL, payload(3), 1);
        let o3 = rx.on_message(7, SEND_SEQUENTIAL, payload(7), 2);
        let o4 = rx.on_message(6, SEND_SEQUENTIAL, payload(6), 3);
        check(
            "be-seq first message delivered",
            matches!(o1, RecvOutcome::Delivered(_)),
            "",
        );
        check(
            "be-seq older message dropped (not buffered)",
            matches!(o2, RecvOutcome::DroppedOutOfOrder { seq: 3, high: 5 })
                && rx.buffered_len() == 0,
            &format!("buffered={}", rx.buffered_len()),
        );
        check(
            "be-seq newer-after-gap delivered (high-water moves forward)",
            matches!(o3, RecvOutcome::Delivered(_)),
            "",
        );
        check(
            "be-seq later older message dropped",
            matches!(o4, RecvOutcome::DroppedOutOfOrder { seq: 6, high: 7 }),
            "",
        );
        check(
            "be-seq nothing ever buffered",
            rx.buffered_len() == 0,
            &format!("buffered={}", rx.buffered_len()),
        );
    }

    // 4. RTO schedule, first-retransmit flag and retry budget.
    {
        let mut tx = Sender::new();
        let out = tx.send(payload(1), SEND_GUARANTEED, 1000);
        let before = tx.due(1059);
        let first = tx.due(1060);
        check(
            "rto no retransmit before 60ms",
            before.is_empty(),
            &format!("due={}", before.len()),
        );
        check(
            "rto first retransmit at 60ms",
            first.len() == 1
                && first[0].first
                && first[0].retries == 1
                && first[0].seq == out.seq
                && !first[0].gave_up,
            &format!("retx={}", first.len()),
        );
        let at1179 = tx.due(1179);
        let at1180 = tx.due(1180);
        check(
            "rto doubles to 120ms",
            at1179.is_empty() && at1180.len() == 1 && !at1180[0].first && at1180[0].retries == 2,
            &format!("early={} late={}", at1179.len(), at1180.len()),
        );
        let mut t = 1180u64;
        let mut gave_up = 0;
        for _ in 0..20 {
            for r in tx.due(t) {
                if r.gave_up {
                    gave_up += 1;
                }
            }
            t += 1000;
        }
        check(
            "rto bounded retries, pending kept until ack/evict",
            gave_up == 1 && tx.pending.len() == 1,
            &format!("gave_up={} pending={}", gave_up, tx.pending.len()),
        );
    }

    // 5. Cumulative ack clears the whole prefix.
    {
        let mut tx = Sender::new();
        for i in 1..=5 {
            tx.send(payload(i), SEND_GUARANTEED | SEND_SEQUENTIAL, 0);
        }
        let cleared = tx.on_ack(3);
        check(
            "ack cumulative clears seq<=3",
            cleared.len() == 3 && tx.pending.len() == 2,
            &format!("cleared={} pending={}", cleared.len(), tx.pending.len()),
        );
    }

    // 6. Forced standalone ack timing and lazy-ack opt-out.
    {
        let mut rx = Receiver::new();
        let _ = rx.on_message(1, SEND_GUARANTEED, payload(1), 1000);
        check(
            "ack not forced before 30ms",
            rx.forced_ack(1029).is_none(),
            "",
        );
        let forced = rx.forced_ack(1030);
        check(
            "ack forced at 30ms when no return traffic",
            forced == Some(1),
            &format!("{forced:?}"),
        );
        let mut lazy = Receiver::new();
        let _ = lazy.on_message(
            1,
            SEND_GUARANTEED | SEND_ALLOW_LAZY_ACKNOWLEDGEMENT,
            payload(1),
            2000,
        );
        check(
            "ack lazy suppresses forced ack",
            lazy.forced_ack(2100).is_none(),
            "",
        );
    }

    // 7. Pending map bound: oldest evicted with the eviction reported.
    {
        let mut tx = Sender::new();
        let mut evicted = None;
        for i in 0..(PENDING_CAP as u32 + 1) {
            let out = tx.send(payload(i), SEND_GUARANTEED, i as u64);
            if out.evicted.is_some() {
                evicted = out.evicted;
            }
        }
        check(
            "pending cap evicts oldest",
            evicted == Some((1, SEND_GUARANTEED)) && tx.pending.len() == PENDING_CAP,
            &format!("evicted={:?} pending={}", evicted, tx.pending.len()),
        );
    }

    // 8. Out-of-order buffer bound.
    {
        let mut rx = Receiver::new();
        let mut evicted = None;
        for i in 2..(OOO_CAP as u32 + 5) {
            rx.on_message(
                i,
                SEND_GUARANTEED | SEND_SEQUENTIAL,
                payload(i),
                0,
            );
            if rx.last_evicted.is_some() {
                evicted = rx.last_evicted;
            }
        }
        check(
            "out-of-order buffer bounded",
            rx.buffered_len() <= OOO_CAP && evicted.is_some(),
            &format!("buffered={} evicted={:?}", rx.buffered_len(), evicted),
        );
        // A stale eviction must not be re-reported by a later unsequenced message.
        let _ = rx.on_message(0, 0, payload(0), 0);
        check(
            "eviction flag does not leak across calls",
            rx.last_evicted.is_none(),
            &format!("last_evicted={:?}", rx.last_evicted),
        );
    }

    // 9. Options decoding the next run should log at the send site.
    println!("decode(0x00) = {}", decode_options(0x00));
    println!("decode(0x01) = {}", decode_options(0x01));
    println!("decode(0x02) = {}", decode_options(0x02));
    println!("decode(0x03) = {}", decode_options(0x03));
    println!("decode(0x13) = {}", decode_options(0x13));

    // 10. Timer deadlines (Option B): the transport thread wakes on these, not on game calls.
    //     `send` is called exactly once in this block; everything after is the clock alone.
    {
        let mut tx = Sender::new();
        check(
            "timer no RTO deadline while nothing is pending",
            tx.next_rto_ms().is_none(),
            &format!("{:?}", tx.next_rto_ms()),
        );
        let out = tx.send(payload(1), SEND_GUARANTEED | SEND_SEQUENTIAL, 0);
        check(
            "timer RTO deadline is first-send + RTO",
            tx.next_rto_ms() == Some(RTO_FIRST_MS),
            &format!("{:?}", tx.next_rto_ms()),
        );
        let mut clock = 0u64;
        let mut retx: Vec<(u64, u32)> = Vec::new();
        for _ in 0..3 {
            clock = tx.next_rto_ms().expect("pending message has an RTO deadline");
            for r in tx.due(clock) {
                if !r.gave_up {
                    retx.push((clock, r.retries));
                }
            }
        }
        check(
            "timer retransmits at 60/180/420ms with ZERO sends in between",
            retx == vec![(60, 1), (180, 2), (420, 3)],
            &format!("retx={retx:?}"),
        );
        check(
            "timer next deadline tracks the doubling RTO",
            tx.next_rto_ms() == Some(420 + 480),
            &format!("{:?}", tx.next_rto_ms()),
        );
        // Deliver the message and return the ack; still no further send call.
        let mut rx = Receiver::new();
        let delivered = rx.on_message(
            out.seq,
            SEND_GUARANTEED | SEND_SEQUENTIAL,
            out.payload.clone(),
            clock,
        );
        check(
            "timer-driven receiver delivered the message",
            matches!(delivered, RecvOutcome::Delivered(ref m) if m.len() == 1 && m[0].payload == out.payload),
            "",
        );
        tx.on_ack(rx.piggyback_ack());
        check(
            "timer ack clears the RTO deadline",
            tx.next_rto_ms().is_none(),
            &format!("{:?}", tx.next_rto_ms()),
        );
    }

    // 11. Forced standalone ack on the timer alone: no return traffic, no sends.
    {
        let mut rx = Receiver::new();
        let _ = rx.on_message(1, SEND_GUARANTEED | SEND_SEQUENTIAL, payload(1), 0);
        check(
            "timer ack deadline is arrival + ACK_DELAY_MS",
            rx.next_ack_due_ms() == Some(ACK_DELAY_MS),
            &format!("{:?}", rx.next_ack_due_ms()),
        );
        check(
            "timer no ack before its deadline",
            rx.forced_ack(ACK_DELAY_MS - 1).is_none(),
            "",
        );
        let forced = rx.forced_ack(ACK_DELAY_MS);
        check(
            "timer forced ack at its deadline with ZERO sends",
            forced == Some(1),
            &format!("{forced:?}"),
        );
        check(
            "timer ack deadline cleared after forcing",
            rx.next_ack_due_ms().is_none(),
            "",
        );
        let mut lazy = Receiver::new();
        let _ = lazy.on_message(
            1,
            SEND_GUARANTEED | SEND_ALLOW_LAZY_ACKNOWLEDGEMENT,
            payload(1),
            0,
        );
        check(
            "timer lazy ack has no forced deadline",
            lazy.next_ack_due_ms().is_none(),
            "",
        );
    }

    // 12. Full timer-driven run over a lossy/duplicating/reordering link. Every message loses its
    //     first two transmissions, so delivery requires RTO retransmission; the send window closes
    //     at 90 ms while the retransmits land at +60/+180 ms, and every ack is forced.
    for (name, options) in [
        ("guar-seq", SEND_GUARANTEED | SEND_SEQUENTIAL),
        ("guar-nonseq", SEND_GUARANTEED),
    ] {
        let ts = simulate_timer_driven(0x0BADC0DE, 12, options, 90, 13, 4);
        check(
            &format!("timer-driven {name} converged with the sender idle"),
            ts.converged,
            &format!(
                "delivered={} send_calls={} last_send={}ms pending={} buffered={}",
                ts.delivered.len(),
                ts.send_calls,
                ts.last_send_ms,
                ts.pending_left,
                ts.buffered_left
            ),
        );
        check(
            &format!("timer-driven {name} every message exactly once, in order"),
            if options & SEND_SEQUENTIAL != 0 {
                ts.delivered == (0..12).map(payload).collect::<Vec<_>>()
            } else {
                let mut got = ts.delivered.clone();
                got.sort();
                let mut exp: Vec<Vec<u8>> = (0..12).map(payload).collect();
                exp.sort();
                got == exp
            },
            &format!("delivered={}", ts.delivered.len()),
        );
        check(
            &format!("timer-driven {name} retransmit with ZERO sends between RTO expiries"),
            ts.send_calls == 12 && ts.retransmits_after_last_send > 0,
            &format!(
                "send_calls={} window=90ms final_send={}ms retx={} retx_after_final_send={}",
                ts.send_calls, ts.last_send_ms, ts.retransmits, ts.retransmits_after_last_send
            ),
        );
        check(
            &format!("timer-driven {name} forced ack with no return traffic and no sends"),
            ts.forced_acks > 0 && ts.forced_acks_after_last_send > 0,
            &format!(
                "forced_acks={} forced_acks_after_final_send={}",
                ts.forced_acks, ts.forced_acks_after_last_send
            ),
        );
    }

    let failures = FAILURES.load(Ordering::SeqCst);
    if failures > 0 {
        println!("{failures} FAILURE(S)");
        std::process::exit(1);
    }
    println!("all reliable-delivery checks passed");
}

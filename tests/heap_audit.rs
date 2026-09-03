// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Property-Based GC Cascade Idempotence Audit (F-01 / T-3.1.3).
//!
//! Sprint 3.1 — Blindagem Formal do GC. This integration target certifies the
//! **exact GC cascade** that NAM-Plug's swap paths consume
//! (`NeuralAmpModeler-rs::common::spsc::{gc_cascade, GcOverflowBuffer,
//! drain_gc_channels}`): the SPSC channel (32 slots) → the 16-slot RT parking
//! lot → the 64-slot overflow buffer, drained off-RT.
//!
//! Every simulated "swap" (model/IR-style resource retirement) is allocated
//! with a unique monotonic `generation` identifier carried inside a real
//! `GcItem` envelope (`CabSimSwap` for IR-style bypass swaps, `ResamplerSwap`
//! carrying a `NamResampler` + `StreamingResampleBuffer` for model-style
//! swaps). Concurrent asynchronous requester threads race an off-RT drainer
//! thread, and every drained item has its identifier recorded before the drop.
//!
//! Formal invariants asserted per property case:
//!   1. `Total(drenados) == Total(alocados)` — every allocated item is
//!      recollected exactly once (nothing lost, nothing leaked).
//!   2. Drained identifiers are strictly disjoint — `HashSet` has no
//!      duplicates, hence zero double-drain / double-free of the same item.
//!   3. `RT_STATUS_GC_OVERFLOW` (slot overwrite / controlled leak) never fires
//!      — under the no-overwrite regime no allocated item can escape the
//!      cascade.
//!   4. `RT_STATUS_GC_TIER3` fires — the case genuinely exercised the overflow
//!      tier (a case that never leaves the SPSC proves nothing).
//!
//! All work happens on off-RT test threads: there is no audio thread here, so
//! the rollback condition of T-3.1.3 ("no dynamic allocation on the RT
//! thread") is vacuous by construction.

use neural_amp_modeler_rs::common::spsc::{
    CabSimSwapPayload, GcItem, GcOverflowBuffer, RT_STATUS_GC_OVERFLOW, RT_STATUS_GC_TIER3,
    ResamplerSwapPayload, RtStatusFlags, gc_cascade,
};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use proptest::prelude::*;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// GC SPSC channel capacity, identical to the production processor channel.
const GC_SPSC_CAPACITY: usize = 32;
/// GC overflow buffer capacity, identical to the production default.
const GC_OVERFLOW_CAPACITY: usize = 64;
/// RT parking-lot slot count, identical to the production `[Option<GcItem>; 16]`.
const PARKING_LOT_SLOTS: usize = 16;
/// Equal-rate construction keeps the resampler/stream probes allocation-light.
const HOST_RATE: u32 = 48_000;
const MODEL_RATE: u32 = 48_000;
const MAX_BLOCK: usize = 64;

type ParkingLot = [Option<GcItem>; PARKING_LOT_SLOTS];

/// Builds one unique swap item carrying `id` as its observable generation.
///
/// Even ids model IR-style retirements (`CabSimSwap`, bypass `pair: None`);
/// odd ids model model-style retirements (`ResamplerSwap` carrying a live
/// `NamResampler` + `StreamingResampleBuffer` so the drain also exercises real
/// nested heap deallocation). The cascade itself is variant-agnostic: any
/// boxed resource travels the same SPSC → lot → overflow tiers.
fn make_swap_item(id: u64) -> GcItem {
    if id.is_multiple_of(2) {
        GcItem::CabSimSwap(Box::new(CabSimSwapPayload {
            generation: id,
            pair: None,
        }))
    } else {
        GcItem::ResamplerSwap(Box::new(ResamplerSwapPayload {
            generation: id,
            resampler: Box::new(
                NamResampler::new(HOST_RATE, MODEL_RATE, 0).expect("equal-rate resampler builds"),
            ),
            stream: Box::new(
                StreamingResampleBuffer::new(HOST_RATE, MODEL_RATE, MAX_BLOCK)
                    .expect("equal-rate streaming adapter builds"),
            ),
        }))
    }
}

/// Guard rail for the soak above: exceeding the cascade's 112-item in-flight
/// capacity (SPSC 32 + parking lot 16 + overflow 64) must set
/// `RT_STATUS_GC_OVERFLOW` — the controlled leak is *flagged*, never silent.
///
/// Pushing 113 items with no drainer overwrites exactly one overflow slot:
/// 113 allocated → 112 drained + 1 documented leak, with the telemetry flag
/// latched. This proves the exactly-once property asserted by the property
/// soak is only ever voided by the engine's explicit, observable leak path —
/// never by a silent duplicate or loss.
#[test]
fn gc_overflow_beyond_capacity_is_flagged_not_silent() {
    let (mut tx, mut rx) = rtrb::RingBuffer::<GcItem>::new(GC_SPSC_CAPACITY);
    let mut lot: ParkingLot = ParkingLot::default();
    let overflow = GcOverflowBuffer::new(GC_OVERFLOW_CAPACITY);
    let rt_status = RtStatusFlags::new();

    for id in 0..113u64 {
        let item = make_swap_item(id);
        gc_cascade(Some(item), &mut tx, &mut lot, &overflow, &rt_status);
    }

    assert!(
        rt_status.check_flag(RT_STATUS_GC_TIER3),
        "113 items must reach the overflow tier"
    );
    assert!(
        rt_status.check_flag(RT_STATUS_GC_OVERFLOW),
        "the 113th item must overwrite an overflow slot and latch the flag"
    );

    // Recover everything that is still owned: 32 (SPSC) + 64 (overflow
    // survivors) + 16 (parking lot) = 112. The overwritten item is the
    // single documented leak.
    let mut drained = 0usize;
    while let Ok(item) = rx.pop() {
        let _ = swap_item_id(&item);
        drop(item);
        drained += 1;
    }
    for item in overflow.drain(&rt_status) {
        let _ = swap_item_id(&item);
        drop(item);
        drained += 1;
    }
    for slot in lot.iter_mut() {
        if let Some(item) = slot.take() {
            let _ = swap_item_id(&item);
            drop(item);
            drained += 1;
        }
    }
    assert_eq!(
        drained, 112,
        "113 allocated → 112 recoverable + 1 flagged leak"
    );
}

/// Recovers the unique identifier carried by a drained item.
///
/// Every item pushed by [`make_swap_item`] is one of the two tracked variants,
/// so any other variant reaching the drainer is a programming error in the
/// probe itself.
fn swap_item_id(item: &GcItem) -> u64 {
    match item {
        GcItem::CabSimSwap(payload) => payload.generation,
        GcItem::ResamplerSwap(payload) => payload.generation,
        other => panic!("unexpected untracked GcItem in idempotence drain: {other:?}"),
    }
}

/// One observable drain pass, mirroring the canonical `drain_gc_channels`
/// order (SPSC first, then overflow; the parking lot is single-owner and only
/// drained at teardown). Returns the identifiers of the drained items so the
/// caller can record them under a shared ledger.
fn drain_pass(
    consumer: &mut rtrb::Consumer<GcItem>,
    overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) -> Vec<u64> {
    let mut ids = Vec::new();
    while let Ok(item) = consumer.pop() {
        ids.push(swap_item_id(&item));
        drop(item);
    }
    for item in overflow.drain(rt_status) {
        ids.push(swap_item_id(&item));
        drop(item);
    }
    ids
}

/// Runs one property case of the concurrent swap + drain soak.
///
/// Phase A deterministically prefills the cascade with `prefill` items
/// (49..=110) on this thread — SPSC (32) + parking lot (16) fill up and the
/// remainder (1..=62) enters the overflow buffer, guaranteeing the Tier-3
/// packing path is exercised without any slot overwrite.
///
/// Phase B spawns `producer_threads` asynchronous requester threads that race
/// an off-RT drainer thread over the remaining `total_swaps - prefill` items.
/// Drain progress is deterministic, never scheduler-luck: the SPSC consumer
/// and the drain ledger sit behind mutexes, and every requester performs one
/// bounded drain immediately after each cascade push — mirroring the
/// production cadence where main-thread housekeeping drains between audio
/// cycles — while the dedicated drainer thread drains concurrently whenever
/// the OS schedules it. Drain progress is therefore guaranteed by *either*
/// party, and the in-flight population never leaves the cascade's 112-item
/// capacity (SPSC 32 + parking lot 16 + overflow 64), the regime in which the
/// exactly-once invariant is defined. (An unbounded burst beyond that capacity
/// is the engine's *documented controlled leak* — see `GcOverflowBuffer` —
/// latched via `RT_STATUS_GC_OVERFLOW`, never a drop on the audio thread.)
///
/// After every producer joins, the drainer stops and a final exhaustive drain
/// (SPSC + overflow + parking-lot handoff) recovers any residue. All drained
/// identifiers are then checked against the exact allocated set.
fn run_idempotence_case(total_swaps: usize, producer_threads: usize, prefill: usize) {
    assert!(
        prefill >= 49 && prefill + producer_threads <= 112 && prefill < total_swaps,
        "prefill must stay within the no-overwrite window and below the total \
         (prefill={prefill}, producers={producer_threads}, total={total_swaps})"
    );

    let (tx, rx) = rtrb::RingBuffer::<GcItem>::new(GC_SPSC_CAPACITY);
    let producer_side = Arc::new(Mutex::new((tx, ParkingLot::default())));
    // SPSC consumer is shared between the drainer thread and the requesters'
    // bounded per-push drains, serialized by this mutex.
    let drain_side = Arc::new(Mutex::new(rx));
    let overflow = Arc::new(GcOverflowBuffer::new(GC_OVERFLOW_CAPACITY));
    let rt_status = Arc::new(RtStatusFlags::new());
    let next_id = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    // Shared drain ledger (off-RT): every drained identifier appended exactly
    // once, in whatever thread performed the drain.
    let recorded = Arc::new(Mutex::new(Vec::<u64>::new()));

    // Phase A — deterministic prefill across all three cascade tiers.
    {
        let mut guard = producer_side.lock().expect("producer lock");
        let (producer, lot) = &mut *guard;
        for id in 0..prefill as u64 {
            let item = make_swap_item(id);
            gc_cascade(Some(item), producer, lot, &overflow, &rt_status);
        }
    }
    next_id.store(prefill as u64, Ordering::Relaxed);
    assert!(
        rt_status.check_flag(RT_STATUS_GC_TIER3),
        "prefill (≥ 49 items) must reach the overflow tier"
    );
    assert!(
        !rt_status.check_flag(RT_STATUS_GC_OVERFLOW),
        "phase-A prefill must never overwrite an overflow slot"
    );

    // Off-RT drainer thread: races the requesters for as long as they push.
    let done_cons = Arc::clone(&done);
    let drain_side_cons = Arc::clone(&drain_side);
    let overflow_cons = Arc::clone(&overflow);
    let rt_status_cons = Arc::clone(&rt_status);
    let recorded_cons = Arc::clone(&recorded);
    let drainer = std::thread::spawn(move || {
        while !done_cons.load(Ordering::Relaxed) {
            let ids = {
                let mut consumer = drain_side_cons.lock().expect("drain lock");
                drain_pass(&mut consumer, &overflow_cons, &rt_status_cons)
            };
            recorded_cons.lock().expect("ledger lock").extend(ids);
        }
    });

    // Phase B — concurrent asynchronous swap requesters.
    let mut handles = Vec::with_capacity(producer_threads);
    for _ in 0..producer_threads {
        let producer_side = Arc::clone(&producer_side);
        let drain_side_p = Arc::clone(&drain_side);
        let overflow_p = Arc::clone(&overflow);
        let rt_status_p = Arc::clone(&rt_status);
        let recorded_p = Arc::clone(&recorded);
        let next_id_p = Arc::clone(&next_id);
        handles.push(std::thread::spawn(move || {
            let total = total_swaps as u64;
            loop {
                let id = next_id_p.fetch_add(1, Ordering::Relaxed);
                if id >= total {
                    break;
                }
                let item = make_swap_item(id);
                {
                    let mut guard = producer_side.lock().expect("producer lock");
                    let (producer, lot) = &mut *guard;
                    gc_cascade(Some(item), producer, lot, &overflow_p, &rt_status_p);
                }
                // Bounded cooperative drain (housekeeping between cycles).
                let ids = {
                    let mut consumer = drain_side_p.lock().expect("drain lock");
                    drain_pass(&mut consumer, &overflow_p, &rt_status_p)
                };
                recorded_p.lock().expect("ledger lock").extend(ids);
            }
        }));
    }
    for handle in handles {
        handle.join().expect("requester thread must not panic");
    }

    // Stop the drainer.
    done.store(true, Ordering::Release);
    drainer.join().expect("drainer thread must not panic");

    // Final exhaustive drain: SPSC + overflow + the single-owner parking-lot
    // handoff (the production teardown contract of R-04).
    {
        let mut consumer = drain_side.lock().expect("drain lock");
        let ids = drain_pass(&mut consumer, &overflow, &rt_status);
        recorded.lock().expect("ledger lock").extend(ids);
    }
    {
        let mut guard = producer_side.lock().expect("producer lock");
        let mut ledger = recorded.lock().expect("ledger lock");
        for slot in guard.1.iter_mut() {
            if let Some(item) = slot.take() {
                let id = swap_item_id(&item);
                ledger.push(id);
                drop(item);
            }
        }
    }
    // Second canonical pass: closes any benign race window (see drain_gc_final).
    {
        let mut consumer = drain_side.lock().expect("drain lock");
        let ids = drain_pass(&mut consumer, &overflow, &rt_status);
        recorded.lock().expect("ledger lock").extend(ids);
    }

    // ── Formal invariants (F-01 acceptance: Total(Alocados) == Total(Drenados)
    // ── with strictly disjoint identifiers) ──────────────────────────────────
    let ledger = recorded.lock().expect("ledger lock");
    assert!(
        !rt_status.check_flag(RT_STATUS_GC_OVERFLOW),
        "GC slot overwrite occurred — an allocated item was leaked and the \
         exactly-once invariant cannot hold \
         (total={total_swaps}, producers={producer_threads}, prefill={prefill}, \
         drained={})",
        ledger.len(),
    );
    assert_eq!(
        ledger.len(),
        total_swaps,
        "every allocated item must be drained exactly once \
         (recorded={}, allocated={total_swaps})",
        ledger.len(),
    );
    let seen: HashSet<u64> = ledger.iter().copied().collect();
    assert_eq!(
        seen.len(),
        total_swaps,
        "duplicate drain detected: an identifier was collected more than once"
    );
    let expected: HashSet<u64> = (0..total_swaps as u64).collect();
    assert_eq!(
        seen, expected,
        "the drained identifier set must equal the allocated identifier set \
         (zero losses, zero duplicates, zero foreign ids)"
    );
    {
        let guard = producer_side.lock().expect("producer lock");
        assert!(
            guard.1.iter().all(Option::is_none),
            "the parking lot must be completely empty after the teardown handoff"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// F-01 / T-3.1.3: 100–500 concurrent model/IR-style swaps, raced between
    /// asynchronous requester threads and an off-RT drainer, must preserve
    /// strict GC idempotence — every unique identifier drained exactly once.
    #[test]
    #[ignore = "property soak: 500 cases of 100-500 concurrent GC swaps"]
    fn prop_gc_swap_idempotence(
        total_swaps in 100usize..=500,
        producer_threads in 2usize..=8,
        prefill in 49usize..=111,
    ) {
        // The initial spike is bounded by prefill + one in-flight item per
        // requester: cap the prefill so the cascade's 112-item capacity is
        // never exceeded even if every requester pushes before the drainer
        // gets a pass.
        let prefill = prefill
            .min(total_swaps.saturating_sub(1))
            .min(112 - producer_threads);
        prop_assert!(prefill >= 49, "case needs ≥ 49 prefilled items to reach Tier 3");
        run_idempotence_case(total_swaps, producer_threads, prefill);
    }
}

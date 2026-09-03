// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::clap::plugin::ClapParamPayload;
use crate::clap::plugin::RestoreTxn;
use crate::clap::plugin::StructuralKind;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::thread;

fn make_test_scheduler() -> (CommandScheduler, Arc<AtomicU64>, Arc<AtomicU64>) {
    let sched = CommandScheduler::new();
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));
    (sched, next_seq, last_ack)
}

/// Builds a complete atomic restore transaction. `mult_adj` differentiates the
/// model component between restores so snapshots can be told apart.
fn make_restore_txn(generation: u64, gain: f32, mult_adj: f32) -> RestoreTxn {
    RestoreTxn {
        generation,
        model: Some(crate::clap::plugin::LoadModelPayload {
            generation,
            model_l: None,
            new_resampler: Box::new(NamResampler::new(48000, 48000, 0).unwrap()),
            new_stream: crate::clap::plugin::build_stream_adapter(48000, 48000, 64).unwrap(),
            input_mult_adj: mult_adj,
            output_mult_adj: mult_adj,
        }),
        ir: Some(None),
        params: RtProcessingParams {
            input_gain_db: gain,
            ..Default::default()
        },
    }
}

#[test]
fn coalesce_single_param_and_flush() {
    let (_sched, next_seq, last_ack) = make_test_scheduler();
    let (tx, _rx) = rtrb::RingBuffer::new(256);
    let mut producer = CommandProducer::new(tx, &next_seq, &last_ack);

    let params = RtProcessingParams {
        input_gain_db: 5.0,
        bypass: false,
        ..Default::default()
    };

    let is_new = producer.push_params(params);
    assert!(is_new, "first push should start a new batch");

    let seq = producer.force_flush().unwrap();
    assert!(seq > 0, "flush should get a sequence number");
}

#[test]
fn coalesce_merges_consecutive_param_updates() {
    let (_sched, next_seq, last_ack) = make_test_scheduler();
    let (tx, mut rx) = rtrb::RingBuffer::new(256);
    let mut producer = CommandProducer::new(tx, &next_seq, &last_ack);

    for gain in 0..100 {
        let p = RtProcessingParams {
            input_gain_db: gain as f32,
            ..Default::default()
        };
        let is_new = producer.push_params(p);
        // First is new, rest are coalesced
        if gain == 0 {
            assert!(is_new);
        } else {
            assert!(!is_new);
        }
    }

    let seq = producer.force_flush().unwrap();
    assert!(seq > 0, "flush should get a sequence number");

    let mut found = false;
    while let Ok(payload) = rx.pop() {
        if let ClapParamPayload::Params(p) = payload {
            assert_eq!(p.input_gain_db, 99.0, "should keep only the latest value");
            found = true;
        }
    }
    assert!(found, "should have received the coalesced params");
}

#[test]
fn coalesce_preserves_multi_param_merging() {
    let (_sched, next_seq, last_ack) = make_test_scheduler();
    let (tx, mut rx) = rtrb::RingBuffer::new(256);
    let mut producer = CommandProducer::new(tx, &next_seq, &last_ack);

    let p1 = RtProcessingParams {
        input_gain_db: 3.0,
        bypass: true,
        ..Default::default()
    };
    assert!(producer.push_params(p1));

    let p2 = RtProcessingParams {
        input_gain_db: 3.0,
        bypass: true,
        output_gain_db: -6.0,
        gate_threshold_db: -50.0,
        ..Default::default()
    };
    assert!(!producer.push_params(p2));

    producer.force_flush().unwrap();

    let mut final_params: Option<RtProcessingParams> = None;
    while let Ok(payload) = rx.pop() {
        if let ClapParamPayload::Params(p) = payload {
            final_params = Some(p);
        }
    }
    let fp = final_params.expect("should receive coalesced params");
    assert_eq!(fp.input_gain_db, 3.0);
    assert_eq!(fp.output_gain_db, -6.0);
    assert_eq!(fp.gate_threshold_db, -50.0);
    assert!(fp.bypass);
}

#[test]
fn non_coalescable_flushes_pending_params_first() {
    let (_sched, next_seq, last_ack) = make_test_scheduler();
    let (tx, mut rx) = rtrb::RingBuffer::new(256);
    let mut producer = CommandProducer::new(tx, &next_seq, &last_ack);

    let p = RtProcessingParams {
        input_gain_db: 12.0,
        ..Default::default()
    };
    assert!(producer.push_params(p));

    let seq = producer
        .push_command(ClapParamPayload::LoadCabIr { adapter: None })
        .unwrap();
    assert!(seq > 0, "command should get a sequence number");

    let expected_order = vec!["Params", "LoadCabIr"];
    let mut actual_order = Vec::new();
    while let Ok(payload) = rx.pop() {
        actual_order.push(match payload {
            ClapParamPayload::Params(_) => "Params",
            ClapParamPayload::LoadCabIr { .. } => "LoadCabIr",
            _ => "Other",
        });
    }
    assert_eq!(
        actual_order, expected_order,
        "params must be flushed before the non-coalescable command"
    );
}

#[test]
fn ack_tracking_basic() {
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));
    let (tx, rx) = rtrb::RingBuffer::new(256);
    let mut producer = CommandProducer::new(tx, &next_seq, &last_ack);

    let mut p = RtProcessingParams {
        input_gain_db: 1.0,
        ..Default::default()
    };
    producer.push_params(p);
    let seq1 = producer.force_flush().unwrap();

    p.input_gain_db = 2.0;
    producer.push_params(p);
    let seq2 = producer.force_flush().unwrap();

    assert!(seq1 > 0);
    assert!(seq2 > seq1);
    assert!(!producer.is_acked(seq2));

    let mut consumer = CommandConsumer::new(rx, &last_ack);
    consumer.drain_and_process(256, |_| {});
    consumer.ack_up_to(seq2);

    assert!(producer.is_acked(seq2));
}

#[test]
fn stress_10k_param_burst_no_loss_no_deadlock() {
    let sched = CommandScheduler::new();
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));

    let channels = sched.extract_producer_consumer().unwrap();
    let cmd_tx = channels.cmd_tx;
    let cmd_rx = channels.cmd_rx;

    let next_seq_clone = Arc::clone(&next_seq);
    let last_ack_clone = Arc::clone(&last_ack);

    let producer_handle = thread::spawn(move || {
        let mut producer = CommandProducer::new(cmd_tx, &next_seq_clone, &last_ack_clone);

        for i in 0..10_000u32 {
            let val = i as f32 * 0.01;
            let p = RtProcessingParams {
                input_gain_db: val,
                output_gain_db: -val,
                gate_threshold_db: -70.0 + val * 0.1,
                bypass: i % 100 == 0,
                ..Default::default()
            };

            producer.push_params(p);
        }
        let last_seq = producer.force_flush().unwrap();

        // T-3.1.1: production ack-wait is the bounded variant — a stalled
        // audio engine must never hang the producer thread forever.
        assert!(
            producer.wait_for_ack_timeout(last_seq, std::time::Duration::from_secs(2)),
            "ack must arrive within the 2 s safety timeout"
        );
        last_seq
    });

    let consumer_handle = thread::spawn(move || {
        let mut consumer = CommandConsumer::new(cmd_rx, &last_ack);
        let mut total_drained = 0usize;

        loop {
            let drained = consumer.drain_and_process(64, |_| {});
            total_drained += drained;

            if drained > 0 {
                consumer.ack_processed();
            }

            let current = next_seq.load(Ordering::Relaxed);
            if current > 0 && last_ack.load(Ordering::Acquire) >= current {
                break;
            }

            std::thread::yield_now();
        }

        total_drained
    });

    let last_seq = producer_handle.join().unwrap();
    let total = consumer_handle.join().unwrap();

    assert!(last_seq > 0, "producer should have sent at least one batch");
    assert!(
        total > 0,
        "consumer should have drained at least one message"
    );
    assert!(
        total <= 256,
        "with coalescing, 10k pushes should produce few messages, got {total}"
    );
}

#[test]
fn interleaved_commands_preserve_ordering() {
    let sched = CommandScheduler::new();
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));

    let channels = sched.extract_producer_consumer().unwrap();
    let cmd_tx = channels.cmd_tx;
    let mut consumer_rx = channels.cmd_rx;

    let mut producer = CommandProducer::new(cmd_tx, &next_seq, &last_ack);

    let mut p = RtProcessingParams {
        input_gain_db: 3.0,
        ..Default::default()
    };
    producer.push_params(p);

    let _ = producer
        .push_command(ClapParamPayload::LoadCabIr { adapter: None })
        .unwrap();

    p.output_gain_db = -6.0;
    producer.push_params(p);

    let _ = producer.force_flush();

    let mut order = Vec::new();
    while let Ok(payload) = consumer_rx.pop() {
        order.push(match payload {
            ClapParamPayload::Params(_) => "P",
            ClapParamPayload::LoadCabIr { .. } => "C",
            _ => "?",
        });
    }

    assert_eq!(
        order,
        vec!["P", "C", "P"],
        "ordering: params before command, then params after"
    );
}

#[test]
fn spin_wait_for_ack_does_not_deadlock() {
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));
    let (tx, rx) = rtrb::RingBuffer::new(256);
    let mut producer = CommandProducer::new(tx, &next_seq, &last_ack);

    let p = RtProcessingParams {
        input_gain_db: 7.0,
        ..Default::default()
    };
    producer.push_params(p);
    let seq = producer.force_flush().unwrap();

    let next_seq2 = Arc::clone(&next_seq);
    let last_ack2 = Arc::clone(&last_ack);

    thread::spawn(move || {
        let mut consumer = CommandConsumer::new(rx, &last_ack2);
        std::thread::sleep(std::time::Duration::from_millis(10));
        consumer.drain_and_process(256, |_| {});
        consumer.ack_up_to(next_seq2.load(Ordering::Relaxed));
    });

    producer.wait_for_ack_timeout(seq, std::time::Duration::from_secs(2));
    assert!(producer.is_acked(seq));
}

/// SA-03 / T-3.1.1: the production ack-wait must time out instead of
/// spin-waiting forever when the audio thread never acknowledges.
///
/// The atomic is left untouched at 0 while the producer waits for a
/// never-issued sequence: the method must return `false` shortly after the
/// deadline, bounding the busy-wait and never freezing the caller.
#[test]
fn wait_for_ack_timeout_times_out_when_ack_never_arrives() {
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));
    let (tx, _rx) = rtrb::RingBuffer::new(256);
    let producer = CommandProducer::new(tx, &next_seq, &last_ack);

    // A sequence that can never be acknowledged: nothing was ever pushed and
    // no consumer thread exists to advance `last_ack`.
    let timeout = std::time::Duration::from_millis(50);
    let start = std::time::Instant::now();
    let acked = producer.wait_for_ack_timeout(42, timeout);
    let elapsed = start.elapsed();

    assert!(
        !acked,
        "a never-acknowledged sequence must time out as false"
    );
    assert!(
        elapsed >= timeout,
        "the wait must not return before the deadline (elapsed={elapsed:?})"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "the wait must stop at the deadline, not keep spinning (elapsed={elapsed:?})"
    );
    assert!(!producer.is_acked(42));
}

/// SA-03 / T-3.1.1: success path — the bounded wait returns `true` as soon as
/// the audio-thread consumer acknowledges the requested sequence.
#[test]
fn wait_for_ack_timeout_returns_true_when_ack_arrives() {
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));
    let (tx, rx) = rtrb::RingBuffer::new(256);
    let mut producer = CommandProducer::new(tx, &next_seq, &last_ack);

    let p = RtProcessingParams {
        input_gain_db: 1.0,
        ..Default::default()
    };
    producer.push_params(p);
    let seq = producer.force_flush().unwrap();

    let mut consumer = CommandConsumer::new(rx, &last_ack);
    consumer.drain_and_process(256, |_| {});
    consumer.ack_up_to(seq);

    assert!(
        producer.wait_for_ack_timeout(seq, std::time::Duration::from_secs(2)),
        "an already-acknowledged sequence must return true immediately"
    );
}

#[test]
fn producer_without_consumer_returns_full_on_overflow() {
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));
    let (tx, _rx) = rtrb::RingBuffer::new(4);
    let mut producer = CommandProducer::new(tx, &next_seq, &last_ack);

    for i in 0..8 {
        let p = RtProcessingParams {
            input_gain_db: i as f32,
            ..Default::default()
        };
        producer.push_params(p);
        let r = producer.force_flush();
        if i < 3 {
            assert!(r.is_ok(), "early pushes should succeed");
        }
    }

    let mut full_count = 0;
    for _ in 0..64 {
        let p = RtProcessingParams {
            input_gain_db: 99.0,
            ..Default::default()
        };
        producer.push_params(p);
        if producer.force_flush().is_err() {
            full_count += 1;
            break;
        }
    }
    assert!(
        full_count > 0,
        "SPSC should have returned Full after saturation"
    );
}

#[test]
fn try_push_command_returns_payload_on_full() {
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));
    let (tx, _rx) = rtrb::RingBuffer::new(4);
    let mut producer = CommandProducer::new(tx, &next_seq, &last_ack);

    // Saturate the ring with 4 non-coalescable commands.
    for _ in 0..4 {
        producer
            .push_command(ClapParamPayload::LoadCabIr { adapter: None })
            .expect("first 4 pushes should succeed");
    }

    // The 5th push must fail and return the command back (fail-closed),
    // instead of dropping it.
    let err = producer
        .try_push_command(ClapParamPayload::LoadCabIr { adapter: None })
        .expect_err("5th push must saturate the ring");
    assert!(
        matches!(err, (PushError::Full, ClapParamPayload::LoadCabIr { .. })),
        "try_push_command must return (Full, payload) so the caller can retain it"
    );

    // No sequence gap: only the 4 successful pushes may have consumed
    // sequence numbers, keeping the FIFO item↔sequence mapping gapless.
    assert_eq!(
        next_seq.load(Ordering::Relaxed),
        4,
        "sequence counter must equal the number of successfully pushed items"
    );
}

#[test]
fn force_flush_full_retains_snapshot() {
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));
    let (tx, mut rx) = rtrb::RingBuffer::new(1);
    let mut producer = CommandProducer::new(tx, &next_seq, &last_ack);

    // First snapshot fills the capacity-1 ring.
    let p1 = RtProcessingParams {
        input_gain_db: 1.0,
        ..Default::default()
    };
    assert!(producer.push_params(p1));
    assert!(producer.force_flush().is_ok());

    // Second snapshot hits Full — and MUST be retained, not dropped (T6.1 #4).
    let p2 = RtProcessingParams {
        input_gain_db: 42.0,
        ..Default::default()
    };
    assert!(producer.push_params(p2));
    assert!(
        producer.force_flush().is_err(),
        "capacity-1 ring must report Full on second flush"
    );

    // Drain the ring, then retry — the retained 42.0 must arrive.
    let mut popped = rx.pop().expect("first snapshot must be present");
    assert!(matches!(popped, ClapParamPayload::Params(_)));

    let seq = producer
        .force_flush()
        .expect("retained snapshot must flush after the ring drains");
    assert!(seq > 0, "retained flush must consume a sequence number");

    popped = rx.pop().expect("retained snapshot must be delivered");
    match popped {
        ClapParamPayload::Params(p) => {
            assert_eq!(
                p.input_gain_db, 42.0,
                "latest parameter value must survive Full via retention"
            );
        }
        other => {
            let _ = other;
            panic!("expected Params payload, got a non-Params command");
        }
    }
}

#[test]
fn restore_txn_atomic_delivery_capacity_1() {
    let sched = CommandScheduler::with_capacity(1);
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));
    let channels = sched.extract_producer_consumer().unwrap();
    let mut producer = CommandProducer::new(channels.cmd_tx, &next_seq, &last_ack);
    let mut consumer = CommandConsumer::new(channels.cmd_rx, &last_ack);

    // Two restores A→B. A fills the capacity-1 ring; B saturates (fail-closed:
    // the whole package is returned to the caller for retention).
    let txn_a = make_restore_txn(1, 1.0, 1.0);
    let txn_b = make_restore_txn(2, 2.0, 2.0);

    let seq_a = match producer.try_push_command(ClapParamPayload::RestoreTxn(txn_a)) {
        Ok(seq) => seq,
        Err(_) => panic!("A must fill the empty ring"),
    };
    let retained_b = match producer.try_push_command(ClapParamPayload::RestoreTxn(txn_b)) {
        Err((PushError::Full, ClapParamPayload::RestoreTxn(t))) => t,
        Ok(_) => panic!("expected Full but B fit the ring"),
        Err(_) => panic!("unexpected error pushing B"),
    };

    // Block 1: the audio thread applies exactly ONE complete package (A).
    let mut applied: Vec<(u64, f32, f32)> = Vec::new();
    let drained = consumer.drain_and_process(1, |p| {
        if let ClapParamPayload::RestoreTxn(t) = p {
            applied.push((
                t.generation,
                t.params.input_gain_db,
                t.model.as_ref().map(|m| m.input_mult_adj).unwrap_or(0.0),
            ));
        }
    });
    assert_eq!(drained, 1, "exactly one command per block");
    consumer.ack_processed();
    assert!(producer.is_acked(seq_a), "A must be acked after its block");

    // Ring now has room: B (retained) is delivered whole.
    let seq_b = match producer.try_push_command(ClapParamPayload::RestoreTxn(retained_b)) {
        Ok(seq) => seq,
        Err(_) => panic!("retained B must push after A drains"),
    };
    assert!(seq_b > seq_a, "B sequence must be strictly after A");

    // Block 2: the audio thread applies exactly ONE complete package (B).
    let drained = consumer.drain_and_process(1, |p| {
        if let ClapParamPayload::RestoreTxn(t) = p {
            applied.push((
                t.generation,
                t.params.input_gain_db,
                t.model.as_ref().map(|m| m.input_mult_adj).unwrap_or(0.0),
            ));
        }
    });
    assert_eq!(drained, 1, "exactly one command per block");
    consumer.ack_processed();
    assert!(producer.is_acked(seq_b), "B must be acked after its block");

    // Every per-block snapshot is 100% A then 100% B — never a hybrid: model,
    // params and generation all come from the same transaction.
    assert_eq!(applied, vec![(1, 1.0, 1.0), (2, 2.0, 2.0)]);
}

#[test]
fn restore_txn_atomic_delivery_capacity_2() {
    let sched = CommandScheduler::with_capacity(2);
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));
    let channels = sched.extract_producer_consumer().unwrap();
    let mut producer = CommandProducer::new(channels.cmd_tx, &next_seq, &last_ack);
    let mut consumer = CommandConsumer::new(channels.cmd_rx, &last_ack);

    // Two restores A→B fit the capacity-2 ring; a third C saturates and is
    // retained whole (fail-closed).
    let txn_a = make_restore_txn(1, 1.0, 1.0);
    let txn_b = make_restore_txn(2, 2.0, 2.0);
    let txn_c = make_restore_txn(3, 3.0, 3.0);

    let seq_a = match producer.try_push_command(ClapParamPayload::RestoreTxn(txn_a)) {
        Ok(seq) => seq,
        Err(_) => panic!("A must fill the empty ring"),
    };
    let seq_b = match producer.try_push_command(ClapParamPayload::RestoreTxn(txn_b)) {
        Ok(seq) => seq,
        Err(_) => panic!("B must fit the capacity-2 ring"),
    };
    assert!(seq_b > seq_a);

    let retained_c = match producer.try_push_command(ClapParamPayload::RestoreTxn(txn_c)) {
        Err((PushError::Full, ClapParamPayload::RestoreTxn(t))) => t,
        Ok(_) => panic!("expected Full but C fit the ring"),
        Err(_) => panic!("unexpected error pushing C"),
    };

    // Each block applies exactly one complete package, in FIFO order.
    let mut snapshots: Vec<(u64, f32, f32)> = Vec::new();
    for _ in 0..2 {
        consumer.drain_and_process(1, |p| {
            if let ClapParamPayload::RestoreTxn(t) = p {
                snapshots.push((
                    t.generation,
                    t.params.input_gain_db,
                    t.model.as_ref().map(|m| m.input_mult_adj).unwrap_or(0.0),
                ));
            }
        });
        consumer.ack_processed();
    }
    assert_eq!(snapshots, vec![(1, 1.0, 1.0), (2, 2.0, 2.0)]);

    // Retained C is delivered whole on the next block — still a complete package.
    let seq_c = match producer.try_push_command(ClapParamPayload::RestoreTxn(retained_c)) {
        Ok(seq) => seq,
        Err(_) => panic!("retained C must push after the ring drains"),
    };
    consumer.drain_and_process(1, |p| {
        if let ClapParamPayload::RestoreTxn(t) = p {
            snapshots.push((
                t.generation,
                t.params.input_gain_db,
                t.model.as_ref().map(|m| m.input_mult_adj).unwrap_or(0.0),
            ));
        }
    });
    consumer.ack_processed();
    assert!(producer.is_acked(seq_c));
    assert_eq!(snapshots, vec![(1, 1.0, 1.0), (2, 2.0, 2.0), (3, 3.0, 3.0)]);
}

// ═══════════════════════════════════════════════════════════════════════════
// T2.3 / F-RT-007 — Command Budgeting primitives
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn structural_classification_light_vs_heavy() {
    // Light: atomic parameter updates drain freely (no budget).
    let light = ClapParamPayload::Params(RtProcessingParams::default());
    assert!(!light.is_structural(), "Params is light, not structural");
    assert_eq!(
        light.structural_kind(),
        None,
        "Params has no structural kind"
    );

    // Heavy: every swap/restore payload is structural and budgeted.
    let heavy = [
        ClapParamPayload::LoadModel {
            generation: 0,
            model_l: None,
            new_resampler: Box::new(NamResampler::new(48000, 48000, 0).unwrap()),
            new_stream: crate::clap::plugin::build_stream_adapter(48000, 48000, 64).unwrap(),
            input_mult_adj: 1.0,
            output_mult_adj: 1.0,
        },
        ClapParamPayload::LoadCabIr { adapter: None },
        ClapParamPayload::SetOversample {
            os_l: Box::new(
                neural_amp_modeler_rs::dsp::oversample::OversampleEngine::new(
                    neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X2,
                    256,
                )
                .unwrap(),
            ),
            os_r: Box::new(
                neural_amp_modeler_rs::dsp::oversample::OversampleEngine::new(
                    neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X2,
                    256,
                )
                .unwrap(),
            ),
        },
        ClapParamPayload::RestoreTxn(RestoreTxn {
            generation: 1,
            model: None,
            ir: None,
            params: RtProcessingParams::default(),
        }),
    ];
    for payload in &heavy {
        assert!(payload.is_structural(), "payload must be structural");
        assert!(
            payload.structural_kind().is_some(),
            "structural payload must carry a kind"
        );
    }

    // Restore is atomic and ack-gated — never coalescible.
    assert!(
        !StructuralKind::Restore.is_coalescible(),
        "RestoreTxn must always be applied atomically (never superseded)"
    );
    for kind in [
        StructuralKind::Model,
        StructuralKind::CabIr,
        StructuralKind::Oversample,
    ] {
        assert!(kind.is_coalescible(), "{kind:?} must be coalescible");
    }
}

#[test]
fn consumer_rollback_and_advance_pending_keep_ack_gapless() {
    let last_ack = Arc::new(AtomicU64::new(0));
    let (mut tx, rx) = rtrb::RingBuffer::new(16);
    let mut consumer = CommandConsumer::new(rx, &last_ack);

    for i in 0..3u32 {
        tx.push(ClapParamPayload::Params(RtProcessingParams {
            input_gain_db: i as f32,
            ..Default::default()
        }))
        .unwrap();
    }

    // Pop two applied commands, then pop a third that will be DEFERRED
    // (structural budget). The deferral must not advance the ack.
    let _ = consumer.pop().unwrap();
    let _ = consumer.pop().unwrap();
    let _ = consumer.pop().unwrap();
    consumer.rollback_last_pop();
    consumer.ack_processed();
    assert_eq!(
        last_ack.load(Ordering::Relaxed),
        2,
        "ack must cover only applied commands, never the deferred one"
    );

    // The deferred command reoccupies its sequence slot when applied at the
    // start of the next callback (advance_pending), keeping the mapping gapless.
    consumer.advance_pending();
    consumer.ack_processed();
    assert_eq!(
        last_ack.load(Ordering::Relaxed),
        3,
        "advance_pending must restore the deferred command's sequence slot"
    );
}

#[test]
fn consumer_peek_does_not_consume() {
    let last_ack = Arc::new(AtomicU64::new(0));
    let (mut tx, rx) = rtrb::RingBuffer::new(16);
    let mut consumer = CommandConsumer::new(rx, &last_ack);

    tx.push(ClapParamPayload::LoadCabIr { adapter: None })
        .unwrap();

    // Peek returns the head without consuming it.
    let peeked = consumer.peek().expect("ring head must be visible");
    assert!(matches!(peeked, ClapParamPayload::LoadCabIr { .. }));
    assert_eq!(
        last_ack.load(Ordering::Relaxed),
        0,
        "peek must not advance ack"
    );

    // The same command is still popped afterwards.
    let popped = consumer.pop().expect("peek must not consume");
    assert!(matches!(popped, ClapParamPayload::LoadCabIr { .. }));

    // Empty ring: peek returns None, pop returns None.
    assert!(consumer.peek().is_none());
    assert!(consumer.pop().is_none());
}

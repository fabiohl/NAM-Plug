// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::clap::plugin::ClapParamPayload;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::thread;

fn make_test_scheduler() -> (CommandScheduler, Arc<AtomicU64>, Arc<AtomicU64>) {
    let sched = CommandScheduler::new();
    let next_seq = Arc::new(AtomicU64::new(0));
    let last_ack = Arc::new(AtomicU64::new(0));
    (sched, next_seq, last_ack)
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

        producer.wait_for_ack(last_seq);
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

    producer.wait_for_ack(seq);
    assert!(producer.is_acked(seq));
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

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Command Budgeting and Structural Command Classification.
//!
//! Verifies that a burst of structural (heavy-swap) commands never executes all
//! in a single audio callback: at most one structural transaction is applied
//! per callback, the excess is deferred (parked) preserving FIFO order and
//! composite-transaction atomicity, same-kind coalescible commands supersede
//! older deferred ones (obsolete resources discarded off-RT via the GC
//! cascade), and the p99/max callback time stays within the performance
//! contract (`p99 < 1.33 ms` per 64-sample block at 48 kHz).

use crate::clap::test_util::{self, StereoTestBuffers, TestHost};
use clack_host::prelude::*;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_STRUCTURAL_DEFERRED, RT_STATUS_STRUCTURAL_SUPERSEDED,
};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use std::sync::atomic::Ordering;
use std::time::Instant;

const N: usize = 512;
const SR_48K: f64 = 48000.0;

/// Performance contract for a 64-sample quantum at 48 kHz
/// (`NeuralAmpModeler-rs/docs/functional-tests.md`, Phase 5): p99 < 1.33 ms.
const RT_BUDGET_NS: u128 = 1_330_000;

fn audio_config(max_frames: u32) -> PluginAudioConfiguration {
    PluginAudioConfiguration {
        sample_rate: SR_48K,
        min_frames_count: 1,
        max_frames_count: max_frames,
    }
}

fn process_block(
    started: &mut StartedPluginAudioProcessor<TestHost>,
    bufs: &mut StereoTestBuffers,
) -> ProcessStatus {
    let mut input_channels = [bufs.in_l.as_mut_slice(), bufs.in_r.as_mut_slice()];
    let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(
            input_channels.iter_mut().map(InputChannel::constant),
        ),
    }]);
    let output_channels = [bufs.out_l.as_mut_slice(), bufs.out_r.as_mut_slice()];
    let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
    }]);
    let input_events = InputEvents::empty();
    let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);
    started
        .process(
            &input_audio,
            &mut output_audio,
            &input_events,
            &mut output_events,
            None,
            None,
        )
        .unwrap()
}

fn main_thread_ptr(
    instance: &mut PluginInstance<TestHost>,
) -> *mut crate::clap::plugin::NamClapMainThread<'static> {
    let raw_ptr = instance.plugin_handle().as_raw_ptr();
    unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |w| Ok(w.main_thread().as_ptr()),
        )
        .unwrap()
    }
}

fn activate_plugin(
    plugin_instance: &mut PluginInstance<TestHost>,
    max_frames: u32,
) -> (
    StartedPluginAudioProcessor<TestHost>,
    *const crate::clap::plugin::NamClapShared,
    *mut crate::clap::plugin::NamClapMainThread<'static>,
) {
    let stopped = plugin_instance
        .activate(|_, _| (), audio_config(max_frames))
        .unwrap();
    let started = stopped.start_processing().unwrap();
    let shared_ptr = test_util::extract_shared(plugin_instance);
    let main_thread_ptr = main_thread_ptr(plugin_instance);
    (started, shared_ptr, main_thread_ptr)
}

/// Builds a cheap, observable cab-sim adapter with a known tail length:
/// `ir_len` samples at partition size `N` ⇒ `tail = (ir_len / N) * N`.
fn make_adapter(ir_len: usize) -> Box<CabSimAdapter> {
    let ir: Vec<f32> = (0..ir_len)
        .map(|i| {
            let t = i as f32;
            (t * 0.05).sin() * (-t * 0.02).exp()
        })
        .collect();
    Box::new(
        CabSimAdapter::new(Box::new(
            ConvEngine::new(&ir, N).expect("ConvEngine must build"),
        ))
        .expect("CabSimAdapter must build"),
    )
}

/// Structural command deferred by the final callback must be resolved at
/// `deactivate()` — its resources drop on the main thread and its rolled-back
/// sequence slot is consumed, so a subsequent deactivate/activate cycle keeps
/// the ack mapping gapless (an ack-gated `RestoreTxn` pushed after
/// reactivation must still be published).
#[test]
fn test_deferred_structural_resolved_on_deactivate_ack_gapless() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let (mut started, _shared_ptr, main_thread_ptr) =
        activate_plugin(&mut plugin_instance, N as u32);

    // Push two structural commands; the first block applies #1 and defers #2.
    {
        let mt = unsafe { &mut *main_thread_ptr };
        mt.cmd_producer
            .push_command(crate::clap::plugin::ClapParamPayload::LoadCabIr { adapter: None })
            .expect("push #1 must succeed");
        mt.cmd_producer
            .push_command(crate::clap::plugin::ClapParamPayload::LoadCabIr { adapter: None })
            .expect("push #2 must succeed");
    }
    let mut bufs = StereoTestBuffers::new(N, 0.0, 0.0);
    process_block(&mut started, &mut bufs);

    // Deactivate with the deferred #2 still parked (never processed).
    let stopped = started.stop_processing();
    plugin_instance.deactivate(stopped);

    // Reactivate and push an ack-gated RestoreTxn: it must apply AND be acked
    // (publish path) — proving the ack sequence survived the deferred drop.
    let (mut started, shared_ptr, main_thread_ptr) =
        activate_plugin(&mut plugin_instance, N as u32);
    let shared = unsafe { &*shared_ptr };
    {
        let mt = unsafe { &mut *main_thread_ptr };
        mt.cmd_producer
            .push_command(crate::clap::plugin::ClapParamPayload::RestoreTxn(
                crate::clap::plugin::RestoreTxn {
                    generation: 42,
                    model: None,
                    ir: None,
                    params: RtProcessingParams::default(),
                },
            ))
            .expect("restore push after reactivation must succeed");
    }

    process_block(&mut started, &mut bufs);
    process_block(&mut started, &mut bufs);
    assert_eq!(
        shared.cold.last_applied_generation.load(Ordering::Relaxed),
        42,
        "the restore must apply after reactivation (ack gapless across the deferred drop)"
    );

    // Ack phase: housekeeping publishes the restore.
    {
        let mt = unsafe { &mut *main_thread_ptr };
        mt.housekeeping();
    }
    assert!(
        unsafe { &*main_thread_ptr }.pending_restore.is_none(),
        "the restore must be acked and published after the deferred-drop resolution"
    );
}

/// Acceptance criterion: a burst of structural commands never executes all in one
/// callback. With `MAX_STRUCTURAL_COMMANDS_PER_CALLBACK == 1`, a burst of 8
/// atomic restores applies exactly one generation per block, in FIFO order,
/// with no loss and no reordering — the excess is deferred, never dropped.
#[test]
fn test_structural_budget_one_per_callback() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let (mut started, shared_ptr, main_thread_ptr) =
        activate_plugin(&mut plugin_instance, N as u32);
    let shared = unsafe { &*shared_ptr };

    let mt = unsafe { &mut *main_thread_ptr };
    for generation in 1..=8u64 {
        mt.cmd_producer
            .push_command(crate::clap::plugin::ClapParamPayload::RestoreTxn(
                crate::clap::plugin::RestoreTxn {
                    generation,
                    model: None,
                    ir: None,
                    params: RtProcessingParams::default(),
                },
            ))
            .expect("restore burst must fit the ring");
    }

    let mut bufs = StereoTestBuffers::new(N, 0.0, 0.0);

    // One block per structural command: each callback applies exactly one
    // generation, strictly in FIFO order (RestoreTxn is never coalescible).
    for expected_gen in 1..=8u64 {
        process_block(&mut started, &mut bufs);
        assert_eq!(
            shared.cold.last_applied_generation.load(Ordering::Relaxed),
            expected_gen,
            "exactly one structural command must be applied per callback, in order"
        );
    }

    // No loss: all 8 generations were applied.
    assert_eq!(
        shared.cold.last_applied_generation.load(Ordering::Relaxed),
        8,
        "the full burst must be applied (no payload lost)"
    );

    // 7 of the 8 commands were deferred (all but the first), observed via the
    // RT telemetry counter and flag.
    assert_eq!(
        shared
            .cold
            .rt_status
            .structural_deferred_total
            .load(Ordering::Relaxed),
        7,
        "all excess structural commands must be deferred, never dropped"
    );
    assert!(
        shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_STRUCTURAL_DEFERRED),
        "RT_STATUS_STRUCTURAL_DEFERRED must be set during a structural burst"
    );
}

/// Command coalescing: a deferred coalescible command superseded by a
/// newer same-kind ring head is never applied; its obsolete resources are
/// discarded off-RT through the GC cascade (latest-wins, no intermediate state
/// observed by the DSP).
#[test]
fn test_structural_coalescing_supersedes_same_kind() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let (mut started, shared_ptr, main_thread_ptr) =
        activate_plugin(&mut plugin_instance, N as u32);
    let shared = unsafe { &*shared_ptr };

    // Three distinct IRs: A (512 ⇒ tail 512), B (1024 ⇒ tail 1024), C
    // (2048 ⇒ tail 2048). A applies; B is deferred; C supersedes B.
    let ir_a = make_adapter(512);
    let ir_b = make_adapter(1024);
    let ir_c = make_adapter(2048);

    {
        let mt = unsafe { &mut *main_thread_ptr };
        mt.cmd_producer
            .push_command(crate::clap::plugin::ClapParamPayload::LoadCabIr {
                adapter: Some(ir_a),
            })
            .expect("IR A push must succeed");
        mt.cmd_producer
            .push_command(crate::clap::plugin::ClapParamPayload::LoadCabIr {
                adapter: Some(ir_b),
            })
            .expect("IR B push must succeed");
        mt.cmd_producer
            .push_command(crate::clap::plugin::ClapParamPayload::LoadCabIr {
                adapter: Some(ir_c),
            })
            .expect("IR C push must succeed");
    }

    let mut bufs = StereoTestBuffers::new(N, 0.1, 0.1);

    // Block 1: applies IR A (budget), defers IR B.
    process_block(&mut started, &mut bufs);
    assert_eq!(
        shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed),
        512,
        "block 1 must apply IR A"
    );
    assert_eq!(
        shared
            .cold
            .rt_status
            .structural_deferred_total
            .load(Ordering::Relaxed),
        1,
        "IR B must be deferred at block 1"
    );

    // Block 2: IR C supersedes the deferred IR B (same kind, coalescible) and
    // applies — the tail jumps 512 → 2048, never passing through 1024. IR B's
    // adapter is discarded off-RT via the GC cascade.
    process_block(&mut started, &mut bufs);
    assert_eq!(
        shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed),
        2048,
        "block 2 must apply IR C (latest wins) — the intermediate IR B must be skipped"
    );
    assert_eq!(
        shared
            .cold
            .rt_status
            .structural_superseded_total
            .load(Ordering::Relaxed),
        1,
        "IR B must be counted as superseded"
    );
    assert!(
        shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_STRUCTURAL_SUPERSEDED),
        "RT_STATUS_STRUCTURAL_SUPERSEDED must be set when a deferred command is superseded"
    );

    // The superseded IR B adapter must be drained off-RT (never dropped on the
    // audio thread, never leaked).
    {
        let mt = unsafe { &mut *main_thread_ptr };
        mt.housekeeping();
    }
    assert!(
        shared.cold.rt_status.drains.load(Ordering::Relaxed) >= 1,
        "the superseded adapter must reach the off-RT GC drain"
    );
}

/// Invariant + acceptance: a burst of 64 structural commands cannot
/// degrade the callback time — p99/max stays within the performance contract
/// (`p99 < 1.33 ms` per 64-sample block at 48 kHz). The bypass path keeps DSP
/// cost negligible so the measurement isolates the *drain* cost.
#[test]
fn test_structural_burst_p99_within_contract() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let (mut started, shared_ptr, main_thread_ptr) = activate_plugin(&mut plugin_instance, 64);
    let shared = unsafe { &*shared_ptr };

    let mut bufs = StereoTestBuffers::new(64, 0.0, 0.0);

    // Warm-up: settle one-time setup (priority query, DAZ/FTZ) and any
    // deferred commands from activation.
    for _ in 0..16 {
        process_block(&mut started, &mut bufs);
    }

    // Steady-state baseline: 64 empty blocks.
    let mut steady: Vec<u128> = Vec::with_capacity(64);
    for _ in 0..64 {
        let t0 = Instant::now();
        process_block(&mut started, &mut bufs);
        steady.push(t0.elapsed().as_nanos());
    }

    // Burst: enqueue 64 non-coalescible structural commands (atomic restores)
    // and drain them over the following 64 blocks. RestoreTxn is never
    // superseded, so every block applies exactly one structural transaction —
    // the worst-case drain cost per callback.
    {
        let mt = unsafe { &mut *main_thread_ptr };
        for generation in 1..=64u64 {
            mt.cmd_producer
                .push_command(crate::clap::plugin::ClapParamPayload::RestoreTxn(
                    crate::clap::plugin::RestoreTxn {
                        generation,
                        model: None,
                        ir: None,
                        params: RtProcessingParams::default(),
                    },
                ))
                .expect("restore burst must fit the ring");
        }
    }

    let mut burst: Vec<u128> = Vec::with_capacity(64);
    for _ in 0..64 {
        let t0 = Instant::now();
        process_block(&mut started, &mut bufs);
        burst.push(t0.elapsed().as_nanos());
    }

    // Deterministic part: all 64 commands were drained (one apply + one defer
    // per block, RestoreTxn never superseded) and the callback budget held.
    assert_eq!(
        shared.cold.last_applied_generation.load(Ordering::Relaxed),
        64,
        "the full 64-command restore burst must be applied (no loss)"
    );
    assert_eq!(
        shared
            .cold
            .rt_status
            .structural_deferred_total
            .load(Ordering::Relaxed),
        63,
        "each of the 64 burst blocks must defer the next structural command"
    );

    let p99 = |v: &[u128]| -> u128 {
        let mut sorted = v.to_vec();
        sorted.sort_unstable();
        // In integer division with N = 64, 64 * 99 / 100 = 63 (index 63 = max).
        // Using (len - 1) * 99 / 100 yields index 62 for p99 on 64 elements,
        // avoiding contamination by a single OS scheduler preemption event.
        let idx = (sorted.len().saturating_sub(1) * 99) / 100;
        sorted[idx]
    };
    let p99_burst = p99(&burst);
    let max_burst = *burst.iter().max().unwrap_or(&0);
    let p99_steady = p99(&steady);

    // Measured: RT contract for a 64-sample quantum is p99 < 1.33 ms
    // (docs/functional-tests.md Phase 5). The burst adds only a single
    // structural apply per block, so the drain must stay well within it.
    assert!(
        p99_burst < RT_BUDGET_NS,
        "p99 burst ({p99_burst} ns) must stay within the 1.33 ms RT contract"
    );
    assert!(
        max_burst < RT_BUDGET_NS,
        "max burst block ({max_burst} ns) must stay within the 1.33 ms RT contract"
    );

    // Relative sanity: the drain-heavy blocks cannot be pathologically slower
    // than steady-state (margin: 4× + 800 µs, tolerating unoptimized debug-profile
    // execution and CI / multi-threaded test runner scheduler preemption,
    // clamped to RT_BUDGET_NS).
    // Measured: steady p99 ~ 56 µs; burst p99 under parallel test runner load ~ 756 µs; RT contract ceiling = 1,330 µs.
    let relative_budget = p99_steady
        .saturating_mul(4)
        .saturating_add(800_000)
        .min(RT_BUDGET_NS);
    assert!(
        p99_burst <= relative_budget,
        "p99 burst ({p99_burst} ns) must not degrade vs steady-state p99 \
         ({p99_steady} ns; relative budget {relative_budget} ns)"
    );
}

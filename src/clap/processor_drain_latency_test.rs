// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Drain-latency certification gate for the canonical per-callback drain
//! window (`SwapBudget` construction + both `RtSwapDrain::drain` calls —
//! command ring first, slimmable ring second):
//! - **Machinery** (structural automation burst): P99 < 1.0 µs.
//! - **Scalar saturation** (full 64-pop cap of inline snapshot applies):
//!   P99 < 1% of the 1.33 ms callback contract.
//!
//! Reference budget: the scheduler spike measured drain p99 ≈ 0.94 µs under
//! full simultaneous saturation across five drains
//! (`NeuralAmpModeler-rs/docs/rt-structural-scheduler-spike.md`); this suite
//! re-certifies that machinery contract for the plugin's two drains after
//! the migration to the engine scheduler.
//!
//! Methodology: a `#[cfg(test)]` probe in `process_events` records the
//! drain-window duration per block with calibrated serialized TSC reads
//! (~15 ns each — the two reads are part of the reported sample). The probe
//! window isolates the drain path from the surrounding DSP work.
//!
//! Scenarios:
//! - **Automation burst**: 256 non-coalescible `RestoreTxn` commands (ring
//!   100% full) drained at the worst-case structural rate of exactly one
//!   apply per callback, each block also popping and deferring behind the
//!   budget-blocked head. This is the regime the spike certified (≈ 0.94 µs
//!   p99 across five saturated drains) — the 1.0 µs machinery gate applies.
//! - **Scalar saturation**: a permanent ≥ 128-deep backlog of light `Params`
//!   payloads keeps the drain at the full 64-pop cap every callback, setting
//!   `RT_STATUS_SPSC_DRAIN_TRUNCATED` continuously. Every popped scalar is
//!   *really applied inline* (~50 ns per payload: cold handler call, box
//!   unpack, full snapshot apply), so this regime is bounded by its share of
//!   the callback deadline — p99 < 1% of the 1.33 ms callback contract
//!   (13.3 µs) — not by the 1.0 µs machinery gate. It models the drain
//!   recovering from a main-thread backlog after an OS scheduling stall
//!   (ring filled to capacity while the audio thread was descheduled).
//!
//! These are micro-latency certification tests: environment-sensitive by
//! nature, they are `#[ignore]`d and run on demand (same convention as the
//! GC stress tests), in release profile on a pinned core:
//!
//! ```text
//! taskset -c <core> cargo test --release --features testing \
//!     --lib processor_drain_latency_test -- --ignored --nocapture
//! ```

#![cfg(target_arch = "x86_64")]

use crate::clap::test_util::{self, StereoTestBuffers, TestHost};
use clack_host::prelude::*;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::common::spsc::RT_STATUS_SPSC_DRAIN_TRUNCATED;
use std::sync::atomic::Ordering;

const N: usize = 64;
const SR_48K: f64 = 48000.0;

/// Latency certification target for the drain machinery under the structural
/// automation burst: P99 < 1.0 µs. The whole-callback contract (p99 < 1.33 ms)
/// is certified separately by `test_structural_burst_p99_within_contract`.
const DRAIN_P99_BUDGET_NS: u64 = 1_000;

/// Scalar-saturation budget: 1% of the 1.33 ms callback contract (13.3 µs).
/// At the full 64-pop cap every popped scalar is really applied inline
/// (~50 ns per payload measured), so this regime is bounded by its share of
/// the callback deadline rather than by the 1.0 µs machinery gate.
const DRAIN_SATURATION_P99_BUDGET_NS: u64 = 13_300;

/// Commands per automation burst (== command ring capacity, `CMD_QUEUE_CAPACITY`):
/// the ring is 100% full while draining.
const BURST_COMMANDS: usize = 256;

/// Scalar-saturation topology: initial backlog and per-block top-up keep the
/// ring permanently ≥ 128 payloads deep, so every callback pops the full
/// `pops_per_callback` cap (64) and exits with the truncation flag set.
const SATURATION_BACKLOG: usize = 192;
const SATURATION_TOPUP: usize = 64;
const SATURATION_ROUNDS: usize = 256;

/// Warm-up blocks before measurement (one-time priority query, DAZ/FTZ,
/// activation leftovers).
const WARMUP_BLOCKS: usize = 16;

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
) -> *const crate::clap::plugin::NamClapMainThread<'static> {
    let raw_ptr = instance.plugin_handle().as_raw_ptr();
    unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |w| Ok(w.main_thread() as *const crate::clap::plugin::NamClapMainThread<'static>),
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
    *const crate::clap::plugin::NamClapMainThread<'static>,
) {
    let stopped = plugin_instance
        .activate(|_, _| (), audio_config(max_frames))
        .unwrap();
    let started = stopped.start_processing().unwrap();
    let shared_ptr = test_util::extract_shared(plugin_instance);
    let main_thread_ptr = main_thread_ptr(plugin_instance);
    (started, shared_ptr, main_thread_ptr)
}

/// Percentile of a sample set, index `(len - 1) * pct / 100` (the top
/// `len - 1 - idx` samples — OS preemption spikes among them — stay above
/// the reported percentile).
fn percentile(samples: &[u64], pct: usize) -> u64 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let idx = (sorted.len().saturating_sub(1) * pct) / 100;
    sorted[idx]
}

fn print_stats(label: &str, samples: &[u64]) {
    let mean = samples.iter().sum::<u64>() / samples.len() as u64;
    println!(
        "{label}: n={} min={} p50={} p90={} p99={} max={} mean={} ns \
         (includes ~30 ns of probe TSC reads)",
        samples.len(),
        samples.iter().min().unwrap_or(&0),
        percentile(samples, 50),
        percentile(samples, 90),
        percentile(samples, 99),
        samples.iter().max().unwrap_or(&0),
        mean,
    );
}

fn read_probe() -> u64 {
    super::DRAIN_PROBE_NS.load(Ordering::Relaxed)
}

/// Automation burst at the worst-case structural rate: 256 non-coalescible
/// restores fill the ring completely, then one structural apply lands per
/// callback while every drain also stops behind the budget-blocked head.
/// The drain window (budget + both drains) must keep p99 < 1.0 µs.
#[test]
#[ignore = "micro-latency certification: run on demand in release on a pinned core — \
           taskset -c <core> cargo test --release --features testing --lib \
           processor_drain_latency_test -- --ignored --nocapture"]
fn test_drain_p99_automation_burst_under_1us() {
    neural_amp_modeler_rs::common::tsc::calibrate_tsc();

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let (mut started, shared_ptr, main_thread_ptr) =
        activate_plugin(&mut plugin_instance, N as u32);
    let shared = unsafe { &*shared_ptr };

    let mut bufs = StereoTestBuffers::new(N, 0.0, 0.0);
    for _ in 0..WARMUP_BLOCKS {
        process_block(&mut started, &mut bufs);
    }
    let ack_before = shared.cold.cmd_last_ack.load(Ordering::Acquire);

    // Fill the command ring to 100% capacity with atomic restore
    // transactions (never coalesced, never superseded).
    {
        let mt = unsafe { &*main_thread_ptr };
        for generation in 1..=BURST_COMMANDS as u64 {
            mt.cmd_producer
                .borrow_mut()
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

    let mut samples: Vec<u64> = Vec::with_capacity(BURST_COMMANDS);
    for _ in 0..BURST_COMMANDS {
        process_block(&mut started, &mut bufs);
        samples.push(read_probe());
    }

    print_stats("drain automation burst", &samples);

    // Zero loss: every transaction applied, every sequence slot acked.
    assert_eq!(
        shared.cold.last_applied_generation.load(Ordering::Relaxed),
        BURST_COMMANDS as u64,
        "the full restore burst must be applied (one structural apply per callback)"
    );
    assert_eq!(
        shared.cold.cmd_last_ack.load(Ordering::Acquire) - ack_before,
        BURST_COMMANDS as u64,
        "the ack must cover every applied transaction exactly once (gapless)"
    );

    let p99 = percentile(&samples, 99);
    assert!(
        p99 < DRAIN_P99_BUDGET_NS,
        "drain p99 under automation burst ({p99} ns) must stay below \
         {DRAIN_P99_BUDGET_NS} ns"
    );
}

/// Light-scalar saturation: a permanent ≥ 128-deep backlog of `Params`
/// payloads forces every callback to pop the full 64-payload cap and exit
/// with `RT_STATUS_SPSC_DRAIN_TRUNCATED` — the drain recovering from a
/// main-thread backlog after a scheduling stall, each scalar applied inline.
/// The drain window must keep p99 below 1% of the callback deadline
/// (`DRAIN_SATURATION_P99_BUDGET_NS`); the 1.0 µs machinery gate is
/// certified by the automation-burst scenario.
#[test]
#[ignore = "micro-latency certification: run on demand in release on a pinned core — \
           taskset -c <core> cargo test --release --features testing --lib \
           processor_drain_latency_test -- --ignored --nocapture"]
fn test_drain_scalar_saturation_within_deadline_budget() {
    use crate::clap::plugin::ClapParamPayload;

    neural_amp_modeler_rs::common::tsc::calibrate_tsc();

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let (mut started, shared_ptr, main_thread_ptr) =
        activate_plugin(&mut plugin_instance, N as u32);
    let shared = unsafe { &*shared_ptr };

    let mut bufs = StereoTestBuffers::new(N, 0.0, 0.0);
    for _ in 0..WARMUP_BLOCKS {
        process_block(&mut started, &mut bufs);
    }
    let ack_before = shared.cold.cmd_last_ack.load(Ordering::Acquire);

    let scalar =
        || ClapParamPayload::Params(RtProcessingParams::default().with_input_gain_db(-6.0));

    // Seed the backlog below the ring capacity (256), then top it up every
    // block so the drain always exits on the pop cap, never on an empty ring.
    {
        let mt = unsafe { &*main_thread_ptr };
        for _ in 0..SATURATION_BACKLOG {
            mt.cmd_producer
                .borrow_mut()
                .push_command(scalar())
                .expect("scalar backlog must fit the ring");
        }
    }

    let mut samples: Vec<u64> = Vec::with_capacity(SATURATION_ROUNDS);
    for _ in 0..SATURATION_ROUNDS {
        process_block(&mut started, &mut bufs);
        samples.push(read_probe());

        let mt = unsafe { &*main_thread_ptr };
        for _ in 0..SATURATION_TOPUP {
            mt.cmd_producer
                .borrow_mut()
                .push_command(scalar())
                .expect("scalar top-up must fit the ring (drain consumed 64)");
        }
    }

    print_stats("drain scalar saturation", &samples);

    // Full-pop-cap saturation held on every block: the ack covers exactly
    // one popped payload per cap slot, and the truncation flag is latched.
    assert_eq!(
        shared.cold.cmd_last_ack.load(Ordering::Acquire) - ack_before,
        (SATURATION_ROUNDS * 64) as u64,
        "every callback must have popped the full 64-payload cap (zero loss)"
    );
    assert!(
        shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_SPSC_DRAIN_TRUNCATED),
        "a permanent backlog must keep RT_STATUS_SPSC_DRAIN_TRUNCATED set"
    );

    let p99 = percentile(&samples, 99);
    println!(
        "scalar saturation marginal cost ≈ {} ns per inline snapshot apply (p99/64 pops)",
        p99 / 64
    );
    assert!(
        p99 < DRAIN_SATURATION_P99_BUDGET_NS,
        "drain p99 under scalar saturation ({p99} ns) must stay below \
         {DRAIN_SATURATION_P99_BUDGET_NS} ns (1% of the 1.33 ms callback contract)"
    );
}

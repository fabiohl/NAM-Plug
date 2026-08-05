// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use clack_host::prelude::*;
use nam_plug::clap::plugin::shared::NamClapShared;
use nam_plug::clap::test_util;
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use std::sync::atomic::Ordering;

const PARTITION_SIZE: usize = 512;
const SAMPLE_RATE: f64 = 48000.0;

fn make_synthetic_ir(len: usize) -> Vec<f32> {
    let mut ir = vec![0.0f32; len];
    ir[0] = 1.0;
    ir
}

/// Creates a fully activated plugin and returns its shared state.
fn setup_shared() -> &'static NamClapShared {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    let audio_config = PluginAudioConfiguration {
        sample_rate: SAMPLE_RATE,
        min_frames_count: PARTITION_SIZE as u32,
        max_frames_count: PARTITION_SIZE as u32,
    };

    let shared_ptr = test_util::extract_shared(&mut plugin_instance);
    let stopped = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let _started = stopped.start_processing().unwrap();

    std::mem::forget(_started);
    std::mem::forget(plugin_instance);

    unsafe { &*shared_ptr }
}

/// Simulates the result of `cold_load_cabsim` by computing the tail length
/// that would be stored in `cabsim_tail_samples` after a CabSim swap.
fn simulate_cold_load_cabsim(shared: &NamClapShared, ir_len: usize, ir_loaded: bool) {
    let cabsim_tail = if ir_loaded {
        let engine = ConvEngine::new(&make_synthetic_ir(ir_len), PARTITION_SIZE)
            .expect("ConvEngine construction failed");
        (engine.num_partitions() * engine.latency_samples()) as u32
    } else {
        0
    };
    shared
        .rt_to_ui
        .cabsim_tail_samples
        .store(cabsim_tail, Ordering::Relaxed);
}

#[test]
fn test_tail_without_ir_is_zero() {
    let shared = setup_shared();

    simulate_cold_load_cabsim(shared, 0, false);

    let tail = shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed);
    assert_eq!(
        tail, 0,
        "cabsim_tail_samples must be 0 when no IR is loaded"
    );
}

#[test]
fn test_tail_with_ir_is_at_least_ir_length() {
    let ir_len = 48000;
    let shared = setup_shared();

    simulate_cold_load_cabsim(shared, ir_len, true);

    let tail = shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed);
    let min_expected = (ir_len as f64 * 0.9) as u32;
    assert!(
        tail >= min_expected,
        "cabsim_tail_samples ({tail}) must be >= 90% of IR length ({min_expected})"
    );

    let expected_ceil = (ir_len.div_ceil(PARTITION_SIZE) * PARTITION_SIZE) as u32;
    assert_eq!(
        tail, expected_ceil,
        "cabsim_tail_samples must equal ceil(IR_len / partition_size) * partition_size"
    );
}

#[test]
fn test_tail_after_ir_removal_returns_to_zero() {
    let ir_len = 48000;
    let shared = setup_shared();

    simulate_cold_load_cabsim(shared, ir_len, true);
    let tail_with_ir = shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed);
    assert!(tail_with_ir > 0, "tail must be > 0 after loading IR");

    simulate_cold_load_cabsim(shared, ir_len, false);
    let tail_after_remove = shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed);
    assert_eq!(
        tail_after_remove, 0,
        "cabsim_tail_samples must return to 0 after IR is removed"
    );
}

#[test]
fn test_tail_includes_base_latency_and_cabsim_tail() {
    let ir_len = 48000;
    let shared = setup_shared();

    let base_latency = 64u32;
    shared
        .rt_to_ui
        .current_latency
        .store(base_latency, Ordering::Relaxed);

    simulate_cold_load_cabsim(shared, ir_len, true);

    let cabsim_tail = shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed);
    let total_tail = base_latency + cabsim_tail;

    let min_expected = (ir_len as f64 * 0.9) as u32;
    assert!(
        total_tail >= min_expected,
        "total tail ({total_tail} = {base_latency} + {cabsim_tail}) must be >= 90% of IR length ({min_expected})"
    );
}

#[test]
fn test_tail_with_different_ir_lengths() {
    let shared = setup_shared();

    let base_latency = 128u32;
    shared
        .rt_to_ui
        .current_latency
        .store(base_latency, Ordering::Relaxed);

    for ir_len in [512, 1024, 2048, 8192, 48000] {
        simulate_cold_load_cabsim(shared, ir_len, true);

        let cabsim_tail = shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed);
        let expected = (ir_len.div_ceil(PARTITION_SIZE) * PARTITION_SIZE) as u32;
        assert_eq!(
            cabsim_tail, expected,
            "cabsim_tail for IR len={ir_len}: got {cabsim_tail}, expected {expected}"
        );

        let total_tail = base_latency + cabsim_tail;
        assert!(
            total_tail >= ir_len as u32,
            "total tail ({total_tail}) must cover IR length ({ir_len})"
        );
    }
}

#[test]
fn test_passthrough_exact_ir_length() {
    let shared = setup_shared();

    shared.rt_to_ui.current_latency.store(0, Ordering::Relaxed);

    let ir_len = 512;
    simulate_cold_load_cabsim(shared, ir_len, true);

    let cabsim_tail = shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed);
    assert_eq!(
        cabsim_tail, ir_len as u32,
        "ir_len=partition_size → cabsim_tail should equal ir_len exactly"
    );
}

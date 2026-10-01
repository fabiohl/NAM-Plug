// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Processor poisoning containment test suite (S2-T1, S2-T2, S2-T4 / F-NP-02, F-NP-09).
//!
//! Validates:
//! * Single panic per activation: Injected panics in `process()` or `reset()` are caught
//!   and latch `poisoned = true` without crashing the host or triggering panic cascades.
//! * Zero-alloc O(1) containment path: Once poisoned, subsequent `process()` calls return
//!   `Ok(ProcessStatus::Continue)` with zeroed output audio ports and zero heap allocations.
//! * Error catalog & zero `Box::leak`: `panic_to_error` uses static catalog string
//!   `errors::processor::AUDIO_CALLBACK_PANICKED`.
//! * Atomic RT status flag `RT_STATUS_PROCESSOR_POISONED` is raised on panic.
//! * Main thread detects status flag during `housekeeping()` and requests host restart once.
//! * Clean reactivation: `deactivate()` discards corrupted DSP state, and a subsequent
//!   `activate()` clears the poisoned flag, allowing clean audio resumption.

use crate::clap::host_harness::{
    extract_plugin_main_thread, extract_plugin_shared, make_harness_audio_processor,
    make_test_plugin_with_harness, perform_restart,
};
use crate::clap::plugin::errors;
use crate::clap::test_util::assert_zero_alloc;
use clack_host::prelude::*;
use clack_plugin::plugin::PluginError;
use neural_amp_modeler_rs::common::spsc::RT_STATUS_PROCESSOR_POISONED;
use std::sync::atomic::Ordering;

const BLOCK: usize = 64;

fn audio_config() -> PluginAudioConfiguration {
    PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: BLOCK as u32,
        max_frames_count: BLOCK as u32,
    }
}

fn process_block(
    started: &mut StartedPluginAudioProcessor<crate::clap::host_harness::CompleteHost>,
    in_l: &mut [f32],
    in_r: &mut [f32],
    out_l: &mut [f32],
    out_r: &mut [f32],
) -> Result<ProcessStatus, PluginInstanceError> {
    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);

    let mut in_ch = [in_l, in_r];
    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(in_ch.iter_mut().map(InputChannel::constant)),
    }]);
    let out_ch = [out_l, out_r];
    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
    }]);
    let mut output_events_buffer = EventBuffer::new();
    let mut out_ev = OutputEvents::from_buffer(&mut output_events_buffer);

    started.process(
        &input_audio,
        &mut output_audio,
        &InputEvents::empty(),
        &mut out_ev,
        None,
        None,
    )
}

fn ensure_isolated_crash_dir() {
    if std::env::var_os("NAM_CRASH_DIR").is_none() {
        let temp_dir = std::env::temp_dir().join("nam_plug_test_crashes");
        let _ = std::fs::create_dir_all(&temp_dir);
        unsafe {
            std::env::set_var("NAM_CRASH_DIR", temp_dir.as_os_str());
        }
    }
}

#[test]
fn test_panic_to_error_uses_static_catalog_message() {
    let panic_payload: Box<dyn std::any::Any + Send> = Box::new("something unexpected");
    let err = super::panic_to_error(panic_payload);
    match err {
        PluginError::Message(msg) => {
            assert_eq!(msg, errors::processor::AUDIO_CALLBACK_PANICKED);
        }
        other => panic!("Expected PluginError::Message, got: {:?}", other),
    }
}

#[test]
fn test_crash_isolation_does_not_pollute_user_cache() {
    let temp_dir = std::env::temp_dir().join("nam_plug_crash_isolation_check");
    let _ = std::fs::create_dir_all(&temp_dir);
    unsafe {
        std::env::set_var("NAM_CRASH_DIR", temp_dir.as_os_str());
    }

    assert_eq!(
        neural_amp_modeler_rs::common::panic_hook::crash_directory(),
        Some(temp_dir)
    );
}

#[test]
fn test_process_panic_poisons_processor_silences_audio_and_limits_panics() {
    ensure_isolated_crash_dir();
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let config = audio_config();
    let state_ap = state.clone();

    let stopped = instance
        .activate(move |_, _| make_harness_audio_processor(&state_ap), config)
        .expect("activate");
    let mut started = stopped.start_processing().expect("start_processing");

    let shared = unsafe { &*extract_plugin_shared(&mut instance) };
    let main_thread = unsafe { &*extract_plugin_main_thread(&mut instance) };

    // Initially, processor is healthy and bit is not set.
    assert!(
        !shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_PROCESSOR_POISONED)
    );

    let mut in_l = vec![0.7f32; BLOCK];
    let mut in_r = vec![0.7f32; BLOCK];
    let mut out_l = vec![999.0f32; BLOCK];
    let mut out_r = vec![999.0f32; BLOCK];

    // Inject panic on next process callback
    super::TEST_PANIC_INJECTION.store(true, Ordering::Relaxed);

    // 1. First process call encounters injected panic.
    let first_res = process_block(&mut started, &mut in_l, &mut in_r, &mut out_l, &mut out_r);

    // Verify:
    // - Err(PluginInstanceError::ProcessingFailed) returned on panic
    assert!(first_res.is_err(), "First call must return error on panic");
    match first_res {
        Err(PluginInstanceError::ProcessingFailed) => {}
        other => panic!(
            "Expected PluginInstanceError::ProcessingFailed, got: {:?}",
            other
        ),
    }

    // - Audio output ports silenced (all zeros) immediately
    for (i, &s) in out_l.iter().enumerate() {
        assert_eq!(s, 0.0, "out_l[{i}] must be silenced to 0.0 on panic");
    }
    for (i, &s) in out_r.iter().enumerate() {
        assert_eq!(s, 0.0, "out_r[{i}] must be silenced to 0.0 on panic");
    }

    // - Status flag RT_STATUS_PROCESSOR_POISONED set
    assert!(
        shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_PROCESSOR_POISONED),
        "RT_STATUS_PROCESSOR_POISONED bit must be set on panic"
    );

    // 2. Subsequent process calls: Fast O(1) containment path
    // They must succeed with Ok(ProcessStatus::Continue), silence output, and trigger zero panics.
    for block_idx in 0..10 {
        out_l.fill(123.45);
        out_r.fill(123.45);

        let res = process_block(&mut started, &mut in_l, &mut in_r, &mut out_l, &mut out_r);
        assert!(
            res.is_ok(),
            "Block {block_idx} after poisoning must return Ok(ProcessStatus::Continue)"
        );
        assert_eq!(res.unwrap(), ProcessStatus::Continue);

        for (i, &s) in out_l.iter().enumerate() {
            assert_eq!(
                s, 0.0,
                "out_l[{i}] in block {block_idx} must remain silence"
            );
        }
        for (i, &s) in out_r.iter().enumerate() {
            assert_eq!(
                s, 0.0,
                "out_r[{i}] in block {block_idx} must remain silence"
            );
        }
    }

    // 3. Zero-alloc check in poisoned state
    assert_zero_alloc("poisoned process() block", || {
        let _ = process_block(&mut started, &mut in_l, &mut in_r, &mut out_l, &mut out_r);
    });

    // 4. Off-RT main thread detects poisoning in housekeeping and requests restart
    assert!(!state.restart_requested.load(Ordering::SeqCst));
    main_thread.housekeeping();
    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "Main thread must request host restart when processor is poisoned"
    );

    // A second housekeeping call does not spuriously re-request while already requested
    state.restart_requested.store(false, Ordering::SeqCst);
    main_thread.housekeeping();
    assert!(
        !state.restart_requested.load(Ordering::SeqCst),
        "Main thread must not re-trigger restart request while already requested"
    );

    // 5. Host restarts plugin (deactivate -> activate)
    let mut restarted = perform_restart(&mut instance, started, &state, config);

    // After reactivation, status flag is cleared
    assert!(
        !shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_PROCESSOR_POISONED),
        "Reactivation must clear RT_STATUS_PROCESSOR_POISONED flag"
    );

    // Main thread resets poison_restart_requested latch
    main_thread.housekeeping();

    // Clean processing resumes normally
    out_l.fill(0.0);
    out_r.fill(0.0);
    let post_restart_res =
        process_block(&mut restarted, &mut in_l, &mut in_r, &mut out_l, &mut out_r);
    assert!(
        post_restart_res.is_ok(),
        "Processing after clean restart must succeed"
    );
}

#[test]
fn test_reset_panic_poisons_processor_and_silences_subsequent_blocks() {
    ensure_isolated_crash_dir();
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let config = audio_config();
    let state_ap = state.clone();

    let stopped = instance
        .activate(move |_, _| make_harness_audio_processor(&state_ap), config)
        .expect("activate");
    let mut started = stopped.start_processing().expect("start_processing");

    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    // Inject panic on reset
    super::TEST_RESET_PANIC_INJECTION.store(true, Ordering::Relaxed);

    // Reset encounters panic, catches it, sets poisoned = true
    started.reset();

    // Verify flag was set
    assert!(
        shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_PROCESSOR_POISONED),
        "RT_STATUS_PROCESSOR_POISONED bit must be set on reset panic"
    );

    // Subsequent process calls are contained and silenced
    let mut in_l = vec![0.5f32; BLOCK];
    let mut in_r = vec![0.5f32; BLOCK];
    let mut out_l = vec![1.0f32; BLOCK];
    let mut out_r = vec![1.0f32; BLOCK];

    let res = process_block(&mut started, &mut in_l, &mut in_r, &mut out_l, &mut out_r);
    assert_eq!(res.unwrap(), ProcessStatus::Continue);

    for &s in &out_l {
        assert_eq!(s, 0.0, "Output L must be silenced after reset panic");
    }
    for &s in &out_r {
        assert_eq!(s, 0.0, "Output R must be silenced after reset panic");
    }

    // Calling reset again when poisoned is a no-op
    started.reset();
}

#[test]
fn test_deactivate_discards_dsp_pipeline_when_poisoned() {
    ensure_isolated_crash_dir();
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let config = audio_config();
    let state_ap = state.clone();

    let stopped = instance
        .activate(move |_, _| make_harness_audio_processor(&state_ap), config)
        .expect("activate");
    let mut started = stopped.start_processing().expect("start_processing");

    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    // Inject panic to poison processor
    super::TEST_PANIC_INJECTION.store(true, Ordering::Relaxed);

    let mut in_l = vec![0.0f32; BLOCK];
    let mut in_r = vec![0.0f32; BLOCK];
    let mut out_l = vec![0.0f32; BLOCK];
    let mut out_r = vec![0.0f32; BLOCK];
    let _ = process_block(&mut started, &mut in_l, &mut in_r, &mut out_l, &mut out_r);

    assert!(
        shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_PROCESSOR_POISONED)
    );

    // Stop and deactivate
    let stopped_proc = started.stop_processing();
    instance.deactivate(stopped_proc);

    // S2-T1 invariant: deactivated_dsp MUST be None when deactivated while poisoned!
    let saved_dsp = shared.cold.deactivated_dsp.lock().unwrap();
    assert!(
        saved_dsp.is_none(),
        "Deactivating a poisoned processor must discard the DSP pipeline (deactivated_dsp must be None)"
    );
}

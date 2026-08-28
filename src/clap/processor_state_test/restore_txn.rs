// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Transactional state restore — stage/commit/ack protocol.
//!
//! Verifies that under SPSC saturation the restore transaction is retained
//! whole (never partially published), UI/paths/hashes/params are published only
//! after the audio thread acks the generation, and a Full restore without a
//! model removes the RT model (does not leave the previous one).

use crate::clap::test_util::{self, StereoTestBuffers, TestHost};
use clack_host::prelude::*;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

const N: usize = 256;
const SR_48K: f64 = 48000.0;

fn audio_config(sample_rate: f64, max_frames: u32) -> PluginAudioConfiguration {
    PluginAudioConfiguration {
        sample_rate,
        min_frames_count: 1,
        max_frames_count: max_frames,
    }
}

fn model_path(name: &str) -> PathBuf {
    crate::clap::test_util::model_path(name)
}

fn process_block(
    started: &mut StartedPluginAudioProcessor<TestHost>,
    bufs: &mut StereoTestBuffers,
) {
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
        .unwrap();
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

fn load_state(instance: &mut PluginInstance<TestHost>, params: &ProcessingParams) {
    let state_ext = test_util::get_state_ext(instance);
    let state_bytes = serde_json::to_vec(params).unwrap();
    let mut handle = instance.plugin_handle();
    state_ext
        .load(&mut handle, &mut state_bytes.as_slice())
        .expect("state.load must succeed");
}

fn try_load_state(
    instance: &mut PluginInstance<TestHost>,
    params: &ProcessingParams,
) -> Result<(), clack_extensions::state::StateError> {
    let state_ext = test_util::get_state_ext(instance);
    let state_bytes = serde_json::to_vec(params).unwrap();
    let mut handle = instance.plugin_handle();
    state_ext.load(&mut handle, &mut state_bytes.as_slice())
}

/// Processes one block (applies any queued transaction) and runs one
/// housekeeping cycle (retries pushes and publishes acked restores).
fn settle(
    started: &mut StartedPluginAudioProcessor<TestHost>,
    main_thread_ptr: *mut crate::clap::plugin::NamClapMainThread<'static>,
    bufs: &mut StereoTestBuffers,
) {
    process_block(started, bufs);
    let mt = unsafe { &mut *main_thread_ptr };
    mt.housekeeping();
}

#[test]
fn test_restore_ui_not_published_until_ack() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let config = audio_config(SR_48K, N as u32);
    let stopped = plugin_instance.activate(|_, _| (), config).unwrap();
    let mut started = stopped.start_processing().unwrap();

    let shared_ptr = test_util::extract_shared(&mut plugin_instance);
    let main_thread_ptr = main_thread_ptr(&mut plugin_instance);
    let shared = unsafe { &*shared_ptr };

    // Saturate the SPSC command ring (capacity 256) so the restore txn cannot
    // be pushed immediately.
    {
        let mt = unsafe { &mut *main_thread_ptr };
        for _ in 0..256 {
            mt.cmd_producer
                .push_command(crate::clap::plugin::ClapParamPayload::LoadCabIr { adapter: None })
                .expect("expected the first 256 pushes to succeed");
        }
    }

    // Load a Full restore (model + params) while the ring is saturated.
    let model = model_path("lstm.nam");
    let params = ProcessingParams {
        model_path: Some(model.clone()),
        model_basename: Some("lstm.nam".to_string()),
        model_hash: crate::clap::test_util::asset_hash(&model),
        input_gain_db: 5.0,
        output_gain_db: -3.0,
        ..Default::default()
    };
    load_state(&mut plugin_instance, &params);

    // Stage phase: the whole transaction is retained; NOTHING is published yet.
    {
        let mt = unsafe { &mut *main_thread_ptr };
        assert!(
            mt.pending_restore.is_some(),
            "restore txn must be retained whole on SPSC full"
        );
    }
    assert_eq!(
        f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed)),
        0.0,
        "params must NOT be published while the restore is pending"
    );
    assert!(
        shared.cold.ui_model_name.lock().unwrap().is_empty(),
        "ui_model_name must NOT be published while the restore is pending"
    );
    assert_eq!(
        shared.cold.model_load_counter.load(Ordering::Relaxed),
        0,
        "model_load_counter must NOT advance while the restore is pending"
    );

    // Drain the 256 saturated commands, then housekeeping retries the
    // transaction push (the ring now has room).
    //
    // Command Budgeting changed the drain rate for structural
    // bursts: at most one structural apply per callback, and same-kind
    // coalescible commands (LoadCabIr here) are superseded at the same rate
    // (1 apply + 1 superseded discard per block). 256 same-kind clears
    // therefore drain in 128 blocks, not 4×64.
    let mut bufs = StereoTestBuffers::new(N, 0.1, 0.1);
    for _ in 0..128 {
        process_block(&mut started, &mut bufs);
    }
    {
        let mt = unsafe { &mut *main_thread_ptr };
        mt.housekeeping();
    }

    // One more block applies the last deferred saturation command; a second
    // block applies the transaction atomically (generation advances). The
    // deferred clear is FIFO-before the txn, so it lands one block earlier.
    process_block(&mut started, &mut bufs);
    process_block(&mut started, &mut bufs);
    let applied_generation = shared.cold.last_applied_generation.load(Ordering::Relaxed);
    assert!(
        applied_generation > 0,
        "audio thread must apply the restore generation"
    );

    // Ack phase: housekeeping publishes the UI now that the ack landed.
    {
        let mt = unsafe { &mut *main_thread_ptr };
        mt.housekeeping();
    }
    assert!(
        unsafe { &*main_thread_ptr }.pending_restore.is_none(),
        "pending restore must be consumed after ack"
    );

    let name = shared.cold.ui_model_name.lock().unwrap();
    assert_eq!(
        name.as_str(),
        "lstm.nam",
        "UI must publish the model name after ack"
    );
    assert_eq!(
        f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed)),
        5.0,
        "params must be published after ack"
    );
    assert_eq!(
        f32::from_bits(shared.ui_to_rt.param_output_gain.load(Ordering::Relaxed)),
        -3.0,
        "output gain must be published after ack"
    );
    assert_eq!(
        shared.cold.model_load_counter.load(Ordering::Relaxed),
        1,
        "model_load_counter must advance after ack"
    );
}

#[test]
fn test_restore_full_clear_removes_rt_model() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let config = audio_config(SR_48K, N as u32);
    let stopped = plugin_instance.activate(|_, _| (), config).unwrap();
    let mut started = stopped.start_processing().unwrap();

    let shared_ptr = test_util::extract_shared(&mut plugin_instance);
    let main_thread_ptr = main_thread_ptr(&mut plugin_instance);
    let shared = unsafe { &*shared_ptr };

    // 1. Load a model via a Full restore.
    let model = model_path("lstm.nam");
    let params = ProcessingParams {
        model_path: Some(model.clone()),
        model_basename: Some("lstm.nam".to_string()),
        model_hash: crate::clap::test_util::asset_hash(&model),
        input_gain_db: 1.0,
        ..Default::default()
    };
    let mut bufs = StereoTestBuffers::new(N, 0.1, 0.1);
    load_state(&mut plugin_instance, &params);
    settle(&mut started, main_thread_ptr, &mut bufs);
    assert_eq!(
        shared.cold.ui_model_name.lock().unwrap().as_str(),
        "lstm.nam",
        "model must be applied and published"
    );

    // 2. Load a Full restore WITHOUT a model (explicit clear).
    let clear_params = ProcessingParams {
        input_gain_db: 2.0,
        output_gain_db: -1.0,
        ..Default::default()
    };
    load_state(&mut plugin_instance, &clear_params);
    settle(&mut started, main_thread_ptr, &mut bufs);

    // UI reflects the clear; params are still published (same transaction).
    assert!(
        shared.cold.ui_model_name.lock().unwrap().is_empty(),
        "Full clear must clear the UI model name"
    );
    assert_eq!(
        shared.cold.model_sample_rate.load(Ordering::Relaxed),
        48000,
        "model_sample_rate must reset on Full clear"
    );
    assert_eq!(
        f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed)),
        2.0,
        "params must still be published for the clear restore"
    );

    // 3. Deactivate: the RT model must be gone — not the previous one.
    let stopped = started.stop_processing();
    plugin_instance.deactivate(stopped);
    let deactivated = shared.cold.deactivated_dsp.lock().unwrap();
    let deactivated = deactivated
        .as_ref()
        .expect("deactivated_dsp must be populated after deactivate");
    assert!(
        deactivated.model_l.is_none(),
        "Full clear must remove the RT model (previous model must not survive)"
    );
}

#[test]
fn test_restore_hash_rejected_keeps_previous_dsp() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let config = audio_config(SR_48K, N as u32);
    let stopped = plugin_instance.activate(|_, _| (), config).unwrap();
    let mut started = stopped.start_processing().unwrap();

    let shared_ptr = test_util::extract_shared(&mut plugin_instance);
    let main_thread_ptr = main_thread_ptr(&mut plugin_instance);
    let shared = unsafe { &*shared_ptr };

    // 1. Load a valid model (with hash) and settle it.
    let model = model_path("lstm.nam");
    let params = ProcessingParams {
        model_path: Some(model.clone()),
        model_basename: Some("lstm.nam".to_string()),
        model_hash: crate::clap::test_util::asset_hash(&model),
        input_gain_db: 1.0,
        ..Default::default()
    };
    let mut bufs = StereoTestBuffers::new(N, 0.1, 0.1);
    load_state(&mut plugin_instance, &params);
    settle(&mut started, main_thread_ptr, &mut bufs);
    assert_eq!(
        shared.cold.ui_model_name.lock().unwrap().as_str(),
        "lstm.nam",
        "model must be applied and published"
    );
    let counter_after_valid = shared.cold.model_load_counter.load(Ordering::Relaxed);
    assert_eq!(counter_after_valid, 1);

    // 2. Restore the same path with the hash omitted → must be rejected
    //    (asset integrity requires a SHA-256 digest in the same cycle).
    let hashless = ProcessingParams {
        model_path: Some(model),
        model_basename: Some("lstm.nam".to_string()),
        model_hash: None,
        input_gain_db: 42.0,
        output_gain_db: 13.0,
        ..Default::default()
    };
    assert!(
        try_load_state(&mut plugin_instance, &hashless).is_err(),
        "hashless restore must fail explicitly"
    );

    // 3. The previous DSP/UI/params are fully intact.
    settle(&mut started, main_thread_ptr, &mut bufs);
    assert_eq!(
        shared.cold.ui_model_name.lock().unwrap().as_str(),
        "lstm.nam",
        "UI must keep the previously restored model after a rejected restore"
    );
    assert_eq!(
        shared.cold.model_load_counter.load(Ordering::Relaxed),
        counter_after_valid,
        "model_load_counter must not advance on a rejected restore"
    );
    assert_eq!(
        f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed)),
        1.0,
        "input gain must keep the previously restored value"
    );
}

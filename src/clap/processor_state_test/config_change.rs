// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Transactional restore across host audio reconfiguration tests (D-14 / E-09 / S03.2).
//!
//! Verifies that a restore transaction staged under a Strict Restart Policy
//! (e.g. at 48 kHz / 512 frames) is NOT discarded when the host restarts with a
//! renegotiated audio configuration (e.g. 44.1 kHz / 256 frames). The DSP
//! pipeline components (polyphase resampler, streaming adapter, model buffer sizes)
//! are reconstructed off-RT during `activate()`, preserving the entire atomic
//! transaction (model, IR, parameters, and metadata).

use crate::clap::test_util::{self, StereoTestBuffers, TestHost};
use clack_host::prelude::*;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

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

fn process_test_block(
    started: &mut StartedPluginAudioProcessor<TestHost>,
    bufs: &mut StereoTestBuffers,
) {
    let mut in_ch = [bufs.in_l.as_mut_slice(), bufs.in_r.as_mut_slice()];
    let in_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(in_ch.iter_mut().map(InputChannel::constant)),
    }]);
    let out_ch = [bufs.out_l.as_mut_slice(), bufs.out_r.as_mut_slice()];
    let mut out_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
    }]);
    let input_events = InputEvents::empty();
    let mut out_ev = OutputEvents::from_buffer(&mut bufs.output_events_buffer);
    started
        .process(
            &in_audio,
            &mut out_audio,
            &input_events,
            &mut out_ev,
            None,
            None,
        )
        .unwrap();
}

fn load_state(instance: &mut PluginInstance<TestHost>, params: &ProcessingParams) {
    let state_ext = test_util::get_state_ext(instance);
    let state_bytes = serde_json::to_vec(params).unwrap();
    let handle = instance.plugin_handle();
    state_ext
        .load(&handle, &mut state_bytes.as_slice())
        .expect("state_ext.load failed");
}

#[test]
fn test_staged_restore_reconstructs_and_applies_on_rate_and_buffer_reconfiguration() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    // 1. Initial activation: 48 kHz, 512 frames
    let initial_config = audio_config(48000.0, 512);
    let stopped = plugin_instance
        .activate(|_, _| (), initial_config)
        .expect("initial activate failed");
    let mut started = stopped.start_processing().unwrap();

    let shared_ptr = test_util::extract_shared(&mut plugin_instance);
    let shared = unsafe { &*shared_ptr };

    let raw_ptr = plugin_instance.plugin_handle().as_raw_ptr();
    let mt_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |w| Ok(w.main_thread() as *const crate::clap::plugin::NamClapMainThread<'static>),
        )
        .unwrap()
    };
    let main_thread = unsafe { &*mt_ptr };

    // Process initial block
    let mut bufs_512 = StereoTestBuffers::new(512, 0.1, 0.1);
    process_test_block(&mut started, &mut bufs_512);

    // 2. Perform state restore requesting a new model + oversampling 2x (Strict Restart Policy)
    let model = model_path("lstm.nam");
    let mut params = ProcessingParams::default();
    params.model_path = Some(model.clone());
    params.model_basename = Some("lstm.nam".to_string());
    params.model_hash = crate::clap::test_util::asset_hash(&model);
    params.oversample = OversampleFactor::X2;
    params.input_gain_db = 3.5;
    params.output_gain_db = -1.5;

    load_state(&mut plugin_instance, &params);

    // Verify restore is staged on the main thread awaiting restart
    let staged_gen = {
        let staged_borrow = main_thread.staged_restore.borrow();
        assert!(
            staged_borrow.is_some(),
            "Restore changing latency/oversample must be staged in staged_restore"
        );
        let staged = staged_borrow.as_ref().unwrap();
        assert_eq!(
            staged.txn.model.as_ref().unwrap().new_stream.host_rate(),
            48000,
            "Staged stream was built for initial 48 kHz"
        );
        assert_eq!(
            staged.txn.model.as_ref().unwrap().new_stream.max_block(),
            512,
            "Staged stream was built for initial 512 frames"
        );
        staged.txn.generation
    };

    // 3. Host initiates restart: stops processing and deactivates
    let stopped = started.stop_processing();
    plugin_instance.deactivate(stopped);

    // 4. Host reactivates with a CHANGED audio configuration: 44.1 kHz, 256 frames (D-14)
    let reconfig = audio_config(44100.0, 256);
    let stopped_new = plugin_instance
        .activate(|_, _| (), reconfig)
        .expect("activate with reconfigured audio parameters must succeed");

    // 5. Verify that staged_restore was NOT discarded:
    //    The model and parameters must be fully applied and published.
    assert_eq!(
        shared.cold.ui_model_name.lock().unwrap().as_str(),
        "lstm.nam",
        "Restored model must NOT be discarded across audio reconfiguration"
    );
    assert_eq!(
        shared.cold.last_applied_generation.load(Ordering::Relaxed),
        staged_gen,
        "Last applied generation must match the staged restore transaction"
    );
    assert_eq!(
        main_thread.params.borrow().input_gain_db,
        3.5,
        "Restored input gain must be adopted by main thread params"
    );
    assert_eq!(
        main_thread.params.borrow().output_gain_db,
        -1.5,
        "Restored output gain must be adopted by main thread params"
    );

    // 6. Verify that the audio processor's active streaming adapter and resampler
    //    are reconstructed for the new 44.1 kHz / 256 frames configuration.
    let proc_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |w| Ok(w.audio_processor().unwrap().as_ptr()),
        )
        .unwrap()
    };
    let processor = unsafe { &*proc_ptr };
    assert_eq!(
        processor.stream.host_rate(),
        44100,
        "Active streaming adapter must be reconstructed for 44.1 kHz"
    );
    assert_eq!(
        processor.stream.max_block(),
        256,
        "Active streaming adapter must be reconstructed for 256 frames"
    );
    assert_eq!(
        processor.resampler.host_rate(),
        44100,
        "Active resampler must be reconstructed for 44.1 kHz"
    );
    assert_eq!(
        processor.params.input_gain_db, 3.5,
        "Processor params must match restored input gain"
    );

    // 7. Start processing and process audio at 44.1 kHz / 256 frames to prove validity
    let mut started_new = stopped_new.start_processing().unwrap();
    let mut bufs_256 = StereoTestBuffers::new(256, 0.2, 0.2);
    process_test_block(&mut started_new, &mut bufs_256);

    // Verify output is non-zero, finite
    let sum: f32 = bufs_256.out_l.iter().map(|s| s.abs()).sum();
    assert!(sum > 0.0, "Processed audio output must be active");
    assert!(
        bufs_256.out_l.iter().all(|s| s.is_finite()),
        "Processed audio must be finite"
    );
}

#[test]
fn test_staged_restore_full_clear_adapts_on_reconfiguration() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    // Initial activation: 48 kHz, 512 frames
    let initial_config = audio_config(48000.0, 512);
    let stopped = plugin_instance.activate(|_, _| (), initial_config).unwrap();
    let started = stopped.start_processing().unwrap();

    let raw_ptr = plugin_instance.plugin_handle().as_raw_ptr();
    let mt_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |w| Ok(w.main_thread() as *const crate::clap::plugin::NamClapMainThread<'static>),
        )
        .unwrap()
    };
    let main_thread = unsafe { &*mt_ptr };

    // Full restore without model, requesting Oversample 4x (forces Strict Restart Policy)
    let mut params = ProcessingParams::default();
    params.model_path = None;
    params.model_basename = None;
    params.model_hash = None;
    params.oversample = OversampleFactor::X4;

    load_state(&mut plugin_instance, &params);

    assert!(
        main_thread.staged_restore.borrow().is_some(),
        "Clear restore with oversample change must be staged"
    );

    // Host restarts at 44.1 kHz / 128 frames
    let stopped = started.stop_processing();
    plugin_instance.deactivate(stopped);

    let stopped_new = plugin_instance
        .activate(|_, _| (), audio_config(44100.0, 128))
        .expect("activate on clear restore reconfig must succeed");

    let proc_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |w| Ok(w.audio_processor().unwrap().as_ptr()),
        )
        .unwrap()
    };
    let processor = unsafe { &*proc_ptr };
    assert!(processor.model_l.is_none(), "Model must be cleared");
    assert_eq!(processor.stream.host_rate(), 44100);
    assert_eq!(processor.stream.max_block(), 128);
    assert_eq!(processor.resampler.host_rate(), 44100);

    let mut started_new = stopped_new.start_processing().unwrap();
    let mut bufs = StereoTestBuffers::new(128, 0.1, 0.1);
    process_test_block(&mut started_new, &mut bufs);
}

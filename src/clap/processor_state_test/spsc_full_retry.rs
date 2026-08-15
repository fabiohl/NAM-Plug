// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! S4-T2 (R-10): SPSC saturation must not drop a validated model nor advance
//! the UI ahead of the DSP. Filling the command ring and attempting a model
//! load must retain the model in `pending_model` for retry.

use crate::clap::plugin::ClapParamPayload;
use crate::clap::test_util;
use clack_host::process::PluginAudioConfiguration;
use std::sync::atomic::Ordering;

#[test]
fn test_model_retained_on_spsc_full_no_ui_desync() {
    let model_path = crate::clap::test_util::model_path("lstm.nam");
    assert!(model_path.exists(), "lstm.nam fixture missing");

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    // Activate first so buffer_size > 0 and the command ring is live.
    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };
    let _stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();

    // Access the main thread to drive load_model directly.
    let raw_ptr = plugin_instance.plugin_handle().as_raw_ptr();
    let main_thread_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |w| Ok(w.main_thread().as_ptr()),
        )
        .unwrap()
    };
    let mt = unsafe { &mut *main_thread_ptr };

    // Saturate the SPSC command ring (capacity 256).
    for _ in 0..256 {
        mt.cmd_producer
            .push_command(ClapParamPayload::LoadCabIr { adapter: None })
            .expect("expected the first 256 pushes to succeed");
    }

    let counter_before = mt.shared.cold.model_load_counter.load(Ordering::Relaxed);

    // On Full, load_model must defer (retain the model) rather than fail —
    // and must NOT advance the UI state.
    mt.load_model(&model_path)
        .expect("load_model should defer on SPSC full, not return an error");

    let pending_is_some = mt.shared.cold.pending_model.lock().unwrap().is_some();
    assert!(
        pending_is_some,
        "model must be retained in pending_model on SPSC full"
    );

    let counter_after = mt.shared.cold.model_load_counter.load(Ordering::Relaxed);
    assert_eq!(
        counter_before, counter_after,
        "model_load_counter must not advance on SPSC full"
    );

    let name = mt.shared.cold.ui_model_name.lock().unwrap();
    assert!(
        name.is_empty(),
        "ui_model_name must not be set on SPSC full (UI must not get ahead of DSP)"
    );
}

#[test]
fn test_cabsim_retained_on_spsc_full_no_ui_desync() {
    let ir_path = std::env::temp_dir().join("nam_plug_spsc_full_ir.wav");
    let samples: Vec<f32> = (0..64).map(|i| (i as f32 * 0.1).sin()).collect();
    neural_amp_modeler_rs::testing::wav::write_wav_f32(&ir_path, &samples, 48000)
        .expect("failed to write synthetic IR WAV");

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };
    let _stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();

    let raw_ptr = plugin_instance.plugin_handle().as_raw_ptr();
    let main_thread_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |w| Ok(w.main_thread().as_ptr()),
        )
        .unwrap()
    };
    let mt = unsafe { &mut *main_thread_ptr };

    for _ in 0..256 {
        mt.cmd_producer
            .push_command(ClapParamPayload::LoadCabIr { adapter: None })
            .expect("expected the first 256 pushes to succeed");
    }

    mt.load_cabsim(&ir_path)
        .expect("load_cabsim should defer on SPSC full, not return an error");

    let pending = mt.shared.cold.ui_pending_ir.lock().unwrap().clone();
    assert_eq!(
        pending.as_deref(),
        Some(ir_path.as_path()),
        "IR path must be retained in ui_pending_ir on SPSC full"
    );

    let ir_committed = mt.shared.cold.ir_path.lock().unwrap().clone();
    assert!(
        ir_committed.is_none(),
        "ir_path must not be committed on SPSC full"
    );

    let raw_committed = mt.shared.cold.ir_raw_samples.lock().unwrap().is_some();
    assert!(
        !raw_committed,
        "ir_raw_samples must not be committed on SPSC full"
    );
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Host tail notification tests across restart cycles (D-17 / E-09 / S03.3).
//!
//! Verifies that when a CabSim IR is installed or cleared via restart (e.g. staged
//! swap or transactional restore), the host receives exactly one `HostTail::changed()`
//! notification on the Audio Thread. Subsequent processing blocks and restarts
//! without tail changes do not emit redundant notifications.

use crate::clap::host_harness::{
    CompleteHost, extract_plugin_main_thread, extract_plugin_shared, make_harness_audio_processor,
    make_test_plugin_with_harness, perform_restart,
};
use crate::clap::test_util::StereoTestBuffers;
use clack_extensions::state::PluginState;
use clack_host::prelude::*;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

fn audio_config() -> PluginAudioConfiguration {
    PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 1,
        max_frames_count: 256,
    }
}

fn write_test_ir(len: usize, name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("nam_plug_tail_test_{name}.wav"));
    let mut ir = vec![0.0f32; len];
    ir[0] = 1.0;
    ir[1] = -0.9;
    neural_amp_modeler_rs::testing::wav::write_wav_f32(&path, &ir, 48000)
        .expect("write test IR wav");
    path
}

fn process_harness_block(
    started: &mut StartedPluginAudioProcessor<CompleteHost>,
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

#[test]
fn test_tail_notification_staged_ir_install_and_clear_across_restart() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    // 1. Initial activation with no IR
    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let mut started = stopped.start_processing().expect("start_processing");

    let mut bufs = StereoTestBuffers::new(256, 0.1, 0.1);

    // Initial steady-state processing: tail is 0, no notifications sent
    for _ in 0..3 {
        process_harness_block(&mut started, &mut bufs);
    }
    assert_eq!(
        state.tail_changed_count.load(Ordering::SeqCst),
        0,
        "No tail notification should be sent on initial empty state"
    );

    // 2. Stage an IR restore requiring host restart (0 -> partition latency)
    let ir_path = write_test_ir(512, "tail_notif_ir");
    let mut params = ProcessingParams::default();
    params.ir_path = Some(ir_path.clone());
    params.ir_hash = crate::clap::test_util::asset_hash(&ir_path);

    let state_ext = instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("State extension not found");
    let state_bytes = serde_json::to_vec(&params).unwrap();
    state_ext
        .load(&instance.plugin_handle(), &mut state_bytes.as_slice())
        .expect("state load failed");

    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "IR restore must request restart"
    );

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &*mt_ptr };
    assert!(
        mt.staged_restore.borrow().is_some(),
        "IR restore must be staged"
    );

    // 3. Perform restart to install the staged IR
    let mut started = perform_restart(&mut instance, started, &state, audio_config());

    // Immediately after activate/start_processing, tail hasn't been notified yet
    // because notification runs in the audio callback context (CLAP specification)
    assert_eq!(
        state.tail_changed_count.load(Ordering::SeqCst),
        0,
        "Tail notification must NOT fire before the audio thread processes"
    );

    // Process first audio block after restart: tail change MUST be notified!
    process_harness_block(&mut started, &mut bufs);
    assert_eq!(
        state.tail_changed_count.load(Ordering::SeqCst),
        1,
        "HostTail::changed must be called exactly once after IR installed via restart"
    );
    assert!(
        shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed) > 0,
        "CabSim tail samples must be active"
    );

    // Process subsequent blocks: notification count must remain 1 (no duplicate spamming)
    for _ in 0..5 {
        process_harness_block(&mut started, &mut bufs);
    }
    assert_eq!(
        state.tail_changed_count.load(Ordering::SeqCst),
        1,
        "No redundant tail notifications should fire during steady state"
    );

    // 4. Clear IR via restore requiring host restart
    state.restart_requested.store(false, Ordering::SeqCst);
    let mut clear_params = ProcessingParams::default();
    clear_params.ir_path = None;
    clear_params.ir_hash = None;

    let clear_bytes = serde_json::to_vec(&clear_params).unwrap();
    state_ext
        .load(&instance.plugin_handle(), &mut clear_bytes.as_slice())
        .expect("state load clear failed");

    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "IR clear restore must request restart"
    );

    // 5. Perform restart to clear the IR
    let mut started = perform_restart(&mut instance, started, &state, audio_config());
    assert_eq!(
        state.tail_changed_count.load(Ordering::SeqCst),
        1,
        "Tail count remains 1 until first audio block after clear restart"
    );

    // Process first block after clear restart: notification count increments to 2!
    process_harness_block(&mut started, &mut bufs);
    assert_eq!(
        state.tail_changed_count.load(Ordering::SeqCst),
        2,
        "HostTail::changed must be called when IR is cleared via restart"
    );
    assert_eq!(
        shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed),
        0,
        "CabSim tail samples must be 0 after clear"
    );

    // Subsequent blocks: count remains 2
    for _ in 0..5 {
        process_harness_block(&mut started, &mut bufs);
    }
    assert_eq!(
        state.tail_changed_count.load(Ordering::SeqCst),
        2,
        "Tail notifications must remain exactly once per transition"
    );

    let _ = std::fs::remove_file(&ir_path);
}

#[test]
fn test_tail_notification_restart_without_tail_change_emits_no_notification() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let mut started = stopped.start_processing().expect("start_processing");

    let mut bufs = StereoTestBuffers::new(256, 0.1, 0.1);
    for _ in 0..3 {
        process_harness_block(&mut started, &mut bufs);
    }
    assert_eq!(state.tail_changed_count.load(Ordering::SeqCst), 0);

    // Restart without changing IR (e.g. quantum change or host reload)
    let mut started = perform_restart(&mut instance, started, &state, audio_config());

    // Process blocks: tail is still 0, so no tail change notification is emitted
    for _ in 0..3 {
        process_harness_block(&mut started, &mut bufs);
    }
    assert_eq!(
        state.tail_changed_count.load(Ordering::SeqCst),
        0,
        "Restart without tail change must not emit spurious notifications"
    );
}

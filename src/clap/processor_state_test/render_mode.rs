// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::test_util::{self, MonoTestBuffers};
use clack_host::prelude::*;

use std::sync::atomic::Ordering;

#[test]
fn test_render_mode_transitions() {
    use clack_extensions::render::{PluginRender, RenderMode};

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    let render_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginRender>()
        .expect("PluginRender extension not found");

    assert!(
        !render_ext.has_realtime_requirement(&mut plugin_instance.plugin_handle()),
        "NAM should not have hard realtime requirement"
    );

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };

    let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started_processor = stopped_processor.start_processing().unwrap();

    let render_mode = shared.cold.render_mode.load(Ordering::Acquire);
    assert_eq!(
        render_mode,
        crate::clap::plugin::RENDER_MODE_REALTIME,
        "should start in realtime mode"
    );

    let mut handle = plugin_instance.plugin_handle();
    render_ext
        .set(&mut handle, RenderMode::Offline)
        .expect("set RenderMode::Offline should succeed");

    let render_mode = shared.cold.render_mode.load(Ordering::Acquire);
    assert_eq!(
        render_mode,
        crate::clap::plugin::RENDER_MODE_OFFLINE,
        "render_mode should be OFFLINE after set"
    );

    let n = 512;
    let mut bufs = MonoTestBuffers::new(n, 0.1);
    let mut input_channels = [bufs.in_buf.as_mut_slice()];
    let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(
            input_channels.iter_mut().map(InputChannel::constant),
        ),
    }]);
    let output_channels = [bufs.out_buf.as_mut_slice()];
    let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
    }]);
    let input_events = InputEvents::empty();
    let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);

    // Process a few blocks in offline mode — degradation flags should stay clear
    for _ in 0..4 {
        started_processor
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .expect("process should succeed in offline mode");
    }

    assert!(
        !shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_REDUCED),
        "DEGRADE_REDUCED should be clear in offline mode"
    );
    assert!(
        !shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_MINIMAL),
        "DEGRADE_MINIMAL should be clear in offline mode"
    );

    let mut handle = plugin_instance.plugin_handle();
    render_ext
        .set(&mut handle, RenderMode::Realtime)
        .expect("set RenderMode::Realtime should succeed");

    let render_mode = shared.cold.render_mode.load(Ordering::Acquire);
    assert_eq!(
        render_mode,
        crate::clap::plugin::RENDER_MODE_REALTIME,
        "render_mode should be back to REALTIME"
    );

    // Process blocks in realtime mode
    for _ in 0..2 {
        started_processor
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .expect("process should succeed in realtime mode");
    }

    // Verify output is not silent (bypass off by default)
    assert!(
        bufs.out_buf.iter().any(|s| *s != 0.0),
        "Output should not be silent (bypass is off)"
    );
}

#[test]
fn test_offline_mode_forces_adaptive_off() {
    use clack_extensions::render::{PluginRender, RenderMode};

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    let render_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginRender>()
        .expect("PluginRender extension not found");

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };

    let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started_processor = stopped_processor.start_processing().unwrap();

    // 1. Configure AdaptiveCompute to Aggressive in Realtime mode
    shared.ui_to_rt.param_adaptive_compute.store(
        neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Aggressive as u32,
        Ordering::Relaxed,
    );
    shared.bump_generation();

    shared
        .rt_to_ui
        .active_channel_count
        .store(1, Ordering::Relaxed);

    let n = 512;
    let mut bufs = MonoTestBuffers::new(n, 0.1);
    let mut input_channels = [bufs.in_buf.as_mut_slice()];
    let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(
            input_channels.iter_mut().map(InputChannel::constant),
        ),
    }]);
    let output_channels = [bufs.out_buf.as_mut_slice()];
    let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
    }]);
    let input_events = InputEvents::empty();
    let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);

    // Process a block to apply/sync parameter changes
    started_processor
        .process(
            &input_audio,
            &mut output_audio,
            &input_events,
            &mut output_events,
            None,
            None,
        )
        .unwrap();

    // 2. Manually set degradation flags to simulate an overload that occurred previously
    shared
        .cold
        .rt_status
        .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_REDUCED);
    shared
        .cold
        .rt_status
        .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_MINIMAL);
    assert!(
        shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_REDUCED)
    );
    assert!(
        shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_MINIMAL)
    );

    // 3. Set RenderMode::Offline
    let mut handle = plugin_instance.plugin_handle();
    render_ext
        .set(&mut handle, RenderMode::Offline)
        .expect("set RenderMode::Offline should succeed");

    // 4. Process a block in Offline mode
    started_processor
        .process(
            &input_audio,
            &mut output_audio,
            &input_events,
            &mut output_events,
            None,
            None,
        )
        .unwrap();

    // 5. Verify that degradation flags are CLEARED in Offline mode
    assert!(
        !shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_REDUCED),
        "DEGRADE_REDUCED should be cleared in offline mode"
    );
    assert!(
        !shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_MINIMAL),
        "DEGRADE_MINIMAL should be cleared in offline mode"
    );

    // 6. Try to change AdaptiveCompute to Conservative while offline
    shared.ui_to_rt.param_adaptive_compute.store(
        neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Conservative as u32,
        Ordering::Relaxed,
    );
    shared.bump_generation();

    // Set degradation flags again manually to see if they are cleared on next block
    shared
        .cold
        .rt_status
        .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_REDUCED);
    shared
        .cold
        .rt_status
        .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_MINIMAL);

    started_processor
        .process(
            &input_audio,
            &mut output_audio,
            &input_events,
            &mut output_events,
            None,
            None,
        )
        .unwrap();

    // Flags must have been cleared again, because AdaptiveCompute is immediately forced to Off when offline
    assert!(
        !shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_REDUCED),
        "DEGRADE_REDUCED must be immediately cleared when attempting parameter changes offline"
    );
    assert!(
        !shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_MINIMAL),
        "DEGRADE_MINIMAL must be immediately cleared when attempting parameter changes offline"
    );
}

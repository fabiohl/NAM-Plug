// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::test_util::{self, MonoTestBuffers};
use clack_host::prelude::*;
use clack_host::process::PluginAudioConfiguration;
use neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED;
use std::sync::atomic::Ordering;

#[test]
fn test_model_load_failed_edge_detection() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 64,
        max_frames_count: 64,
    };

    let raw_ptr = plugin_instance.plugin_handle().as_raw_ptr();
    let main_thread_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |w| Ok(w.main_thread().as_ptr()),
        )
        .unwrap()
    };
    let mt = unsafe { &mut *main_thread_ptr };

    let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started_processor = stopped_processor.start_processing().unwrap();

    let n = 64;
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

    // Process first block with no model loaded and bypass = false (default)
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

    // Edge detection sets the flag on transition to fail
    assert!(
        mt.shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_MODEL_LOAD_FAILED),
        "RT_STATUS_MODEL_LOAD_FAILED should be set when model_l is None and bypass is false"
    );

    // Process a second block: edge detection should keep the flag set without oscillating
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
    assert!(
        mt.shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_MODEL_LOAD_FAILED),
        "RT_STATUS_MODEL_LOAD_FAILED should remain set on subsequent blocks"
    );

    // Set bypass to true on ui_to_rt
    mt.shared.ui_to_rt.param_bypass.store(1, Ordering::Relaxed);
    mt.shared
        .ui_to_rt
        .gui_param_generation
        .fetch_add(1, Ordering::Release);

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
    assert!(
        !mt.shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_MODEL_LOAD_FAILED),
        "RT_STATUS_MODEL_LOAD_FAILED should be cleared when bypass is enabled"
    );
}

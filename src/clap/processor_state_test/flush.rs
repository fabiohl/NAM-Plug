// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::test_util::{self, StereoTestBuffers};
use clack_host::prelude::*;
use std::sync::atomic::Ordering;

#[test]
fn test_audio_processor_flush() {
    use crate::clap::extensions::params::PARAM_INPUT_GAIN;
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::{ClapId, Cookie};
    use clack_extensions::params::PluginAudioProcessorParams;

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };

    // Activate the plugin to instantiate the audio processor.
    let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();

    let raw_ptr = plugin_instance.plugin_handle().as_raw_ptr();
    let processor_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |wrapper| {
                let ptr = wrapper.audio_processor().unwrap().as_ptr();
                Ok(ptr)
            },
        )
        .unwrap()
    };
    let processor = unsafe { &mut *processor_ptr };

    // Ensure initially generation is 0
    let initial_gen = processor
        .shared
        .ui_to_rt
        .gui_param_generation
        .load(Ordering::Relaxed);

    // Prepare parameter events for flush: set input gain to 10.5 dB
    let mut input_events_buffer = EventBuffer::new();
    let event = ParamValueEvent::new(
        0,
        ClapId::new(PARAM_INPUT_GAIN),
        Pckn::match_all(),
        10.5f64,
        Cookie::empty(),
    );
    input_events_buffer.push(&event);
    let input_events = InputEvents::from_buffer(&input_events_buffer);

    let mut output_events_buffer = EventBuffer::new();
    let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

    // Call flush() on the audio processor
    processor.flush(&input_events, &mut output_events);

    // 1. Verify that the parameter target is updated
    assert_eq!(processor.params.input_gain_db, 10.5);
    let target_linear = processor.gain_lut.db_to_linear(10.5);
    assert!((processor.smoother_in.target_value() - target_linear).abs() < 1e-5);

    // 2. Verify that ui_to_rt param atomic is updated
    let stored_db = f32::from_bits(
        processor
            .shared
            .ui_to_rt
            .param_input_gain
            .load(Ordering::Relaxed),
    );
    assert_eq!(stored_db, 10.5);

    // 3. Verify that the generation counter was bumped
    let current_gen = processor
        .shared
        .ui_to_rt
        .gui_param_generation
        .load(Ordering::Relaxed);
    assert!(
        current_gen > initial_gen,
        "Generation counter should have been bumped"
    );

    // Start processing (calls process_events internally, which should sync generation)
    let mut started_processor = stopped_processor.start_processing().unwrap();
    let n = 512;
    let mut bufs = StereoTestBuffers::new(n, 0.1, 0.2);
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

    // After process(), the processor's last_seen_generation should match current_gen
    let processor_ptr_after = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |wrapper| {
                let ptr = wrapper.audio_processor().unwrap().as_ptr();
                Ok(ptr)
            },
        )
        .unwrap()
    };
    let processor_after = unsafe { &mut *processor_ptr_after };
    assert_eq!(processor_after.last_seen_generation, current_gen);
}

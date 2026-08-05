// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::plugin::command_scheduler::CMD_QUEUE_CAPACITY;
    use crate::clap::plugin::shared::ClapParamPayload;
    use crate::clap::test_util;
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::common::params::RtProcessingParams;
    use neural_amp_modeler_rs::common::spsc::RT_STATUS_SPSC_DRAIN_TRUNCATED;
    use rtrb::RingBuffer;

    #[test]
    fn test_spsc_drain_truncation_warning_emitted() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // Create a fresh SPSC channel pair and populate it with 70 commands before activation.
        let (mut tx, rx) = RingBuffer::new(CMD_QUEUE_CAPACITY);
        for i in 0..70 {
            let params = RtProcessingParams {
                input_gain_db: i as f32 * 0.1,
                ..Default::default()
            };
            let _ = tx.push(ClapParamPayload::Params(params));
        }

        // Install our custom SPSC pair into shared.cold so processor extracts rx during activate()
        *shared.cold.param_tx.lock().unwrap() = Some(tx);
        *shared.cold.param_rx.lock().unwrap() = Some(rx);

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();

        let mut started_processor = stopped_processor.start_processing().unwrap();

        let n = 512;
        let mut mono_bufs = test_util::MonoTestBuffers::new(n, 0.0);
        let mut input_channels = [mono_bufs.in_buf.as_mut_slice()];
        let input_audio = mono_bufs.input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input_channels.iter_mut().map(InputChannel::constant),
            ),
        }]);

        let output_channels = [mono_bufs.out_buf.as_mut_slice()];
        let mut output_audio = mono_bufs
            .output_ports
            .with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
            }]);

        let input_events = InputEvents::empty();
        let mut output_events = OutputEvents::from_buffer(&mut mono_bufs.output_events_buffer);

        // Process audio block — process_events() will drain 64 events and hit the truncation limit,
        // setting RT_STATUS_SPSC_DRAIN_TRUNCATED flag.
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

        // Verify that the atomic flag was set on rt_status
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_SPSC_DRAIN_TRUNCATED),
            "RT_STATUS_SPSC_DRAIN_TRUNCATED flag should be set when SPSC drain exceeds 64 items"
        );

        // Deactivate processor to return main thread access
        let stopped = started_processor.stop_processing();
        plugin_instance.deactivate(stopped);

        // Call on_main_thread callback to trigger emit_pending_logs()
        plugin_instance.call_on_main_thread_callback();

        // Confirm that the warning message was logged to LogBuffer
        test_util::assert_log_buffer_contains(
            "SPSC command queue drain limit reached (64 events) - pending events deferred",
        );
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::params::{
        PARAM_BYPASS, PARAM_GATE_THRESH, PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN,
    };
    use crate::clap::plugin::command_scheduler::CMD_QUEUE_CAPACITY;
    use crate::clap::plugin::shared::ClapParamPayload;
    use crate::clap::test_util;
    use clack_host::events::event_types::ParamValueEvent;
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::common::params::RtProcessingParams;
    use neural_amp_modeler_rs::common::spsc::{
        RT_STATUS_EVENT_TIMING_ANOMALY, RT_STATUS_SPSC_DRAIN_TRUNCATED,
    };
    use rtrb::RingBuffer;
    use std::sync::atomic::Ordering;

    #[test]
    fn test_spsc_drain_truncation_warning_emitted() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // Create a fresh SPSC channel pair and populate it with 70 commands before activation.
        let (mut tx, rx) = RingBuffer::new(CMD_QUEUE_CAPACITY);
        for i in 0..70 {
            let params = RtProcessingParams::default().with_input_gain_db(i as f32 * 0.1);
            let _ = tx.push(Box::new(ClapParamPayload::Params(params)));
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
            "Event queue saturation: SPSC drain limit (64) or input event budget (4096) exceeded",
        );
    }

    #[test]
    fn test_out_of_order_events_clamped_and_flag_set() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let mut bufs = test_util::StereoTestBuffers::new(512, 0.0, 0.0);

        // Host provides events out-of-order: t=200 then t=50 then t=120
        let mut event_buffer = EventBuffer::new();
        let ev1 = ParamValueEvent::new(200, ClapId::new(PARAM_INPUT_GAIN), Pckn::match_all(), 2.5);
        let ev2 = ParamValueEvent::new(50, ClapId::new(PARAM_OUTPUT_GAIN), Pckn::match_all(), -3.0);
        let ev3 = ParamValueEvent::new(
            120,
            ClapId::new(PARAM_GATE_THRESH),
            Pckn::match_all(),
            -60.0,
        );
        event_buffer.push(&ev1);
        event_buffer.push(&ev2);
        event_buffer.push(&ev3);
        let input_events = InputEvents::from_buffer(&event_buffer);

        test_util::process_stereo_block_prealloc(
            &mut started_processor,
            &mut bufs,
            Some(&input_events),
        );

        // 1. RT flag must be asserted
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "RT_STATUS_EVENT_TIMING_ANOMALY must be set when out-of-order events arrive"
        );

        // 2. Zero event loss: all parameter changes applied to state
        let gain_in = f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed));
        let gain_out = f32::from_bits(shared.ui_to_rt.param_output_gain.load(Ordering::Relaxed));
        let gate_thresh = f32::from_bits(shared.ui_to_rt.param_gate_thresh.load(Ordering::Relaxed));
        assert_eq!(
            gain_in, 2.5,
            "Input gain must be applied despite out-of-order timing"
        );
        assert_eq!(
            gain_out, -3.0,
            "Output gain must be applied despite out-of-order timing"
        );
        assert_eq!(
            gate_thresh, -60.0,
            "Gate thresh must be applied despite out-of-order timing"
        );

        // 3. Deactivate and trigger off-RT log emission
        let stopped = started_processor.stop_processing();
        plugin_instance.deactivate(stopped);

        plugin_instance.call_on_main_thread_callback();

        // 4. Flag cleared and warning logged
        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "RT_STATUS_EVENT_TIMING_ANOMALY must be cleared after emit_pending_logs()"
        );
        test_util::assert_log_buffer_contains("Parameter event timing anomaly detected");
    }

    #[test]
    fn test_events_beyond_n_samples_clamped_at_block_end_and_flag_set() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let mut bufs = test_util::StereoTestBuffers::new(512, 0.0, 0.0);

        // Host provides events with time >= n_samples (512): t=600 and t=1024
        let mut event_buffer = EventBuffer::new();
        let ev1 = ParamValueEvent::new(600, ClapId::new(PARAM_INPUT_GAIN), Pckn::match_all(), 4.0);
        let ev2 = ParamValueEvent::new(
            1024,
            ClapId::new(PARAM_OUTPUT_GAIN),
            Pckn::match_all(),
            -6.0,
        );
        event_buffer.push(&ev1);
        event_buffer.push(&ev2);
        let input_events = InputEvents::from_buffer(&event_buffer);

        test_util::process_stereo_block_prealloc(
            &mut started_processor,
            &mut bufs,
            Some(&input_events),
        );

        // 1. RT flag asserted
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "RT_STATUS_EVENT_TIMING_ANOMALY must be set when events with time >= n_samples arrive"
        );

        // 2. Zero event loss: all events clamped to block close and applied
        let gain_in = f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed));
        let gain_out = f32::from_bits(shared.ui_to_rt.param_output_gain.load(Ordering::Relaxed));
        assert_eq!(gain_in, 4.0, "Input gain must be applied at block end");
        assert_eq!(gain_out, -6.0, "Output gain must be applied at block end");

        // 3. Deactivate and check warning
        let stopped = started_processor.stop_processing();
        plugin_instance.deactivate(stopped);

        plugin_instance.call_on_main_thread_callback();

        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY)
        );
        test_util::assert_log_buffer_contains("Parameter event timing anomaly detected");
    }

    #[test]
    fn test_event_at_exact_boundary_clamped_and_flag_set() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let mut bufs = test_util::StereoTestBuffers::new(512, 0.0, 0.0);

        // Event at exact boundary t = 512 (valid range in CLAP is 0..512, so 512 is out of range)
        let mut event_buffer = EventBuffer::new();
        let ev = ParamValueEvent::new(512, ClapId::new(PARAM_BYPASS), Pckn::match_all(), 1.0);
        event_buffer.push(&ev);
        let input_events = InputEvents::from_buffer(&event_buffer);

        test_util::process_stereo_block_prealloc(
            &mut started_processor,
            &mut bufs,
            Some(&input_events),
        );

        assert!(
            shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "RT_STATUS_EVENT_TIMING_ANOMALY must be set for event at t == n_samples"
        );
        assert_eq!(
            shared.ui_to_rt.param_bypass.load(Ordering::Relaxed),
            1,
            "Bypass must be set to ON"
        );

        let stopped = started_processor.stop_processing();
        plugin_instance.deactivate(stopped);
    }

    #[test]
    fn test_in_order_events_do_not_set_timing_anomaly_flag() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let mut bufs = test_util::StereoTestBuffers::new(512, 0.0, 0.0);

        // In-order events strictly within [0, 512)
        let mut event_buffer = EventBuffer::new();
        let ev1 = ParamValueEvent::new(0, ClapId::new(PARAM_INPUT_GAIN), Pckn::match_all(), 1.5);
        let ev2 =
            ParamValueEvent::new(128, ClapId::new(PARAM_OUTPUT_GAIN), Pckn::match_all(), -2.0);
        let ev3 = ParamValueEvent::new(
            256,
            ClapId::new(PARAM_GATE_THRESH),
            Pckn::match_all(),
            -50.0,
        );
        event_buffer.push(&ev1);
        event_buffer.push(&ev2);
        event_buffer.push(&ev3);
        let input_events = InputEvents::from_buffer(&event_buffer);

        test_util::process_stereo_block_prealloc(
            &mut started_processor,
            &mut bufs,
            Some(&input_events),
        );

        // Flag must NOT be set
        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "RT_STATUS_EVENT_TIMING_ANOMALY must NOT be set for valid in-order events"
        );

        // Zero event loss: all applied
        let gain_in = f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed));
        let gain_out = f32::from_bits(shared.ui_to_rt.param_output_gain.load(Ordering::Relaxed));
        let gate_thresh = f32::from_bits(shared.ui_to_rt.param_gate_thresh.load(Ordering::Relaxed));
        assert_eq!(gain_in, 1.5);
        assert_eq!(gain_out, -2.0);
        assert_eq!(gate_thresh, -50.0);

        let stopped = started_processor.stop_processing();
        plugin_instance.deactivate(stopped);
    }

    #[test]
    fn test_empty_audio_fallback_drains_events() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let mut event_buffer = EventBuffer::new();
        let ev = ParamValueEvent::new(0, ClapId::new(PARAM_INPUT_GAIN), Pckn::match_all(), 5.0);
        event_buffer.push(&ev);
        let input_events = InputEvents::from_buffer(&event_buffer);

        let mut mono_bufs = test_util::MonoTestBuffers::new(0, 0.0);
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
        let mut output_events = OutputEvents::from_buffer(&mut mono_bufs.output_events_buffer);

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

        // Parameter was applied via fallback drain
        let gain_in = f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed));
        assert_eq!(
            gain_in, 5.0,
            "Input gain must be applied even with empty audio ports"
        );

        let stopped = started_processor.stop_processing();
        plugin_instance.deactivate(stopped);
    }

    #[test]
    fn test_out_of_order_events_zero_alloc() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let mut bufs = test_util::StereoTestBuffers::new(512, 0.0, 0.0);

        // Warm up ports before entering zero-alloc audit window
        test_util::process_stereo_block_prealloc(&mut started_processor, &mut bufs, None);

        let mut event_buffer = EventBuffer::new();
        let ev1 = ParamValueEvent::new(100, ClapId::new(PARAM_INPUT_GAIN), Pckn::match_all(), 2.5);
        let ev2 = ParamValueEvent::new(50, ClapId::new(PARAM_OUTPUT_GAIN), Pckn::match_all(), -3.0);
        event_buffer.push(&ev1);
        event_buffer.push(&ev2);
        let input_events = InputEvents::from_buffer(&event_buffer);

        test_util::assert_zero_alloc("out_of_order_event_containment", || {
            test_util::process_stereo_block_prealloc(
                &mut started_processor,
                &mut bufs,
                Some(&input_events),
            );
        });

        let stopped = started_processor.stop_processing();
        plugin_instance.deactivate(stopped);
    }
}

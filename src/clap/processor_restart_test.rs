// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::params::PARAM_OVERSAMPLE;
    use crate::clap::test_util;
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::{ClapId, Cookie};
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_OS_REBUILD;
    use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
    use std::sync::atomic::Ordering;

    /// Verify that changing oversampling during active processing
    /// stores the pending restart factor and does NOT set RT_STATUS_NEEDS_OS_REBUILD.
    #[test]
    fn test_oversample_change_stores_pending_factor_when_active() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let n = 256;
        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: n as u32,
            max_frames_count: n as u32,
        };

        let stopped = plugin_instance
            .activate(|_, _| (), audio_config)
            .expect("Failed to activate");
        let mut started = stopped
            .start_processing()
            .expect("Failed to start processing");

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // Verify plugin is active
        let buffer_size = shared.cold.buffer_size.load(Ordering::Relaxed);
        assert!(
            buffer_size > 0,
            "plugin should be active after start_processing"
        );

        // Verify no pending restart initially
        assert_eq!(
            shared
                .cold
                .pending_restart_os_factor
                .load(Ordering::Relaxed),
            0
        );

        // Build input events with oversampling change (2x)
        let event = ParamValueEvent::new(
            0u32,
            ClapId::new(PARAM_OVERSAMPLE),
            Pckn::match_all(),
            1.0,
            Cookie::empty(),
        );
        let mut event_buffer = EventBuffer::new();
        event_buffer.push(&event);
        let input_events = InputEvents::from_buffer(&event_buffer);
        let mut bufs = test_util::StereoTestBuffers::new(n, 0.0, 0.0);
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
            .expect("process should succeed");

        // After processing, the oversample change should have been detected
        // by process_dsp_audio -> set_oversample -> apply_oversample.
        // Since buffer_size > 0, it should store the pending factor
        // and NOT set RT_STATUS_NEEDS_OS_REBUILD.
        let pending = shared
            .cold
            .pending_restart_os_factor
            .load(Ordering::Relaxed);
        assert_eq!(
            OversampleFactor::from_f32(pending as f32),
            OversampleFactor::X2,
            "pending restart factor should be stored for the requested oversampling"
        );

        let needs_rebuild = shared.cold.rt_status.check_flag(RT_STATUS_NEEDS_OS_REBUILD);
        assert!(
            !needs_rebuild,
            "RT_STATUS_NEEDS_OS_REBUILD should NOT be set for active oversample changes"
        );
    }

    /// Verify that `activate()` consumes the pending restart factor.
    #[test]
    fn test_activate_clears_pending_restart_factor() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // Set a pending restart factor as if the audio thread requested it
        shared
            .cold
            .pending_restart_os_factor
            .store(OversampleFactor::X2.to_f32() as u32, Ordering::Release);

        assert_ne!(
            shared
                .cold
                .pending_restart_os_factor
                .load(Ordering::Relaxed),
            0
        );

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 256,
            max_frames_count: 256,
        };

        let _stopped = plugin_instance
            .activate(|_, _| (), audio_config)
            .expect("Failed to activate");

        // After activate(), the pending factor should be consumed
        assert_eq!(
            shared
                .cold
                .pending_restart_os_factor
                .load(Ordering::Relaxed),
            0,
            "pending restart factor should be cleared after activate() consumes it"
        );
    }
}

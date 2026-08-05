// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    #[cfg(feature = "heap-audit")]
    use crate::clap::test_util::{self, StereoTestBuffers};
    #[cfg(feature = "heap-audit")]
    use clack_host::prelude::*;

    #[cfg(feature = "heap-audit")]
    use std::sync::atomic::Ordering;

    #[cfg(feature = "heap-audit")]
    struct AuditEnabledGuard;

    #[cfg(feature = "heap-audit")]
    impl AuditEnabledGuard {
        fn new() -> Self {
            neural_amp_modeler_rs::common::alloc_audit::AUDIT_ENABLED
                .store(true, Ordering::Relaxed);
            Self
        }
    }

    #[cfg(feature = "heap-audit")]
    impl Drop for AuditEnabledGuard {
        fn drop(&mut self) {
            neural_amp_modeler_rs::common::alloc_audit::AUDIT_ENABLED
                .store(false, Ordering::Relaxed);
        }
    }

    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_heap_audit_trigger() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let model_dir = neural_amp_modeler_rs::testing::fixtures::model_path("mock_a2.nam");

        let params = test_util::make_default_params(Some(model_dir));
        let state_bytes = serde_json::to_vec(&params).unwrap();
        let state_ext = test_util::get_state_ext(&mut plugin_instance);
        let mut handle = plugin_instance.plugin_handle();
        let _ = state_ext.load(&mut handle, &mut state_bytes.as_slice());

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // mock_a2.nam fails to build (model_l = None post-build).
        // load_model now rejects this — counter stays at 0.
        assert_eq!(
            shared.cold.model_load_counter.load(Ordering::Relaxed),
            0,
            "model_load_counter should not increment when model build fails"
        );

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
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

        // Activates heap audit globally using RAII guard
        let _audit_guard = AuditEnabledGuard::new();

        // Resets the status flag to ensure it is clean before
        shared
            .cold
            .rt_status
            .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC);

        // Runs process(), which should return Continue (zero heap allocations in the hot path)
        let status = started_processor
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .expect("process failed in heap audit test");

        // Verifies that the returned status is Continue (zero-alloc path)
        assert!(
            matches!(status, ProcessStatus::Continue),
            "Expected ProcessStatus::Continue (zero-alloc), got {status:?}"
        );

        // Verifies the RT_STATUS_HEAP_ALLOC flag was NOT set (no allocations detected)
        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC)
        );

        // Verifies the RT_STATUS_MODEL_LOAD_FAILED flag was set (as mock_a2.nam fails to build)
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED),
            "Expected RT_STATUS_MODEL_LOAD_FAILED to be set because mock_a2.nam fails to build"
        );
    }
}

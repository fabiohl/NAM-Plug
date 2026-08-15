// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    #[cfg(feature = "heap-audit")]
    use crate::clap::test_util::TestHost;
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

    /// Runs a single stereo block through the processor, returning the
    /// `ProcessStatus`. The audio and event buffers are re-created per block
    /// so the borrows stay local to each call.
    #[cfg(feature = "heap-audit")]
    fn process_block(
        started: &mut StartedPluginAudioProcessor<TestHost>,
        bufs: &mut StereoTestBuffers,
    ) -> ProcessStatus {
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

        started
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .expect("process failed in heap audit test")
    }

    /// Writes a short decaying-sine impulse response as a mono IEEE-float32
    /// WAV so the cabsim hot path is exercised during the audit.
    #[cfg(feature = "heap-audit")]
    fn write_synthetic_ir(path: &std::path::Path, sample_rate: u32) {
        let samples: Vec<f32> = (0..512)
            .map(|i| {
                let t = i as f32;
                (t * 0.1).sin() * (-t * 0.02).exp()
            })
            .collect();
        neural_amp_modeler_rs::testing::wav::write_wav_f32(path, &samples, sample_rate)
            .expect("failed to write synthetic IR WAV");
    }

    /// R-12: the heap-audit gate must run continuous inference on a real
    /// model with oversampling and cabsim enabled, asserting zero heap
    /// allocations in the active hot path. A model that fails to load is
    /// a failure (the previous fixture was intentionally invalid and only
    /// exercised the bypass path).
    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_heap_audit_real_inference_zero_alloc() {
        let model_path = crate::clap::test_util::model_path("wavenet_a1_standard.nam");
        assert!(
            model_path.exists(),
            "wavenet_a1_standard.nam fixture missing — heap-audit gate requires a real model"
        );

        let ir_path = std::env::temp_dir().join("nam_plug_heap_audit_ir.wav");
        write_synthetic_ir(&ir_path, 48000);

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let mut params = test_util::make_default_params(Some(model_path));
        params.oversample = neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X2;
        params.ir_path = Some(ir_path);
        test_util::load_plugin_state(&mut plugin_instance, &params);

        plugin_instance.call_on_main_thread_callback();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // The model MUST have loaded — a zero counter means the audit only
        // exercised the bypass path (the previous fail-open behaviour).
        assert!(
            shared.cold.model_load_counter.load(Ordering::Relaxed) > 0,
            "model_load_counter must be > 0 — the audit must run real inference"
        );

        let n = 512;
        let mut bufs = StereoTestBuffers::new(n, 0.2, 0.2);

        // Warm-up: drain the LoadModel / LoadCabIr / Params commands and let
        // the gate/hysteresis/smoothers converge before auditing.
        for _ in 0..8 {
            process_block(&mut started_processor, &mut bufs);
        }

        // Audited steady-state blocks: continuous inference with oversampling
        // and cabsim must be zero-alloc.
        let _audit_guard = AuditEnabledGuard::new();
        shared
            .cold
            .rt_status
            .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC);

        for _ in 0..8 {
            let status = process_block(&mut started_processor, &mut bufs);
            assert!(
                matches!(status, ProcessStatus::Continue),
                "expected ProcessStatus::Continue (zero-alloc), got {status:?}"
            );
            assert!(
                !shared
                    .cold
                    .rt_status
                    .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC),
                "RT_STATUS_HEAP_ALLOC set — allocation detected in inference hot path"
            );
            assert_eq!(
                neural_amp_modeler_rs::common::alloc_audit::get_alloc_count(),
                0,
                "zero heap allocations expected in active inference path"
            );
        }
    }

    /// Secondary gate: an invalid model must fail to load gracefully (no
    /// panic, `RT_STATUS_MODEL_LOAD_FAILED` set, counter stays at 0). Kept
    /// as a distinct test so graceful-degradation behaviour is still covered.
    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_heap_audit_invalid_model_graceful() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let invalid_path = std::env::temp_dir().join("nam_plug_heap_audit_invalid.nam");
        test_util::write_invalid_model_fixture(&invalid_path);

        let params = test_util::make_default_params(Some(invalid_path));
        let state_bytes = serde_json::to_vec(&params).unwrap();
        let state_ext = test_util::get_state_ext(&mut plugin_instance);
        let mut handle = plugin_instance.plugin_handle();
        let _ = state_ext.load(&mut handle, &mut state_bytes.as_slice());

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // Invalid fixture fails to build (model_l = None post-build).
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

        // Activates heap audit globally using RAII guard
        let _audit_guard = AuditEnabledGuard::new();

        // Resets the status flag to ensure it is clean before
        shared
            .cold
            .rt_status
            .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC);

        // Runs process(), which should return Continue (zero heap allocations in the hot path)
        let status = process_block(&mut started_processor, &mut bufs);

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

        // Verifies the RT_STATUS_MODEL_LOAD_FAILED flag was set (invalid fixture fails to build)
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED),
            "Expected RT_STATUS_MODEL_LOAD_FAILED to be set because invalid fixture fails to build"
        );
    }
}

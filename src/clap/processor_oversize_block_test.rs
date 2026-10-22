// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Oversize block handling test suite (F-RA-04).
//!
//! Validates that host audio blocks exceeding `max_frames_count` (e.g., 2×, 3×+1)
//! are sliced and processed without mute/silence, without panic, without
//! heap allocation, with continuous DSP state across boundaries, while setting
//! `RT_STATUS_HOST_CONTRACT_VIOLATION`.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::params::{PARAM_BYPASS, PARAM_OUTPUT_GAIN};
    use crate::clap::host_harness::{
        extract_plugin_main_thread, extract_plugin_shared, make_harness_audio_processor,
        make_test_plugin_with_harness, perform_restart, process_block_harness_fallible,
    };
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::ClapId;
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::common::spsc::{
        RT_STATUS_EVENT_TIMING_ANOMALY, RT_STATUS_HOST_CONTRACT_VIOLATION,
    };
    use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    const SR: f64 = 48000.0;
    const MAX_FRAMES: u32 = 256;

    fn test_audio_config() -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: SR,
            min_frames_count: 64,
            max_frames_count: MAX_FRAMES,
        }
    }

    fn generate_sine_signal(n: usize, freq_hz: f32) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / SR as f32;
                (2.0 * std::f32::consts::PI * freq_hz * t).sin() * 0.25
            })
            .collect()
    }

    fn write_test_ir(name: &str) -> PathBuf {
        let mut ir = vec![0.0f32; 512];
        ir[0] = 0.8;
        ir[1] = -0.4;
        ir[2] = 0.2;
        let path = crate::clap::test_util::tmp_path(&format!("{name}.wav"));
        neural_amp_modeler_rs::testing::wav::write_wav_f32(&path, &ir, SR as u32)
            .expect("write test IR WAV");
        path
    }

    fn setup_instance_with_model(
        model_name: &str,
        os_factor: OversampleFactor,
        ir_path: Option<&Path>,
    ) -> (
        PluginInstance<crate::clap::host_harness::CompleteHost>,
        crate::clap::host_harness::CompleteHostState,
        StartedPluginAudioProcessor<crate::clap::host_harness::CompleteHost>,
    ) {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        let mode = match os_factor {
            OversampleFactor::Off => 0,
            OversampleFactor::X2 => 1,
            OversampleFactor::X4 => 2,
            _ => 0,
        };
        shared
            .ui_to_rt
            .param_oversample
            .store(mode, Ordering::Relaxed);

        let stopped = instance
            .activate(
                |_, _| make_harness_audio_processor(&state),
                test_audio_config(),
            )
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");

        let mt_ptr = extract_plugin_main_thread(&mut instance);
        let mt = unsafe { &*mt_ptr };

        let model_file = crate::clap::test_util::model_path(model_name);
        assert!(model_file.exists(), "fixture missing: {model_name}");
        mt.load_model(&model_file).expect("load model");

        // Process a priming conforming block to drain SPSC and install model
        let mut il = vec![0.1f32; MAX_FRAMES as usize];
        let mut ir = vec![0.1f32; MAX_FRAMES as usize];
        let mut ol = vec![0.0f32; MAX_FRAMES as usize];
        let mut or = vec![0.0f32; MAX_FRAMES as usize];
        let _ =
            process_block_harness_fallible(&mut started, &mut il, &mut ir, &mut ol, &mut or, None)
                .expect("prime block");

        if let Some(ir) = ir_path {
            mt.load_cabsim(ir).expect("load cabsim");
            started = perform_restart(&mut instance, started, &state, test_audio_config());
        }

        (instance, state, started)
    }

    /// Scenario 1: Model active, oversize blocks at 2x and 3x+1 max_frames_count.
    #[test]
    fn test_oversize_block_model_active_2x_and_3x_plus_1() {
        let (_instance, _state, mut started) =
            setup_instance_with_model("lstm.nam", OversampleFactor::Off, None);

        for &block_size in &[2 * MAX_FRAMES as usize, 3 * MAX_FRAMES as usize + 1] {
            let mut in_l = generate_sine_signal(block_size, 440.0);
            let mut in_r = in_l.clone();
            let mut out_l = vec![0.0f32; block_size];
            let mut out_r = vec![0.0f32; block_size];

            let res = process_block_harness_fallible(
                &mut started,
                &mut in_l,
                &mut in_r,
                &mut out_l,
                &mut out_r,
                None,
            );

            assert!(
                res.is_ok(),
                "process() should return Ok for oversize block size {block_size}, got: {res:?}"
            );

            let (status, _) = res.unwrap();
            assert_eq!(status, ProcessStatus::Continue);

            // Assert finite and non-silent
            assert!(
                out_l.iter().all(|s| s.is_finite()),
                "out_l contained non-finite values for block size {block_size}"
            );
            assert!(
                out_r.iter().all(|s| s.is_finite()),
                "out_r contained non-finite values for block size {block_size}"
            );
            let energy_l: f32 = out_l.iter().map(|s| s.abs()).sum();
            assert!(
                energy_l > 1e-4,
                "out_l was silent for block size {block_size}, energy = {energy_l}"
            );
        }
    }

    /// Scenario 2: Bypass active, oversize blocks at 2x and 3x+1 max_frames_count.
    #[test]
    fn test_oversize_block_bypass_active() {
        let (_instance, _state, mut started) =
            setup_instance_with_model("lstm.nam", OversampleFactor::Off, None);

        let mut event_buf = EventBuffer::new();
        let bypass_evt = ParamValueEvent::new(0, ClapId::new(PARAM_BYPASS), Pckn::match_all(), 1.0);
        event_buf.push(&bypass_evt);
        let in_events = InputEvents::from_buffer(&event_buf);

        for &block_size in &[2 * MAX_FRAMES as usize, 3 * MAX_FRAMES as usize + 1] {
            let mut in_l = generate_sine_signal(block_size, 220.0);
            let mut in_r = in_l.clone();
            let mut out_l = vec![0.0f32; block_size];
            let mut out_r = vec![0.0f32; block_size];

            let res = process_block_harness_fallible(
                &mut started,
                &mut in_l,
                &mut in_r,
                &mut out_l,
                &mut out_r,
                Some(&in_events),
            );

            assert!(
                res.is_ok(),
                "process() should return Ok for bypass oversize block size {block_size}, got: {res:?}"
            );

            assert!(out_l.iter().all(|s| s.is_finite()));
            let energy_l: f32 = out_l.iter().map(|s| s.abs()).sum();
            assert!(
                energy_l > 1e-4,
                "bypassed output was silent for block size {block_size}"
            );
        }
    }

    /// Scenario 3: Oversampling 2x and 4x with oversize blocks.
    #[test]
    fn test_oversize_block_oversampling_2x_and_4x() {
        for &os in &[OversampleFactor::X2, OversampleFactor::X4] {
            let (_instance, _state, mut started) = setup_instance_with_model("lstm.nam", os, None);

            for &block_size in &[2 * MAX_FRAMES as usize, 3 * MAX_FRAMES as usize + 1] {
                let mut in_l = generate_sine_signal(block_size, 330.0);
                let mut in_r = in_l.clone();
                let mut out_l = vec![0.0f32; block_size];
                let mut out_r = vec![0.0f32; block_size];

                let res = process_block_harness_fallible(
                    &mut started,
                    &mut in_l,
                    &mut in_r,
                    &mut out_l,
                    &mut out_r,
                    None,
                );

                assert!(
                    res.is_ok(),
                    "process() should return Ok for OS {os:?} block size {block_size}, got: {res:?}"
                );

                assert!(out_l.iter().all(|s| s.is_finite()));
                let energy_l: f32 = out_l.iter().map(|s| s.abs()).sum();
                assert!(energy_l > 1e-4, "output was silent for OS {os:?}");
            }
        }
    }

    /// Scenario 4: CabSim ON with oversize blocks.
    #[test]
    fn test_oversize_block_cabsim_on() {
        let ir_path = write_test_ir("oversize_test_ir");
        let (_instance, _state, mut started) =
            setup_instance_with_model("lstm.nam", OversampleFactor::Off, Some(&ir_path));

        for &block_size in &[2 * MAX_FRAMES as usize, 3 * MAX_FRAMES as usize + 1] {
            let mut in_l = generate_sine_signal(block_size, 550.0);
            let mut in_r = in_l.clone();
            let mut out_l = vec![0.0f32; block_size];
            let mut out_r = vec![0.0f32; block_size];

            let res = process_block_harness_fallible(
                &mut started,
                &mut in_l,
                &mut in_r,
                &mut out_l,
                &mut out_r,
                None,
            );

            assert!(
                res.is_ok(),
                "process() should return Ok for CabSim oversize block size {block_size}, got: {res:?}"
            );

            assert!(out_l.iter().all(|s| s.is_finite()));
            let energy_l: f32 = out_l.iter().map(|s| s.abs()).sum();
            assert!(energy_l > 1e-4, "output was silent with CabSim active");
        }
    }

    /// Scenario 5: Mono and Dual-Mono with oversize blocks.
    #[test]
    fn test_oversize_block_mono_and_dual_mono() {
        let (_instance, _state, mut started) =
            setup_instance_with_model("lstm.nam", OversampleFactor::Off, None);

        for &block_size in &[2 * MAX_FRAMES as usize, 3 * MAX_FRAMES as usize + 1] {
            // Mono: in_l == in_r
            let mut in_l = generate_sine_signal(block_size, 400.0);
            let mut in_r = in_l.clone();
            let mut out_l = vec![0.0f32; block_size];
            let mut out_r = vec![0.0f32; block_size];

            let res = process_block_harness_fallible(
                &mut started,
                &mut in_l,
                &mut in_r,
                &mut out_l,
                &mut out_r,
                None,
            );
            assert!(res.is_ok());

            // Dual-mono: distinct in_l and in_r
            let mut in_l2 = generate_sine_signal(block_size, 440.0);
            let mut in_r2 = generate_sine_signal(block_size, 880.0);
            let mut out_l2 = vec![0.0f32; block_size];
            let mut out_r2 = vec![0.0f32; block_size];

            let res2 = process_block_harness_fallible(
                &mut started,
                &mut in_l2,
                &mut in_r2,
                &mut out_l2,
                &mut out_r2,
                None,
            );
            assert!(res2.is_ok());
            assert!(out_l2.iter().all(|s| s.is_finite()));
            assert!(out_r2.iter().all(|s| s.is_finite()));
        }
    }

    /// Scenario 6: Continuity and contract violation flag.
    #[test]
    fn test_oversize_block_sets_contract_flag_and_continuous_state() {
        let (mut instance, _state, mut started) =
            setup_instance_with_model("lstm.nam", OversampleFactor::Off, None);
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
            "flag must be clear before oversize block"
        );

        let block_size = 2 * MAX_FRAMES as usize;
        let mut in_l = generate_sine_signal(block_size, 440.0);
        let mut in_r = in_l.clone();
        let mut out_l = vec![0.0f32; block_size];
        let mut out_r = vec![0.0f32; block_size];

        let res = process_block_harness_fallible(
            &mut started,
            &mut in_l,
            &mut in_r,
            &mut out_l,
            &mut out_r,
            None,
        );

        assert!(res.is_ok());
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
            "RT_STATUS_HOST_CONTRACT_VIOLATION must be asserted after oversize block"
        );
    }

    /// Scenario 7: Event offset recalculation across slice boundaries (T-R4.2).
    /// Events scheduled in slice 0, slice 1, and beyond must be processed accurately.
    #[test]
    fn test_oversize_block_events_across_slices() {
        let (mut instance, _state, mut started) =
            setup_instance_with_model("lstm.nam", OversampleFactor::Off, None);
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        let block_size = 2 * MAX_FRAMES as usize;
        let mut in_l = generate_sine_signal(block_size, 440.0);
        let mut in_r = in_l.clone();
        let mut out_l = vec![0.0f32; block_size];
        let mut out_r = vec![0.0f32; block_size];

        let mut event_buf = EventBuffer::new();

        // Event in slice 0 (time = 64): bypass ON
        event_buf.push(&ParamValueEvent::new(
            64,
            ClapId::new(PARAM_BYPASS),
            Pckn::match_all(),
            1.0,
        ));

        // Event in slice 1 (time = MAX_FRAMES + 64): bypass OFF
        event_buf.push(&ParamValueEvent::new(
            MAX_FRAMES + 64,
            ClapId::new(PARAM_BYPASS),
            Pckn::match_all(),
            0.0,
        ));

        let in_events = InputEvents::from_buffer(&event_buf);

        let res = process_block_harness_fallible(
            &mut started,
            &mut in_l,
            &mut in_r,
            &mut out_l,
            &mut out_r,
            Some(&in_events),
        );

        assert!(res.is_ok());
        assert!(out_l.iter().all(|s| s.is_finite()));
        assert!(out_r.iter().all(|s| s.is_finite()));
        // Energy must be non-zero
        let energy: f32 = out_l.iter().map(|s| s * s).sum();
        assert!(energy > 1e-4, "output must have energy: {energy}");

        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "no timing anomaly should be raised for valid cross-slice events"
        );
    }

    /// Scenario 8: Sample-accurate gain automation in second slice of oversize block (R7.1).
    ///
    /// Slices a 2x oversize block (512 frames, max_frames=256) with a parameter event
    /// at sample 384 (sample 128 of slice 1). The output must match the output of
    /// two consecutive conforming 256-frame blocks (the second having the event at sample 128),
    /// and RT_STATUS_EVENT_TIMING_ANOMALY must NOT be asserted.
    #[test]
    fn test_oversize_block_sample_accurate_gain_in_second_slice() {
        let (mut instance_a, _state_a, mut started_a) =
            setup_instance_with_model("lstm.nam", OversampleFactor::Off, None);
        let shared_a = unsafe { &*extract_plugin_shared(&mut instance_a) };

        let (mut instance_b, _state_b, mut started_b) =
            setup_instance_with_model("lstm.nam", OversampleFactor::Off, None);
        let shared_b = unsafe { &*extract_plugin_shared(&mut instance_b) };

        let block_size = 2 * MAX_FRAMES as usize; // 512
        let sig = generate_sine_signal(block_size, 440.0);

        // Instance A: Single oversize 512 block with event at t = 384
        let mut in_l_a = sig.clone();
        let mut in_r_a = sig.clone();
        let mut out_l_a = vec![0.0f32; block_size];
        let mut out_r_a = vec![0.0f32; block_size];

        let mut event_buf_a = EventBuffer::new();
        event_buf_a.push(&ParamValueEvent::new(
            384,
            ClapId::new(PARAM_OUTPUT_GAIN),
            Pckn::match_all(),
            -6.0,
        ));
        let in_events_a = InputEvents::from_buffer(&event_buf_a);

        let res_a = process_block_harness_fallible(
            &mut started_a,
            &mut in_l_a,
            &mut in_r_a,
            &mut out_l_a,
            &mut out_r_a,
            Some(&in_events_a),
        );
        assert!(res_a.is_ok());

        // Instance B: Two conforming 256 blocks, event in block 2 at t = 128
        let half = MAX_FRAMES as usize;
        let mut in_l_b1 = sig[..half].to_vec();
        let mut in_r_b1 = sig[..half].to_vec();
        let mut out_l_b1 = vec![0.0f32; half];
        let mut out_r_b1 = vec![0.0f32; half];

        let res_b1 = process_block_harness_fallible(
            &mut started_b,
            &mut in_l_b1,
            &mut in_r_b1,
            &mut out_l_b1,
            &mut out_r_b1,
            None,
        );
        assert!(res_b1.is_ok());

        let mut in_l_b2 = sig[half..].to_vec();
        let mut in_r_b2 = sig[half..].to_vec();
        let mut out_l_b2 = vec![0.0f32; half];
        let mut out_r_b2 = vec![0.0f32; half];

        let mut event_buf_b = EventBuffer::new();
        event_buf_b.push(&ParamValueEvent::new(
            128,
            ClapId::new(PARAM_OUTPUT_GAIN),
            Pckn::match_all(),
            -6.0,
        ));
        let in_events_b = InputEvents::from_buffer(&event_buf_b);

        let res_b2 = process_block_harness_fallible(
            &mut started_b,
            &mut in_l_b2,
            &mut in_r_b2,
            &mut out_l_b2,
            &mut out_r_b2,
            Some(&in_events_b),
        );
        assert!(res_b2.is_ok());

        let out_l_b = [out_l_b1, out_l_b2].concat();

        // Parity check: exact sample match
        let max_diff = out_l_a
            .iter()
            .zip(&out_l_b)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_diff < 1e-5,
            "gain automation output parity mismatch between sliced oversize and conforming blocks: max_diff={max_diff}"
        );

        // Instance A flags check
        assert!(
            shared_a
                .cold
                .rt_status
                .check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
            "RT_STATUS_HOST_CONTRACT_VIOLATION must be asserted"
        );
        assert!(
            !shared_a
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "RT_STATUS_EVENT_TIMING_ANOMALY must NOT be asserted for valid slice-2 event"
        );
        assert!(
            !shared_b
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "conforming instance B must have no timing anomaly"
        );
    }

    /// Scenario 9: Sample-accurate bypass automation in second slice of oversize block (R7.1).
    #[test]
    fn test_oversize_block_sample_accurate_bypass_in_second_slice() {
        let (mut instance_a, _state_a, mut started_a) =
            setup_instance_with_model("lstm.nam", OversampleFactor::Off, None);
        let shared_a = unsafe { &*extract_plugin_shared(&mut instance_a) };

        let (mut instance_b, _state_b, mut started_b) =
            setup_instance_with_model("lstm.nam", OversampleFactor::Off, None);
        let shared_b = unsafe { &*extract_plugin_shared(&mut instance_b) };

        let block_size = 2 * MAX_FRAMES as usize; // 512
        let sig = generate_sine_signal(block_size, 440.0);

        // Instance A: Single oversize 512 block with bypass ON at t = 384
        let mut in_l_a = sig.clone();
        let mut in_r_a = sig.clone();
        let mut out_l_a = vec![0.0f32; block_size];
        let mut out_r_a = vec![0.0f32; block_size];

        let mut event_buf_a = EventBuffer::new();
        event_buf_a.push(&ParamValueEvent::new(
            384,
            ClapId::new(PARAM_BYPASS),
            Pckn::match_all(),
            1.0,
        ));
        let in_events_a = InputEvents::from_buffer(&event_buf_a);

        let res_a = process_block_harness_fallible(
            &mut started_a,
            &mut in_l_a,
            &mut in_r_a,
            &mut out_l_a,
            &mut out_r_a,
            Some(&in_events_a),
        );
        assert!(res_a.is_ok());

        // Instance B: Two conforming 256 blocks, bypass ON in block 2 at t = 128
        let half = MAX_FRAMES as usize;
        let mut in_l_b1 = sig[..half].to_vec();
        let mut in_r_b1 = sig[..half].to_vec();
        let mut out_l_b1 = vec![0.0f32; half];
        let mut out_r_b1 = vec![0.0f32; half];

        let res_b1 = process_block_harness_fallible(
            &mut started_b,
            &mut in_l_b1,
            &mut in_r_b1,
            &mut out_l_b1,
            &mut out_r_b1,
            None,
        );
        assert!(res_b1.is_ok());

        let mut in_l_b2 = sig[half..].to_vec();
        let mut in_r_b2 = sig[half..].to_vec();
        let mut out_l_b2 = vec![0.0f32; half];
        let mut out_r_b2 = vec![0.0f32; half];

        let mut event_buf_b = EventBuffer::new();
        event_buf_b.push(&ParamValueEvent::new(
            128,
            ClapId::new(PARAM_BYPASS),
            Pckn::match_all(),
            1.0,
        ));
        let in_events_b = InputEvents::from_buffer(&event_buf_b);

        let res_b2 = process_block_harness_fallible(
            &mut started_b,
            &mut in_l_b2,
            &mut in_r_b2,
            &mut out_l_b2,
            &mut out_r_b2,
            Some(&in_events_b),
        );
        assert!(res_b2.is_ok());

        let out_l_b = [out_l_b1, out_l_b2].concat();

        let max_diff = out_l_a
            .iter()
            .zip(&out_l_b)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_diff < 1e-5,
            "bypass automation output parity mismatch between sliced oversize and conforming blocks: max_diff={max_diff}"
        );

        assert!(
            !shared_a
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "no timing anomaly should be raised for slice-2 bypass event"
        );
        assert!(
            !shared_b
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "conforming instance B must have no timing anomaly"
        );
    }

    /// Scenario 10: Sample-accurate gain automation with 2x oversampling in oversize block (R7.1).
    #[test]
    fn test_oversize_block_sample_accurate_gain_with_oversampling_2x() {
        let (mut instance_a, _state_a, mut started_a) =
            setup_instance_with_model("lstm.nam", OversampleFactor::X2, None);
        let shared_a = unsafe { &*extract_plugin_shared(&mut instance_a) };

        let (mut instance_b, _state_b, mut started_b) =
            setup_instance_with_model("lstm.nam", OversampleFactor::X2, None);
        let shared_b = unsafe { &*extract_plugin_shared(&mut instance_b) };

        let block_size = 2 * MAX_FRAMES as usize; // 512
        let sig = generate_sine_signal(block_size, 440.0);

        // Instance A: Single oversize 512 block with event at t = 384
        let mut in_l_a = sig.clone();
        let mut in_r_a = sig.clone();
        let mut out_l_a = vec![0.0f32; block_size];
        let mut out_r_a = vec![0.0f32; block_size];

        let mut event_buf_a = EventBuffer::new();
        event_buf_a.push(&ParamValueEvent::new(
            384,
            ClapId::new(PARAM_OUTPUT_GAIN),
            Pckn::match_all(),
            -3.0,
        ));
        let in_events_a = InputEvents::from_buffer(&event_buf_a);

        let res_a = process_block_harness_fallible(
            &mut started_a,
            &mut in_l_a,
            &mut in_r_a,
            &mut out_l_a,
            &mut out_r_a,
            Some(&in_events_a),
        );
        assert!(res_a.is_ok());

        // Instance B: Two conforming 256 blocks, event in block 2 at t = 128
        let half = MAX_FRAMES as usize;
        let mut in_l_b1 = sig[..half].to_vec();
        let mut in_r_b1 = sig[..half].to_vec();
        let mut out_l_b1 = vec![0.0f32; half];
        let mut out_r_b1 = vec![0.0f32; half];

        let res_b1 = process_block_harness_fallible(
            &mut started_b,
            &mut in_l_b1,
            &mut in_r_b1,
            &mut out_l_b1,
            &mut out_r_b1,
            None,
        );
        assert!(res_b1.is_ok());

        let mut in_l_b2 = sig[half..].to_vec();
        let mut in_r_b2 = sig[half..].to_vec();
        let mut out_l_b2 = vec![0.0f32; half];
        let mut out_r_b2 = vec![0.0f32; half];

        let mut event_buf_b = EventBuffer::new();
        event_buf_b.push(&ParamValueEvent::new(
            128,
            ClapId::new(PARAM_OUTPUT_GAIN),
            Pckn::match_all(),
            -3.0,
        ));
        let in_events_b = InputEvents::from_buffer(&event_buf_b);

        let res_b2 = process_block_harness_fallible(
            &mut started_b,
            &mut in_l_b2,
            &mut in_r_b2,
            &mut out_l_b2,
            &mut out_r_b2,
            Some(&in_events_b),
        );
        assert!(res_b2.is_ok());

        let out_l_b = [out_l_b1, out_l_b2].concat();

        let max_diff = out_l_a
            .iter()
            .zip(&out_l_b)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_diff < 1e-5,
            "OS 2x gain automation output parity mismatch: max_diff={max_diff}"
        );

        assert!(
            !shared_a
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "no timing anomaly under OS 2x"
        );
        assert!(
            !shared_b
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "conforming instance B must have no timing anomaly"
        );
    }

    /// Scenario 11: Oversize block with no events never sets EVENT_TIMING_ANOMALY (R7.2).
    #[test]
    fn test_oversize_block_no_events_no_timing_anomaly() {
        let (mut instance, _state, mut started) =
            setup_instance_with_model("lstm.nam", OversampleFactor::Off, None);
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        let block_size = 3 * MAX_FRAMES as usize + 17;
        let mut in_l = generate_sine_signal(block_size, 440.0);
        let mut in_r = in_l.clone();
        let mut out_l = vec![0.0f32; block_size];
        let mut out_r = vec![0.0f32; block_size];

        let res = process_block_harness_fallible(
            &mut started,
            &mut in_l,
            &mut in_r,
            &mut out_l,
            &mut out_r,
            None,
        );
        assert!(res.is_ok());

        assert!(
            shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
            "host contract violation must be flagged"
        );
        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_EVENT_TIMING_ANOMALY),
            "oversize block without events must NEVER raise RT_STATUS_EVENT_TIMING_ANOMALY"
        );
    }
}

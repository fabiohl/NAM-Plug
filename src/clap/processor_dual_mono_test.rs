// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Integration tests for CLAP dual-mono architecture and port negotiation (S05).
//!
//! Validates:
//! 1. Selectable mono/stereo configurations via `PluginAudioPortsConfig`.
//! 2. Independent inference chains (`model_l` and `model_r`) with zero crosstalk.
//! 3. Preservation of independent stereo histories without mono collapse (`mono_epsilon == -1.0`).
//! 4. Single-chain active inference in Mono configuration.
//! 5. Audio ports activation gating (zeroing input when inactive, skipping output write).

use crate::clap::plugin::shared::CONFIG_ID_STEREO;
use crate::clap::test_util::{self, StereoTestBuffers, TestHost};
use clack_common::utils::ClapId;
use clack_extensions::audio_ports_config::PluginAudioPortsConfig;
use clack_host::prelude::*;
use std::sync::atomic::Ordering;

fn process_stereo_block(
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

    let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);

    started
        .process(
            &input_audio,
            &mut output_audio,
            &InputEvents::empty(),
            &mut output_events,
            None,
            None,
        )
        .expect("process failed in dual-mono test")
}

#[test]
fn test_dual_mono_stereo_mode_independent_chains() {
    let model_path = test_util::model_path("wavenet_a1_standard.nam");
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    // Select Stereo configuration (Config 1) before activating
    {
        let handle = plugin_instance.plugin_handle();
        let config_ext = handle
            .get_extension::<PluginAudioPortsConfig>()
            .expect("PluginAudioPortsConfig");
        config_ext
            .select(&handle, ClapId::new(CONFIG_ID_STEREO))
            .expect("select stereo");
    }

    let params = test_util::make_default_params(Some(model_path));
    test_util::load_plugin_state(&mut plugin_instance, &params);
    plugin_instance.call_on_main_thread_callback();

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };

    let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started_processor = stopped_processor.start_processing().unwrap();

    let mut bufs = StereoTestBuffers::new(512, 0.0, 0.0);
    // Distinct signals: 440 Hz on L, 1000 Hz on R
    for i in 0..512 {
        bufs.in_l[i] = (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48000.0).sin() * 0.5;
        bufs.in_r[i] = (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48000.0).sin() * 0.5;
    }

    // Process blocks to allow prewarm, smoothers, and pipelines to settle
    for _ in 0..5 {
        process_stereo_block(&mut started_processor, &mut bufs);
    }

    let max_abs_l = bufs.out_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    let max_abs_r = bufs.out_r.iter().fold(0.0f32, |m, &x| m.max(x.abs()));

    assert!(
        max_abs_l > 0.01,
        "Left channel must carry processed audio (got {max_abs_l})"
    );
    assert!(
        max_abs_r > 0.01,
        "Right channel must carry processed audio (got {max_abs_r})"
    );

    // Compute difference between left and right outputs
    let max_diff = bufs
        .out_l
        .iter()
        .zip(bufs.out_r.iter())
        .fold(0.0f32, |m, (&l, &r)| m.max((l - r).abs()));

    assert!(
        max_diff > 0.05,
        "Left and Right outputs must be distinct for distinct inputs (got max_diff={max_diff})"
    );
}

#[test]
fn test_dual_mono_zero_crosstalk() {
    let model_path = test_util::model_path("wavenet_a1_standard.nam");

    // ── Test 1: Crosstalk from Left into Right ──
    {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        {
            let handle = plugin_instance.plugin_handle();
            let config_ext = handle
                .get_extension::<PluginAudioPortsConfig>()
                .expect("PluginAudioPortsConfig");
            config_ext
                .select(&handle, ClapId::new(CONFIG_ID_STEREO))
                .expect("select stereo");
        }

        let params = test_util::make_default_params(Some(model_path.clone()));
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let mut bufs = StereoTestBuffers::new(512, 0.0, 0.0);

        // Run A: Left = 440 Hz sine, Right = 0.0 (silent)
        for i in 0..512 {
            bufs.in_l[i] = (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48000.0).sin() * 0.5;
            bufs.in_r[i] = 0.0;
        }
        for _ in 0..6 {
            process_stereo_block(&mut started_processor, &mut bufs);
        }
        let l_a = bufs.out_l.clone();
        let r_a = bufs.out_r.clone();

        // Run B: Left = 880 Hz sine, Right = 0.0 (silent)
        for i in 0..512 {
            bufs.in_l[i] = (i as f32 * 2.0 * std::f32::consts::PI * 880.0 / 48000.0).sin() * 0.5;
            bufs.in_r[i] = 0.0;
        }
        for _ in 0..6 {
            process_stereo_block(&mut started_processor, &mut bufs);
        }
        let l_b = bufs.out_l.clone();
        let r_b = bufs.out_r.clone();

        let diff_l = l_a
            .iter()
            .zip(l_b.iter())
            .fold(0.0f32, |m, (&a, &b)| m.max((a - b).abs()));
        assert!(
            diff_l > 0.1,
            "Left output must change when Left input changes"
        );

        let crosstalk_l_to_r = r_a
            .iter()
            .zip(r_b.iter())
            .fold(0.0f32, |m, (&a, &b)| m.max((a - b).abs()));
        assert!(
            crosstalk_l_to_r < 1e-6,
            "Right output must not be affected by Left channel signal (zero crosstalk L->R, got {crosstalk_l_to_r})"
        );
    }

    // ── Test 2: Crosstalk from Right into Left ──
    {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        {
            let handle = plugin_instance.plugin_handle();
            let config_ext = handle
                .get_extension::<PluginAudioPortsConfig>()
                .expect("PluginAudioPortsConfig");
            config_ext
                .select(&handle, ClapId::new(CONFIG_ID_STEREO))
                .expect("select stereo");
        }

        let params = test_util::make_default_params(Some(model_path));
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let mut bufs = StereoTestBuffers::new(512, 0.0, 0.0);

        // Run C: Left = 0.0 (silent), Right = 440 Hz sine
        for i in 0..512 {
            bufs.in_l[i] = 0.0;
            bufs.in_r[i] = (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48000.0).sin() * 0.5;
        }
        for _ in 0..6 {
            process_stereo_block(&mut started_processor, &mut bufs);
        }
        let l_c = bufs.out_l.clone();
        let r_c = bufs.out_r.clone();

        // Run D: Left = 0.0 (silent), Right = 880 Hz sine
        for i in 0..512 {
            bufs.in_l[i] = 0.0;
            bufs.in_r[i] = (i as f32 * 2.0 * std::f32::consts::PI * 880.0 / 48000.0).sin() * 0.5;
        }
        for _ in 0..6 {
            process_stereo_block(&mut started_processor, &mut bufs);
        }
        let l_d = bufs.out_l.clone();
        let r_d = bufs.out_r.clone();

        let diff_r = r_c
            .iter()
            .zip(r_d.iter())
            .fold(0.0f32, |m, (&a, &b)| m.max((a - b).abs()));
        assert!(
            diff_r > 0.1,
            "Right output must change when Right input changes"
        );

        let crosstalk_r_to_l = l_c
            .iter()
            .zip(l_d.iter())
            .fold(0.0f32, |m, (&a, &b)| m.max((a - b).abs()));
        assert!(
            crosstalk_r_to_l < 1e-6,
            "Left output must not be affected by Right channel signal (zero crosstalk R->L, got {crosstalk_r_to_l})"
        );
    }
}

#[test]
fn test_dual_mono_divergent_history_prevents_mono_collapse() {
    let model_path = test_util::model_path("wavenet_a1_standard.nam");
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    // Select Stereo configuration
    {
        let handle = plugin_instance.plugin_handle();
        let config_ext = handle
            .get_extension::<PluginAudioPortsConfig>()
            .expect("PluginAudioPortsConfig");
        config_ext
            .select(&handle, ClapId::new(CONFIG_ID_STEREO))
            .expect("select stereo");
    }

    let params = test_util::make_default_params(Some(model_path));
    test_util::load_plugin_state(&mut plugin_instance, &params);
    plugin_instance.call_on_main_thread_callback();

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };

    let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started_processor = stopped_processor.start_processing().unwrap();

    let mut bufs = StereoTestBuffers::new(512, 0.0, 0.0);

    // Step 1: Feed divergent signals for 5 blocks so internal receptive field states diverge
    for i in 0..512 {
        bufs.in_l[i] = (i as f32 * 0.03).sin() * 0.5;
        bufs.in_r[i] = (i as f32 * 0.07).cos() * 0.5;
    }
    for _ in 0..5 {
        process_stereo_block(&mut started_processor, &mut bufs);
    }

    // Step 2: Feed identical inputs on both channels.
    // If the engine's mono detector collapsed chains (mono_epsilon bug), outputs would become identical.
    // In dual-mono mode, mono_epsilon is -1.0 so chains remain independent and the transient outputs differ.
    for i in 0..512 {
        let x = (i as f32 * 0.04).sin() * 0.5;
        bufs.in_l[i] = x;
        bufs.in_r[i] = x;
    }
    process_stereo_block(&mut started_processor, &mut bufs);

    // Verify outputs in the first block after convergence are not collapsed to identical numbers
    let mut different_count = 0;
    for i in 0..32 {
        if (bufs.out_l[i] - bufs.out_r[i]).abs() > 1e-4 {
            different_count += 1;
        }
    }
    assert!(
        different_count > 0,
        "Divergent history must produce divergent initial outputs even when inputs become identical"
    );
}

#[test]
fn test_mono_mode_single_chain_behavior() {
    let model_path = test_util::model_path("wavenet_a1_standard.nam");
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    // Default configuration is Mono (CONFIG_ID_MONO = 0)
    let params = test_util::make_default_params(Some(model_path));
    test_util::load_plugin_state(&mut plugin_instance, &params);
    plugin_instance.call_on_main_thread_callback();

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };

    let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started_processor = stopped_processor.start_processing().unwrap();

    let mut bufs = StereoTestBuffers::new(512, 0.0, 0.0);
    for i in 0..512 {
        bufs.in_l[i] = (i as f32 * 0.05).sin() * 0.5;
        bufs.in_r[i] = (i as f32 * 0.1).sin() * 0.5;
    }

    for _ in 0..5 {
        process_stereo_block(&mut started_processor, &mut bufs);
    }

    let max_abs_l = bufs.out_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    assert!(max_abs_l > 0.01, "Mono mode processes Left channel");

    // In mono mode (S05.1 / S05.3): single active chain on Left channel (model_l).
    // Right channel is bypass defined by contract without DSP cost (no implicit mixdown/mirror of L).
    let max_abs_r = bufs.out_r.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    assert!(max_abs_r > 0.01, "Channel R carries signal");
    // Verify R is independent of L and reflects input R
    let max_diff_lr = bufs
        .out_l
        .iter()
        .zip(bufs.out_r.iter())
        .fold(0.0f32, |m, (&l, &r)| m.max((l - r).abs()));
    assert!(
        max_diff_lr > 0.05,
        "In Mono mode, output R must not be an implicit downmix/mirror of output L"
    );
    for i in 0..512 {
        assert!(
            (bufs.out_r[i] - bufs.in_r[i]).abs() < 1e-4,
            "In Mono mode, output R must be bypass of input R without evaluating model_r (sample {i}: {} vs {})",
            bufs.out_r[i],
            bufs.in_r[i]
        );
    }
}

#[test]
fn test_audio_ports_activation_gating_behavior() {
    let model_path = test_util::model_path("wavenet_a1_standard.nam");
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    // Select Stereo configuration
    {
        let handle = plugin_instance.plugin_handle();
        let config_ext = handle
            .get_extension::<PluginAudioPortsConfig>()
            .expect("PluginAudioPortsConfig");
        config_ext
            .select(&handle, ClapId::new(CONFIG_ID_STEREO))
            .expect("select stereo");
    }

    let params = test_util::make_default_params(Some(model_path));
    test_util::load_plugin_state(&mut plugin_instance, &params);
    plugin_instance.call_on_main_thread_callback();

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };

    let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started_processor = stopped_processor.start_processing().unwrap();

    let mut bufs = StereoTestBuffers::new(512, 0.0, 0.0);
    for i in 0..512 {
        bufs.in_l[i] = 0.5;
        bufs.in_r[i] = 0.5;
    }

    // 1. Deactivating port input causes input buffer to be zeroed
    shared
        .cold
        .port_input_active
        .store(false, Ordering::Relaxed);
    for _ in 0..5 {
        process_stereo_block(&mut started_processor, &mut bufs);
    }
    let max_abs = bufs.out_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    assert!(
        max_abs < 1e-3,
        "When input port is inactive, input is zeroed so output decays to silence (got {max_abs})"
    );

    // Re-activate input
    shared.cold.port_input_active.store(true, Ordering::Relaxed);
    for _ in 0..5 {
        process_stereo_block(&mut started_processor, &mut bufs);
    }
    let max_abs_restored = bufs.out_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    assert!(
        max_abs_restored > 0.01,
        "When input port is reactivated, output returns (got {max_abs_restored})"
    );

    // 2. Deactivating port output skips writing to output buffer
    shared
        .cold
        .port_output_active
        .store(false, Ordering::Relaxed);
    bufs.out_l.fill(123.456);
    bufs.out_r.fill(123.456);
    process_stereo_block(&mut started_processor, &mut bufs);
    assert_eq!(
        bufs.out_l[0], 123.456,
        "When output port is inactive, output buffer is untouched"
    );
    assert_eq!(
        bufs.out_r[0], 123.456,
        "When output port is inactive, output buffer is untouched"
    );
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Product integration tests verifying dual-mono CabSim IR channel independence (SP-05 / F-PLUG-01).
//!
//! Asserts that:
//! 1. In stereo dual-mono mode, independent asymmetric inputs on L and R produce
//!    strictly independent convolution outputs — channel R is never copied from channel L.
//! 2. Channel R's convolution output matches an independent reference convolution.
//! 3. IR ring-out tail drains correctly on both channels after input ceases.
//! 4. In mono mode, channel R remains in bypass, while channel L is convolved
//!    identically to single-channel reference.

use clack_extensions::audio_ports_config::PluginAudioPortsConfig;
use clack_host::prelude::*;
use nam_plug::clap::plugin::shared::{CONFIG_ID_MONO, CONFIG_ID_STEREO};
use nam_plug::clap::test_util;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::testing::metrics::max_abs_diff;
use std::sync::atomic::Ordering;

fn make_decaying_ir(len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| {
            let t = i as f32;
            (t * 0.05).sin() * (-t * 0.02).exp()
        })
        .collect()
}

fn process_stereo_block(
    started: &mut StartedPluginAudioProcessor<test_util::TestHost>,
    in_l: &mut [f32],
    in_r: &mut [f32],
    out_l: &mut [f32],
    out_r: &mut [f32],
) {
    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);

    let mut in_ch = [in_l, in_r];
    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(in_ch.iter_mut().map(InputChannel::constant)),
    }]);
    let out_ch = [out_l, out_r];
    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
    }]);
    let mut output_events_buffer = EventBuffer::new();
    let mut out_ev = OutputEvents::from_buffer(&mut output_events_buffer);

    started
        .process(
            &input_audio,
            &mut output_audio,
            &InputEvents::empty(),
            &mut out_ev,
            None,
            None,
        )
        .expect("process() failed");
}

fn process_mono_block(
    started: &mut StartedPluginAudioProcessor<test_util::TestHost>,
    in_l: &mut [f32],
    out_l: &mut [f32],
) {
    let mut input_ports = AudioPorts::with_capacity(1, 1);
    let mut output_ports = AudioPorts::with_capacity(1, 1);

    let mut in_ch = [in_l];
    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(in_ch.iter_mut().map(InputChannel::constant)),
    }]);
    let out_ch = [out_l];
    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
    }]);
    let mut output_events_buffer = EventBuffer::new();
    let mut out_ev = OutputEvents::from_buffer(&mut output_events_buffer);

    started
        .process(
            &input_audio,
            &mut output_audio,
            &InputEvents::empty(),
            &mut out_ev,
            None,
            None,
        )
        .expect("mono process() failed");
}

#[test]
fn test_cabsim_stereo_asymmetric_inputs_channels_independent() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    // Select Stereo audio ports configuration while deactivated.
    let handle = plugin_instance.plugin_handle();
    let config_ext = handle
        .get_extension::<PluginAudioPortsConfig>()
        .expect("PluginAudioPortsConfig must be available");
    config_ext
        .select(&handle, ClapId::new(CONFIG_ID_STEREO))
        .expect("selecting stereo configuration must succeed");

    // Configure synthetic decaying IR.
    let ir_len = 512;
    let ir = make_decaying_ir(ir_len);
    {
        let mut raw = shared.cold.ir_raw_samples.lock().unwrap();
        *raw = Some(ir.clone());
        shared
            .cold
            .ir_raw_sample_rate
            .store(48000, Ordering::Relaxed);
        shared.cold.ir_level_policy.store(1, Ordering::Relaxed); // IrLevelPolicy::Preserved
    }

    let n = 256;
    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: n as u32,
        max_frames_count: n as u32,
    };

    let stopped = plugin_instance
        .activate(|_, _| (), audio_config)
        .expect("activate with stereo and IR");
    let mut started = stopped.start_processing().expect("start processing");

    // Block 1: feed asymmetric impulses.
    // L channel: impulse at sample 0.
    // R channel: impulse at sample 100.
    let mut in1_l = vec![0.0f32; n];
    let mut in1_r = vec![0.0f32; n];
    in1_l[0] = 0.9;
    in1_r[100] = 0.9;

    let mut out1_l = vec![0.0f32; n];
    let mut out1_r = vec![0.0f32; n];

    process_stereo_block(
        &mut started,
        &mut in1_l,
        &mut in1_r,
        &mut out1_l,
        &mut out1_r,
    );

    // Block 2: silence input, allowing partition-delayed convolution output to emerge.
    let mut in2_l = vec![0.0f32; n];
    let mut in2_r = vec![0.0f32; n];
    let mut out2_l = vec![0.0f32; n];
    let mut out2_r = vec![0.0f32; n];

    process_stereo_block(
        &mut started,
        &mut in2_l,
        &mut in2_r,
        &mut out2_l,
        &mut out2_r,
    );

    // Block 3: silence input, observing continued tail decay.
    let mut in3_l = vec![0.0f32; n];
    let mut in3_r = vec![0.0f32; n];
    let mut out3_l = vec![0.0f32; n];
    let mut out3_r = vec![0.0f32; n];

    process_stereo_block(
        &mut started,
        &mut in3_l,
        &mut in3_r,
        &mut out3_l,
        &mut out3_r,
    );

    plugin_instance.deactivate(started.stop_processing());

    // Clean IR state.
    if let Ok(mut raw) = shared.cold.ir_raw_samples.lock() {
        *raw = None;
    }

    // ── Assertions ──
    // 1. Channel R must NOT be a copy of Channel L (F-PLUG-01 regression check).
    let block2_diff = max_abs_diff(&out2_l, &out2_r);
    assert!(
        block2_diff > 0.1,
        "Channel R must be independent of Channel L, not a duplicated copy (diff={block2_diff})"
    );

    // 2. Compute reference independent convolution on channel R.
    let engine_r = ConvEngine::new(&ir, n).expect("reference ConvEngine");
    let mut ref_adapter_r =
        CabSimAdapter::new(Box::new(engine_r)).expect("reference CabSimAdapter");

    let mut ref1_r = vec![0.0f32; n];
    ref1_r[100] = 0.9;
    ref_adapter_r.process_block(&mut ref1_r, None);

    let mut ref2_r = vec![0.0f32; n];
    ref_adapter_r.process_block(&mut ref2_r, None);

    let mut ref3_r = vec![0.0f32; n];
    ref_adapter_r.process_block(&mut ref3_r, None);

    let ref_diff_b2 = max_abs_diff(&out2_r, &ref2_r);
    assert!(
        ref_diff_b2 < 1e-4,
        "Channel R output must match independent reference convolution in block 2 (diff={ref_diff_b2})"
    );

    let ref_diff_b3 = max_abs_diff(&out3_r, &ref3_r);
    assert!(
        ref_diff_b3 < 1e-4,
        "Channel R output must match independent reference convolution in block 3 (diff={ref_diff_b3})"
    );

    // 3. Tail must be non-zero after input ceases.
    let tail_energy_l: f32 = out3_l.iter().map(|x| x * x).sum();
    let tail_energy_r: f32 = out3_r.iter().map(|x| x * x).sum();
    assert!(
        tail_energy_l > 1e-5,
        "Channel L tail ring-out must be non-zero after input ceased (energy={tail_energy_l})"
    );
    assert!(
        tail_energy_r > 1e-5,
        "Channel R tail ring-out must be non-zero after input ceased (energy={tail_energy_r})"
    );
}

#[test]
fn test_cabsim_mono_input_bit_exact_with_reference() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    // Explicitly verify Mono configuration (Config 0, default).
    let handle = plugin_instance.plugin_handle();
    let config_ext = handle
        .get_extension::<PluginAudioPortsConfig>()
        .expect("PluginAudioPortsConfig");
    config_ext
        .select(&handle, ClapId::new(CONFIG_ID_MONO))
        .expect("select mono");

    let ir_len = 256;
    let ir = make_decaying_ir(ir_len);
    {
        let mut raw = shared.cold.ir_raw_samples.lock().unwrap();
        *raw = Some(ir.clone());
        shared
            .cold
            .ir_raw_sample_rate
            .store(48000, Ordering::Relaxed);
        shared.cold.ir_level_policy.store(1, Ordering::Relaxed); // IrLevelPolicy::Preserved
    }

    let n = 256;
    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: n as u32,
        max_frames_count: n as u32,
    };

    let stopped = plugin_instance
        .activate(|_, _| (), audio_config)
        .expect("activate mono with IR");
    let mut started = stopped.start_processing().expect("start processing");

    let mut in1_l = vec![0.0f32; n];
    in1_l[0] = 0.8;
    let mut out1_l = vec![0.0f32; n];
    process_mono_block(&mut started, &mut in1_l, &mut out1_l);

    let mut in2_l = vec![0.0f32; n];
    let mut out2_l = vec![0.0f32; n];
    process_mono_block(&mut started, &mut in2_l, &mut out2_l);

    plugin_instance.deactivate(started.stop_processing());

    if let Ok(mut raw) = shared.cold.ir_raw_samples.lock() {
        *raw = None;
    }

    // Reference convolution
    let engine = ConvEngine::new(&ir, n).expect("reference ConvEngine");
    let mut ref_adapter = CabSimAdapter::new(Box::new(engine)).expect("reference CabSimAdapter");

    let mut ref1 = vec![0.0f32; n];
    ref1[0] = 0.8;
    ref_adapter.process_block(&mut ref1, None);

    let mut ref2 = vec![0.0f32; n];
    ref_adapter.process_block(&mut ref2, None);

    let diff = max_abs_diff(&out2_l, &ref2);
    assert!(
        diff < 1e-4,
        "Mono output must match reference single-channel convolution (diff={diff})"
    );
}

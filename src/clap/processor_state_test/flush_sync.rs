// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Flush parameter synchronization tests (D-10 / E-07 / S03.1).
//!
//! Verifies that `params.flush()` synchronizes GUI parameter updates even when
//! not echoed as host events, retargeting smoothers, flagging gate coefficient
//! recalculation, and configuring adaptive compute / slim controllers. Also validates
//! modulation handling in flush and seamless transition to audio processing.

use crate::clap::processor::NamClapProcessor;
use crate::clap::test_util::{self, StereoTestBuffers, TestHost};
use clack_extensions::params::PluginAudioProcessorParams;
use clack_host::prelude::*;
use std::sync::atomic::Ordering;

fn get_processor_mut(instance: &mut PluginInstance<TestHost>) -> &mut NamClapProcessor<'_> {
    let raw_ptr = instance.plugin_handle().as_raw_ptr();
    let ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |wrapper| Ok(wrapper.audio_processor().unwrap().as_ptr()),
        )
        .unwrap()
    };
    unsafe { &mut *ptr }
}

fn process_block(
    started: &mut StartedPluginAudioProcessor<TestHost>,
    bufs: &mut StereoTestBuffers,
) {
    let mut in_ch = [bufs.in_l.as_mut_slice(), bufs.in_r.as_mut_slice()];
    let in_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(in_ch.iter_mut().map(InputChannel::constant)),
    }]);
    let out_ch = [bufs.out_l.as_mut_slice(), bufs.out_r.as_mut_slice()];
    let mut out_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
    }]);
    let in_ev = InputEvents::empty();
    let mut out_ev = OutputEvents::from_buffer(&mut bufs.output_events_buffer);
    started
        .process(&in_audio, &mut out_audio, &in_ev, &mut out_ev, None, None)
        .unwrap();
}

fn standard_audio_config(frames: u32) -> PluginAudioConfiguration {
    PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: frames,
        max_frames_count: frames,
    }
}

#[test]
fn test_audio_processor_flush_gui_without_host_echo_syncs_smoothers_gate_and_controllers() {
    use neural_amp_modeler_rs::common::params::{ActivationPrecision, AdaptiveComputeMode};
    use neural_amp_modeler_rs::dsp::adaptive::SlimOverride;
    use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let stopped = plugin_instance
        .activate(|_, _| (), standard_audio_config(512))
        .unwrap();
    let processor = get_processor_mut(&mut plugin_instance);

    processor.gate_dirty = false;
    let u2r = &processor.shared.ui_to_rt;
    u2r.param_input_gain
        .store(6.0f32.to_bits(), Ordering::Relaxed);
    u2r.param_output_gain
        .store((-4.0f32).to_bits(), Ordering::Relaxed);
    u2r.param_gate_thresh
        .store((-45.0f32).to_bits(), Ordering::Relaxed);
    u2r.param_bypass.store(1, Ordering::Relaxed);
    u2r.param_adaptive_compute
        .store(AdaptiveComputeMode::Aggressive as u32, Ordering::Relaxed);
    u2r.param_slim_override
        .store(SlimOverride::ForceLite as u32, Ordering::Relaxed);
    u2r.param_oversample
        .store(OversampleFactor::X2.to_f32() as u32, Ordering::Relaxed);
    u2r.param_activation
        .store(ActivationPrecision::Fast as u32, Ordering::Relaxed);

    processor.shared.bump_generation();
    let bumped_gen = u2r.gui_param_generation.load(Ordering::Acquire);

    let in_ev = InputEvents::empty();
    let mut out_buf = EventBuffer::new();
    let mut out_ev = OutputEvents::from_buffer(&mut out_buf);
    processor.flush(&in_ev, &mut out_ev);

    assert_eq!(processor.params.input_gain_db, 6.0);
    let target_in = processor.gain_lut.db_to_linear(6.0);
    assert!((processor.smoother_in.target_value() - target_in).abs() < 1e-5);

    assert_eq!(processor.params.output_gain_db, -4.0);
    let target_out = processor.gain_lut.db_to_linear(-4.0);
    assert!((processor.smoother_out.target_value() - target_out).abs() < 1e-5);

    assert_eq!(processor.params.gate_threshold_db, -45.0);
    assert!(processor.gate_dirty, "gate_dirty must be flagged by flush");

    assert!(
        processor.params.bypass,
        "bypass state must be updated by flush"
    );
    assert_eq!(
        processor.params.adaptive_compute,
        AdaptiveComputeMode::Aggressive
    );
    assert_eq!(
        processor.adaptive_compute.mode(),
        AdaptiveComputeMode::Aggressive
    );

    assert_eq!(processor.params.slim_override, SlimOverride::ForceLite);
    assert_eq!(
        processor.adaptive_compute.slim_override(),
        SlimOverride::ForceLite
    );

    assert_eq!(processor.params.oversample, OversampleFactor::X2);
    assert_eq!(
        processor.params.activation_precision,
        ActivationPrecision::Fast
    );
    assert_eq!(processor.last_seen_generation, bumped_gen);

    // Verify process() runs cleanly without clobbering synced state
    let mut started = stopped.start_processing().unwrap();
    let mut bufs = StereoTestBuffers::new(512, 0.2, 0.2);
    process_block(&mut started, &mut bufs);

    let processor_after = get_processor_mut(&mut plugin_instance);
    assert_eq!(processor_after.last_seen_generation, bumped_gen);
    assert_eq!(processor_after.params.input_gain_db, 6.0);
    assert!((processor_after.smoother_in.target_value() - target_in).abs() < 1e-5);
}

#[test]
fn test_audio_processor_flush_param_mod_event() {
    use crate::clap::extensions::params::PARAM_INPUT_GAIN;
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamModEvent;
    use clack_common::utils::ClapId;

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let _stopped = plugin_instance
        .activate(|_, _| (), standard_audio_config(512))
        .unwrap();
    let processor = get_processor_mut(&mut plugin_instance);

    let mut in_buf = EventBuffer::new();
    let mod_ev = ParamModEvent::new(0, ClapId::new(PARAM_INPUT_GAIN), Pckn::match_all(), 4.5f64);
    in_buf.push(&mod_ev);
    let in_events = InputEvents::from_buffer(&in_buf);

    let mut out_buf = EventBuffer::new();
    let mut out_events = OutputEvents::from_buffer(&mut out_buf);
    processor.flush(&in_events, &mut out_events);

    assert_eq!(processor.mod_input_gain, 4.5);
    let expected_target = processor
        .gain_lut
        .db_to_linear(processor.params.input_gain_db + 4.5);
    assert!(
        (processor.smoother_in.target_value() - expected_target).abs() < 1e-5,
        "smoother_in target must include modulation amount from flush"
    );
}

#[test]
fn test_flush_then_process_and_process_then_flush_idempotent() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let stopped = plugin_instance
        .activate(|_, _| (), standard_audio_config(256))
        .unwrap();
    let processor = get_processor_mut(&mut plugin_instance);

    // 1. GUI updates input gain to 8.0 dB
    processor
        .shared
        .ui_to_rt
        .param_input_gain
        .store(8.0f32.to_bits(), Ordering::Relaxed);
    processor.shared.bump_generation();

    let in_ev = InputEvents::empty();
    let mut out_buf = EventBuffer::new();
    let mut out_ev = OutputEvents::from_buffer(&mut out_buf);
    processor.flush(&in_ev, &mut out_ev);

    assert_eq!(processor.params.input_gain_db, 8.0);
    let target = processor.gain_lut.db_to_linear(8.0);
    assert!((processor.smoother_in.target_value() - target).abs() < 1e-5);

    // Call flush again without changes — strictly idempotent
    processor.flush(&in_ev, &mut out_ev);
    assert_eq!(processor.params.input_gain_db, 8.0);
    assert!((processor.smoother_in.target_value() - target).abs() < 1e-5);

    // 2. Start processing and process audio
    let mut started = stopped.start_processing().unwrap();
    let mut bufs = StereoTestBuffers::new(256, 0.1, 0.1);
    process_block(&mut started, &mut bufs);

    let proc_after = get_processor_mut(&mut plugin_instance);
    assert_eq!(proc_after.params.input_gain_db, 8.0);

    // 3. Process then flush: GUI updates to 2.0 dB, process runs first, then flush runs
    proc_after
        .shared
        .ui_to_rt
        .param_input_gain
        .store(2.0f32.to_bits(), Ordering::Relaxed);
    proc_after.shared.bump_generation();

    process_block(&mut started, &mut bufs);

    let proc_after_p = get_processor_mut(&mut plugin_instance);
    assert_eq!(proc_after_p.params.input_gain_db, 2.0);

    proc_after_p.flush(&in_ev, &mut out_ev);
    assert_eq!(proc_after_p.params.input_gain_db, 2.0);
    let target_2 = proc_after_p.gain_lut.db_to_linear(2.0);
    assert!((proc_after_p.smoother_in.target_value() - target_2).abs() < 1e-5);
}

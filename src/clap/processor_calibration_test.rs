// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::test_util::{self, StereoTestBuffers};
    use clack_host::prelude::*;

    #[test]
    fn test_model_gain_calibration() {
        let model_path = neural_amp_modeler_rs::testing::fixtures::model_path("BossWN-nano.nam");

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

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

        let n = 512;
        let mut bufs = StereoTestBuffers::new(n, 0.5, 0.5);

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

        // ── Verify CLAP output is a processed signal (not silence, not passthrough) ──
        let clap_rms: f32 = (bufs.out_l.iter().map(|x| x * x).sum::<f32>() / n as f32).sqrt();
        assert!(
            (0.05..=0.20).contains(&clap_rms),
            "CLAP output RMS out of band ({:.6}) — expected [0.05, 0.20] for Nano model",
            clap_rms
        );

        // ── Load model directly and verify calibration multipliers ──
        use neural_amp_modeler_rs::models::NamModel;
        let sys = neural_amp_modeler_rs::common::diagnostics::SystemSnapshot::capture();
        let model_pair = neural_amp_modeler_rs::loader::build::load_and_build_model(
            &model_path,
            &sys,
            true,
            neural_amp_modeler_rs::loader::LoadOptions::default(),
        )
        .expect("Failed to load model directly");
        let mut direct_model = model_pair.model_l.expect("Failed to build direct model");
        let input_mult = model_pair.input_mult_adj;
        let output_mult = model_pair.output_mult_adj;

        assert!((input_mult - 1.0).abs() < 1e-4);
        assert!((output_mult - 1.0).abs() > 1e-4);

        use neural_amp_modeler_rs::dsp::pipeline::DENORMAL_DITHER_OFFSET;
        let mut direct_in = vec![0.5f32; n];
        for val in direct_in.iter_mut() {
            *val = *val * input_mult + DENORMAL_DITHER_OFFSET;
        }

        let mut direct_out = vec![0.0f32; n];
        direct_model.process(&direct_in, &mut direct_out);

        for val in direct_out.iter_mut() {
            *val = (*val - DENORMAL_DITHER_OFFSET) * output_mult;
        }

        let direct_rms: f32 = (direct_out.iter().map(|x| x * x).sum::<f32>() / n as f32).sqrt();
        assert!(
            direct_rms > 0.001,
            "Direct output RMS too low ({:.6})",
            direct_rms
        );
    }
}

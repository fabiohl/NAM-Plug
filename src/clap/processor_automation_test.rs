// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::params::{PARAM_GATE_THRESH, PARAM_INPUT_GAIN};
    use crate::clap::test_util::{self, StereoTestBuffers};
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::{ClapId, Cookie};
    use clack_host::prelude::*;

    #[test]
    fn test_sample_accurate_input_gain_mid_block() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let n = 256;
        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: n as u32,
            max_frames_count: n as u32,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let dc_val = 0.5f32;
        let mut bufs = StereoTestBuffers::new(n, dc_val, dc_val);

        macro_rules! run {
            ($label:expr, $events:expr) => {{
                let mut input_events_buffer = EventBuffer::new();
                for (t, pid, val) in $events {
                    input_events_buffer.push(&ParamValueEvent::new(
                        t,
                        ClapId::new(pid),
                        Pckn::match_all(),
                        val,
                        Cookie::empty(),
                    ));
                }
                let input_events = InputEvents::from_buffer(&input_events_buffer);
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
                test_util::assert_zero_alloc($label, || {
                    started_processor
                        .process(
                            &input_audio,
                            &mut output_audio,
                            &input_events,
                            &mut output_events,
                            None,
                            None,
                        )
                        .expect("Failure in process()");
                });
            }};
        }

        run!(
            "pre-warm",
            [
                (0u32, PARAM_GATE_THRESH, -90.0f64),
                (0u32, PARAM_INPUT_GAIN, -60.0f64)
            ]
        );
        run!(
            "pre-warm",
            [
                (0u32, PARAM_GATE_THRESH, -90.0f64),
                (0u32, PARAM_INPUT_GAIN, -60.0f64)
            ]
        );
        run!(
            "pre-warm",
            [
                (0u32, PARAM_GATE_THRESH, -90.0f64),
                (0u32, PARAM_INPUT_GAIN, -60.0f64)
            ]
        );
        run!(
            "pre-warm",
            [
                (0u32, PARAM_GATE_THRESH, -90.0f64),
                (0u32, PARAM_INPUT_GAIN, -60.0f64)
            ]
        );

        run!(
            "test",
            [
                (0u32, PARAM_GATE_THRESH, -90.0f64),
                (0u32, PARAM_INPUT_GAIN, -60.0f64),
                ((n / 2) as u32, PARAM_INPUT_GAIN, 0.0f64),
            ]
        );

        let mid = n / 2;
        let first_avg =
            bufs.out_l[16..mid].iter().fold(0.0f32, |a, &b| a + b.abs()) / (mid - 16) as f32;
        let second_avg = bufs.out_l[(mid + 16)..]
            .iter()
            .fold(0.0f32, |a, &b| a + b.abs())
            / (n - mid - 16) as f32;

        assert!(
            second_avg > first_avg * 1.5,
            "Second half (0dB) should be louder than first half (-60dB): first={first_avg}, second={second_avg}",
        );
    }

    #[test]
    fn test_sample_accurate_input_gain_three_events() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let n = 512;
        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: n as u32,
            max_frames_count: n as u32,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let dc_val = 0.5f32;
        let mut bufs = StereoTestBuffers::new(n, dc_val, dc_val);

        macro_rules! run {
            ($label:expr, $events:expr) => {{
                let mut input_events_buffer = EventBuffer::new();
                for (t, pid, val) in $events {
                    input_events_buffer.push(&ParamValueEvent::new(
                        t,
                        ClapId::new(pid),
                        Pckn::match_all(),
                        val,
                        Cookie::empty(),
                    ));
                }
                let input_events = InputEvents::from_buffer(&input_events_buffer);
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
                test_util::assert_zero_alloc($label, || {
                    started_processor
                        .process(
                            &input_audio,
                            &mut output_audio,
                            &input_events,
                            &mut output_events,
                            None,
                            None,
                        )
                        .expect("Failure in process()");
                });
            }};
        }

        run!(
            "pre-warm",
            [
                (0u32, PARAM_GATE_THRESH, -90.0f64),
                (0u32, PARAM_INPUT_GAIN, -60.0f64)
            ]
        );
        run!(
            "pre-warm",
            [
                (0u32, PARAM_GATE_THRESH, -90.0f64),
                (0u32, PARAM_INPUT_GAIN, -60.0f64)
            ]
        );
        run!(
            "pre-warm",
            [
                (0u32, PARAM_GATE_THRESH, -90.0f64),
                (0u32, PARAM_INPUT_GAIN, -60.0f64)
            ]
        );
        run!(
            "pre-warm",
            [
                (0u32, PARAM_GATE_THRESH, -90.0f64),
                (0u32, PARAM_INPUT_GAIN, -60.0f64)
            ]
        );

        run!(
            "test",
            [
                (0u32, PARAM_GATE_THRESH, -90.0f64),
                (0u32, PARAM_INPUT_GAIN, -60.0f64),
                (64u32, PARAM_INPUT_GAIN, 0.0f64),
                (256u32, PARAM_INPUT_GAIN, -60.0f64),
            ]
        );

        let avg_a = bufs.out_l[16..64].iter().fold(0.0f32, |a, &b| a + b.abs()) / 48.0;
        let avg_b = bufs.out_l[192..256]
            .iter()
            .fold(0.0f32, |a, &b| a + b.abs())
            / 64.0;
        let avg_c = bufs.out_l[448..512]
            .iter()
            .fold(0.0f32, |a, &b| a + b.abs())
            / 64.0;

        assert!(
            avg_b > avg_a * 1.3,
            "Segment B (0dB) should be louder than A (-60dB): A={avg_a}, B={avg_b}",
        );
        assert!(
            avg_b > avg_c * 1.3,
            "Segment B (0dB) should be louder than C (-60dB): B={avg_b}, C={avg_c}",
        );
    }

    #[test]
    fn test_block_splitting_preserves_event_order() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let n = 256;
        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: n as u32,
            max_frames_count: n as u32,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let raw_ptr = plugin_instance.plugin_handle().as_raw_ptr();
        let mut started_processor = stopped_processor.start_processing().unwrap();
        let mut bufs = StereoTestBuffers::new(n, 0.5, 0.5);

        let mut input_events_buffer = EventBuffer::new();
        for (t, pid, val) in &[
            (0u32, PARAM_GATE_THRESH, -90.0f64),
            (0u32, PARAM_INPUT_GAIN, 6.0f64),
            (64u32, PARAM_INPUT_GAIN, -12.0f64),
            (128u32, PARAM_INPUT_GAIN, 0.0f64),
        ] {
            input_events_buffer.push(&ParamValueEvent::new(
                *t,
                ClapId::new(*pid),
                Pckn::match_all(),
                *val,
                Cookie::empty(),
            ));
        }
        let input_events = InputEvents::from_buffer(&input_events_buffer);

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

        started_processor
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .expect("Failure in process()");

        let processor_ptr = unsafe {
            clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
                raw_ptr,
                |wrapper| {
                    let ptr = wrapper.audio_processor().unwrap().as_ptr();
                    Ok(ptr)
                },
            )
            .unwrap()
        };
        let processor = unsafe { &mut *processor_ptr };

        assert_eq!(
            processor.params.input_gain_db, 0.0,
            "Last ParamValueEvent (at t=128) should set input_gain_db=0.0",
        );
        assert_eq!(
            processor.smoother_in.target_value(),
            processor.gain_lut.db_to_linear(0.0),
            "Smoother target should reflect last event value (0dB)"
        );
    }

    #[test]
    fn test_apply_params_from_spsc_sets_gate_dirty() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let n = 256;
        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: n as u32,
            max_frames_count: n as u32,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let raw_ptr = plugin_instance.plugin_handle().as_raw_ptr();
        let _started_processor = stopped_processor.start_processing().unwrap();

        let processor_ptr = unsafe {
            clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
                raw_ptr,
                |wrapper| {
                    let ptr = wrapper.audio_processor().unwrap().as_ptr();
                    Ok(ptr)
                },
            )
            .unwrap()
        };
        let processor = unsafe { &mut *processor_ptr };

        processor.gate_dirty = false;

        let mut new_params = processor.params;
        new_params.gate_threshold_db = -30.0;

        processor.apply_params_from_spsc(new_params);

        assert!(
            processor.gate_dirty,
            "apply_params_from_spsc with new gate threshold must mark gate_dirty = true"
        );
    }
}

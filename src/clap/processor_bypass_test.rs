// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::params::PARAM_BYPASS;
    use crate::clap::test_util::{self, StereoTestBuffers};
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::{ClapId, Cookie};
    use clack_host::prelude::*;

    struct ProcessedStereo {
        out_l: Vec<f32>,
        out_r: Vec<f32>,
    }

    fn run_stereo_block(bypass: bool, in_l: &[f32], in_r: &[f32]) -> ProcessedStereo {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let n = in_l.len();
        let mut bufs = StereoTestBuffers::new(n, 0.0, 0.0);
        bufs.in_l.copy_from_slice(in_l);
        bufs.in_r.copy_from_slice(in_r);

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

        let mut input_events_buffer = EventBuffer::new();
        if bypass {
            let event = ParamValueEvent::new(
                0,
                ClapId::new(PARAM_BYPASS),
                Pckn::match_all(),
                1.0,
                Cookie::empty(),
            );
            input_events_buffer.push(&event);
        }
        let input_events = InputEvents::from_buffer(&input_events_buffer);
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

        ProcessedStereo {
            out_l: bufs.out_l.clone(),
            out_r: bufs.out_r.clone(),
        }
    }

    fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max)
    }

    #[test]
    fn test_zero_alloc_process_bypass() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let mut bufs = StereoTestBuffers::new(512, 0.1, 0.2);

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

        let input_events =
            InputEvents::from_buffer::<[clack_host::events::event_types::NoteOnEvent; 0]>(&[]);
        let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);

        test_util::assert_zero_alloc("CLAP process bypass", || {
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

        for i in 0..512 {
            assert!(
                (bufs.out_l[i] - bufs.in_l[i]).abs() < 1e-4,
                "Bypass failure Channel L sample {}: {} vs {}",
                i,
                bufs.out_l[i],
                bufs.in_l[i]
            );
            assert!(
                (bufs.out_r[i] - bufs.in_r[i]).abs() < 1e-4,
                "Bypass failure Channel R sample {}: {} vs {} (L was {})",
                i,
                bufs.out_r[i],
                bufs.in_r[i],
                bufs.in_l[i]
            );
        }
    }

    #[test]
    fn test_bypass_preserves_independent_stereo_channels() {
        let n = 512;
        let in_l = vec![0.1f32; n];
        let in_r = vec![0.2f32; n];

        let processed = run_stereo_block(true, &in_l, &in_r);

        assert!(
            max_abs_diff(&processed.out_l, &in_l) < 1e-4,
            "bypass must pass L through untouched"
        );
        assert!(
            max_abs_diff(&processed.out_r, &in_r) < 1e-4,
            "bypass must pass R through untouched (independent of L)"
        );
        assert!(
            max_abs_diff(&processed.out_r, &in_l) > 0.05,
            "bypass must NOT collapse R to a copy of L"
        );
    }

    #[test]
    fn test_bypass_preserves_phase_opposition_cancellation() {
        let n = 512;
        let in_l = vec![1.0f32; n];
        let in_r = vec![-1.0f32; n];

        let processed = run_stereo_block(true, &in_l, &in_r);

        for i in 0..n {
            assert!(
                (processed.out_l[i] - in_l[i]).abs() < 1e-4,
                "phase-opposition L sample {i}: {} vs {}",
                processed.out_l[i],
                in_l[i]
            );
            assert!(
                (processed.out_r[i] - in_r[i]).abs() < 1e-4,
                "phase-opposition R sample {i}: {} vs {} (L was {})",
                processed.out_r[i],
                in_r[i],
                in_l[i]
            );
        }
    }

    #[test]
    fn test_bypass_r_only_impulse_stays_on_r() {
        let n = 512;
        let in_l = vec![0.0f32; n];
        let mut in_r = vec![0.0f32; n];
        in_r[n / 2] = 1.0;

        let processed = run_stereo_block(true, &in_l, &in_r);

        assert!(
            max_abs_diff(&processed.out_l, &in_l) < 1e-4,
            "R-only impulse must not leak into L"
        );
        assert!(
            (processed.out_r[n / 2] - 1.0).abs() < 1e-4,
            "R impulse must reach R output"
        );
        assert!(
            max_abs_diff(&processed.out_r, &in_r) < 1e-4,
            "bypass must preserve the R impulse verbatim"
        );
    }

    #[test]
    fn test_wet_path_stereo_independence() {
        let n = 512;
        let in_l = vec![0.1f32; n];
        let in_r = vec![0.2f32; n];

        let processed = run_stereo_block(false, &in_l, &in_r);

        assert!(
            max_abs_diff(&processed.out_l, &in_l) < 1e-4,
            "wet path L must reflect L input"
        );
        assert!(
            max_abs_diff(&processed.out_r, &in_r) < 1e-4,
            "wet path R must reflect the real R input, not a copy of L"
        );
        assert!(
            max_abs_diff(&processed.out_r, &in_l) > 0.05,
            "wet path must NOT collapse R to a copy of L"
        );
    }

    #[test]
    fn test_in_place_stereo_no_aliasing_corruption() {
        // Host re-uses the same buffers for input and output (CLAP in-place
        // pair, as declared in audio_ports.rs). The plugin must copy into its
        // scratch before writing outputs back, so L/R stay independent and
        // neither channel corrupts the other.
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let n = 512;
        let mut buf_l = vec![0.1f32; n];
        let mut buf_r = vec![0.2f32; n];
        let l_ptr = buf_l.as_mut_ptr();
        let r_ptr = buf_r.as_mut_ptr();

        {
            // SAFETY: mirrors the CLAP FFI contract — the host hands the plugin
            // the same buffer for input and output; the slices are used only to
            // register raw pointers, never aliased by this test.
            let in_l = unsafe { std::slice::from_raw_parts_mut(l_ptr, n) };
            let out_l = unsafe { std::slice::from_raw_parts_mut(l_ptr, n) };
            let in_r = unsafe { std::slice::from_raw_parts_mut(r_ptr, n) };
            let out_r = unsafe { std::slice::from_raw_parts_mut(r_ptr, n) };

            let mut input_ports = AudioPorts::with_capacity(2, 1);
            let mut output_ports = AudioPorts::with_capacity(2, 1);
            let mut in_ch = [in_l, in_r];
            let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    in_ch.iter_mut().map(InputChannel::constant),
                ),
            }]);
            let out_ch = [out_l, out_r];
            let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
            }]);

            let input_events = InputEvents::empty();
            let mut output_events_buffer = EventBuffer::new();
            let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

            test_util::assert_zero_alloc("CLAP process in-place", || {
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
        }

        for i in 0..n {
            assert!(
                (buf_l[i] - 0.1).abs() < 1e-4,
                "InPlace L sample {i} corrupted: {}",
                buf_l[i]
            );
            assert!(
                (buf_r[i] - 0.2).abs() < 1e-4,
                "InPlace R sample {i} corrupted: {}",
                buf_r[i]
            );
        }
    }

    #[test]
    fn test_wet_path_r_only_impulse_stays_on_r() {
        let n = 512;
        let in_l = vec![0.0f32; n];
        let mut in_r = vec![0.0f32; n];
        in_r[n / 2] = 1.0;

        let processed = run_stereo_block(false, &in_l, &in_r);

        assert!(
            max_abs_diff(&processed.out_l, &in_l) < 1e-4,
            "R-only impulse must not leak into L in the wet path"
        );
        assert!(
            (processed.out_r[n / 2] - 1.0).abs() < 1e-4,
            "R impulse must reach R output in the wet path"
        );
    }

    #[test]
    fn test_flush_saturate_spsc_bump_generation_fallback() {
        use crate::clap::extensions::params::PARAM_INPUT_GAIN;
        use clack_common::events::Pckn;
        use clack_common::events::event_types::ParamValueEvent;
        use clack_common::utils::{ClapId, Cookie};
        use clack_extensions::params::PluginAudioProcessorParams;
        use std::sync::atomic::Ordering;

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();

        let raw_ptr = plugin_instance.plugin_handle().as_raw_ptr();
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

        let initial_gen = processor
            .shared
            .ui_to_rt
            .gui_param_generation
            .load(Ordering::Relaxed);

        for i in 0..20 {
            let mut input_events_buffer = EventBuffer::new();
            let event = ParamValueEvent::new(
                0,
                ClapId::new(PARAM_INPUT_GAIN),
                Pckn::match_all(),
                (10.0 + i as f64) % 20.0,
                Cookie::empty(),
            );
            input_events_buffer.push(&event);
            let input_events = InputEvents::from_buffer(&input_events_buffer);

            let mut output_events_buffer = EventBuffer::new();
            let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

            processor.flush(&input_events, &mut output_events);
        }

        let final_gen = processor
            .shared
            .ui_to_rt
            .gui_param_generation
            .load(Ordering::Relaxed);

        assert!(
            final_gen > initial_gen,
            "bump_generation should have been triggered after SPSC saturation: initial={initial_gen}, final={final_gen}",
        );

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

        started_processor
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .expect("Failure in process after flush saturation");

        let processor_after = unsafe {
            &mut *{
                clack_plugin::extensions::wrapper::PluginWrapper::<
                    crate::clap::NamClapPlugin,
                >::handle(raw_ptr, |wrapper| {
                    let ptr = wrapper.audio_processor().unwrap().as_ptr();
                    Ok(ptr)
                })
                .unwrap()
            }
        };

        assert_eq!(
            processor_after.last_seen_generation, final_gen,
            "Processor should have synced generation after flush saturation"
        );
    }

    #[test]
    fn test_smoother_warm_reset_on_reactivate() {
        use crate::clap::extensions::params::PARAM_INPUT_GAIN;
        use clack_common::events::Pckn;
        use clack_common::events::event_types::ParamValueEvent;
        use clack_common::utils::{ClapId, Cookie};
        use clack_extensions::params::PluginAudioProcessorParams;
        use std::sync::atomic::Ordering;

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();

        let raw_ptr = plugin_instance.plugin_handle().as_raw_ptr();

        {
            let processor_ptr = unsafe {
                clack_plugin::extensions::wrapper::PluginWrapper::<
                    crate::clap::NamClapPlugin,
                >::handle(raw_ptr, |wrapper| {
                    let ptr = wrapper.audio_processor().unwrap().as_ptr();
                    Ok(ptr)
                })
                .unwrap()
            };
            let processor = unsafe { &mut *processor_ptr };

            let mut input_events_buffer = EventBuffer::new();
            let event = ParamValueEvent::new(
                0,
                ClapId::new(PARAM_INPUT_GAIN),
                Pckn::match_all(),
                -12.0f64,
                Cookie::empty(),
            );
            input_events_buffer.push(&event);
            let input_events = InputEvents::from_buffer(&input_events_buffer);

            let mut output_events_buffer = EventBuffer::new();
            let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

            processor.flush(&input_events, &mut output_events);
        }

        plugin_instance.deactivate(stopped_processor);

        let stopped_processor2 = plugin_instance.activate(|_, _| (), audio_config).unwrap();

        let processor_ptr2 = unsafe {
            clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
                raw_ptr,
                |wrapper| {
                    let ptr = wrapper.audio_processor().unwrap().as_ptr();
                    Ok(ptr)
                },
            )
            .unwrap()
        };
        let processor2 = unsafe { &mut *processor_ptr2 };

        let expected_linear = processor2.gain_lut.db_to_linear(-12.0);
        let actual_initial = processor2.smoother_in.peek();
        assert!(
            (actual_initial - expected_linear).abs() < 1e-5,
            "Smoother should be warm-reset to -12 dB gain on reactivate: expected {expected_linear}, got {actual_initial}",
        );

        let input_db = f32::from_bits(
            processor2
                .shared
                .ui_to_rt
                .param_input_gain
                .load(Ordering::Relaxed),
        );
        assert_eq!(
            input_db, -12.0,
            "Shared atomic should retain -12 dB across deactivate/activate cycle",
        );

        let _ = stopped_processor2;
    }
}

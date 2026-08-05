// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::test_util::{self, StereoTestBuffers};
    use clack_host::prelude::*;

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
                (bufs.out_r[i] - bufs.in_l[i]).abs() < 1e-4,
                "Bypass failure Channel R sample {}: {} vs {}",
                i,
                bufs.out_r[i],
                bufs.in_l[i]
            );
        }
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

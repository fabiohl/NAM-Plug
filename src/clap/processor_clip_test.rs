// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "stereo"))]
    use crate::clap::test_util::{self};
    #[cfg(not(feature = "stereo"))]
    use clack_host::prelude::*;
    #[cfg(not(feature = "stereo"))]
    use std::sync::atomic::Ordering;

    #[cfg(not(feature = "stereo"))]
    #[test]
    fn test_mono_clipping_detection() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let n = 512;
        let mut input_ports = AudioPorts::with_capacity(1, 1);
        let mut output_ports = AudioPorts::with_capacity(1, 1);

        // Case 1: No clipping (signal = 0.5, gain = 0 dB -> output = 0.5)
        {
            shared.rt_to_ui.ui_clipped.store(false, Ordering::Relaxed);
            let mut in_buf = vec![0.5f32; n];
            let mut out_buf = vec![0.0f32; n];

            let mut input_channels = [in_buf.as_mut_slice()];
            let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    input_channels.iter_mut().map(InputChannel::constant),
                ),
            }]);

            let output_channels = [out_buf.as_mut_slice()];
            let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
            }]);

            let input_events = InputEvents::empty();
            let mut output_events_buffer = EventBuffer::new();
            let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

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

            let clipped = shared.rt_to_ui.ui_clipped.load(Ordering::Relaxed);
            assert!(
                !clipped,
                "ui_clipped should be false for non-clipping signal"
            );
        }

        // Case 2: Steady state clipping (signal = 1.5, gain = 0 dB -> output = 1.5 -> clipped)
        {
            shared.rt_to_ui.ui_clipped.store(false, Ordering::Relaxed);
            let mut in_buf = vec![1.5f32; n];
            let mut out_buf = vec![0.0f32; n];

            let mut input_channels = [in_buf.as_mut_slice()];
            let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    input_channels.iter_mut().map(InputChannel::constant),
                ),
            }]);

            let output_channels = [out_buf.as_mut_slice()];
            let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
            }]);

            let input_events = InputEvents::empty();
            let mut output_events_buffer = EventBuffer::new();
            let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

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

            let clipped = shared.rt_to_ui.ui_clipped.load(Ordering::Relaxed);
            assert!(
                clipped,
                "ui_clipped should be true for steady state clipping signal (>1.0)"
            );
        }

        // Case 3: Ramp clipping (signal = 0.5, gain modulates from 0 dB to 12 dB -> gain linear = 3.98 -> output ~1.99 -> clipped)
        {
            shared.rt_to_ui.ui_clipped.store(false, Ordering::Relaxed);
            let mut in_buf = vec![0.5f32; n];
            let mut out_buf = vec![0.0f32; n];

            let mut input_channels = [in_buf.as_mut_slice()];
            let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    input_channels.iter_mut().map(InputChannel::constant),
                ),
            }]);

            let output_channels = [out_buf.as_mut_slice()];
            let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
            }]);

            use crate::clap::extensions::params::PARAM_INPUT_GAIN;
            use clack_common::events::Pckn;
            use clack_common::events::event_types::ParamValueEvent;
            use clack_common::utils::{ClapId, Cookie};

            let mut input_events_buffer = EventBuffer::new();
            let val_event = ParamValueEvent::new(
                0,
                ClapId::new(PARAM_INPUT_GAIN),
                Pckn::match_all(),
                12.0, // +12 dB
                Cookie::empty(),
            );
            input_events_buffer.push(&val_event);
            let input_events = InputEvents::from_buffer(&input_events_buffer);

            let mut output_events_buffer = EventBuffer::new();
            let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

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

            let clipped = shared.rt_to_ui.ui_clipped.load(Ordering::Relaxed);
            assert!(
                clipped,
                "ui_clipped should be true for ramp clipping signal"
            );
        }
    }
}

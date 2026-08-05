// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::test_util::{self, StereoTestBuffers};
    use clack_host::prelude::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn test_irregular_block_sizes_stress() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 1,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let sizes = [1, 7, 17, 33, 53, 128, 256, 512, 1, 1];

        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);
        let mut output_events_buffer = EventBuffer::new();

        for &n in &sizes {
            let mut in_l = vec![0.1f32; n];
            let mut in_r = vec![0.2f32; n];
            let mut out_l = vec![0.0f32; n];
            let mut out_r = vec![0.0f32; n];

            let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
            let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    input_channels.iter_mut().map(InputChannel::constant),
                ),
            }]);

            let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
            let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
            }]);

            let input_events = InputEvents::empty();
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
                .unwrap_or_else(|_| panic!("Failure in process() for n={}", n));

            for i in 0..n {
                assert!(
                    out_l[i].is_finite(),
                    "NaN/Inf detected in channel L for n={}",
                    n
                );
                assert!(
                    out_r[i].is_finite(),
                    "NaN/Inf detected in channel R for n={}",
                    n
                );
            }
            let rms_l = (out_l[..n].iter().map(|x| (x * x) as f64).sum::<f64>() / n as f64).sqrt();
            let rms_r = (out_r[..n].iter().map(|x| (x * x) as f64).sum::<f64>() / n as f64).sqrt();
            assert!(
                rms_l > 0.0001 && rms_l < 10.0,
                "CLAP output L RMS out of band for n={}: {}",
                n,
                rms_l
            );
            assert!(
                rms_r > 0.0001 && rms_r < 10.0,
                "CLAP output R RMS out of band for n={}: {}",
                n,
                rms_r
            );
        }
    }

    static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_model_switching_stress() {
        let _mutex_guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 64,
            max_frames_count: 64,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let models = [
            "BossWN-nano.nam",
            "BossWN-feather.nam",
            "BossWN-standard.nam",
            "wavenet_a2_lite.nam",
        ];

        let n = 64;
        let mut bufs = StereoTestBuffers::new(n, 0.0, 0.0);
        let state_ext = test_util::get_state_ext(&mut plugin_instance);

        for i in 0..1000 {
            if i % 50 == 0 {
                let model_name = models[(i / 50) % models.len()];
                let path = neural_amp_modeler_rs::testing::fixtures::model_path(model_name);

                let params = test_util::make_default_params(Some(path));
                let state_bytes = serde_json::to_vec(&params).unwrap();
                let mut handle = plugin_instance.plugin_handle();
                let prev_counter = shared.cold.model_load_counter.load(Ordering::Relaxed);
                state_ext
                    .load(&mut handle, &mut state_bytes.as_slice())
                    .expect("Failed to load state");

                let current_counter = shared.cold.model_load_counter.load(Ordering::Relaxed);
                assert!(
                    current_counter > prev_counter,
                    "Model load counter did not increment after loading {}, indicating the load failed.",
                    model_name
                );
            }

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

            test_util::assert_zero_alloc(&format!("process cycle {}", i), || {
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
            });
        }
    }

    #[test]
    fn test_parameter_modulation_stress() {
        let _mutex_guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let n = 512;
        let mut bufs = StereoTestBuffers::new(n, 0.5, 0.5);

        // Pre-warm the resampler with the DC signal to avoid step response ringing
        {
            let mut input_channels = [bufs.in_l.as_mut_slice(), bufs.in_r.as_mut_slice()];
            let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    input_channels.iter_mut().map(InputChannel::constant),
                ),
            }]);

            let mut out_l_pre = vec![0.0f32; n];
            let mut out_r_pre = vec![0.0f32; n];
            let output_channels = [out_l_pre.as_mut_slice(), out_r_pre.as_mut_slice()];
            let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
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
                .expect("Failure in pre-warm");
        }

        let mut output_events_buffer = EventBuffer::new();

        use crate::clap::extensions::params::PARAM_INPUT_GAIN;
        use clack_common::events::Pckn;
        use clack_common::events::event_types::ParamValueEvent;
        use clack_common::utils::{ClapId, Cookie};

        let mut input_events_buffer = EventBuffer::new();
        for i in 0..n {
            let val = -20.0 + (i as f32 / n as f32) * 40.0;
            let event = ParamValueEvent::new(
                i as u32,
                ClapId::new(PARAM_INPUT_GAIN),
                Pckn::match_all(),
                val as f64,
                Cookie::empty(),
            );
            input_events_buffer.push(&event);
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

        let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

        test_util::assert_zero_alloc("intense modulation", || {
            started_processor
                .process(
                    &input_audio,
                    &mut output_audio,
                    &input_events,
                    &mut output_events,
                    None,
                    None,
                )
                .expect("Failure in process() with intense modulation");
        });

        for i in 1..n {
            let diff_l = (bufs.out_l[i] - bufs.out_l[i - 1]).abs();
            let diff_r = (bufs.out_r[i] - bufs.out_r[i - 1]).abs();
            assert!(
                diff_l < 0.05,
                "Possible zipper noise detected in channel L sample {}",
                i
            );
            assert!(
                diff_r < 0.05,
                "Possible zipper noise detected in channel R sample {}",
                i
            );
        }
    }

    #[test]
    fn test_monophonic_parameter_modulation() {
        let _mutex_guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let n = 512;
        let mut bufs = StereoTestBuffers::new(n, 0.5, 0.5);

        use crate::clap::extensions::params::{PARAM_GATE_THRESH, PARAM_INPUT_GAIN};
        use clack_common::events::Pckn;
        use clack_common::events::event_types::{ParamModEvent, ParamValueEvent};
        use clack_common::utils::{ClapId, Cookie};

        // 1. Base case: No modulation, base gain = 0dB.
        {
            let mut input_events_buffer = EventBuffer::new();
            let val_event = ParamValueEvent::new(
                0,
                ClapId::new(PARAM_INPUT_GAIN),
                Pckn::match_all(),
                0.0,
                Cookie::empty(),
            );
            input_events_buffer.push(&val_event);

            let gate_event = ParamValueEvent::new(
                0,
                ClapId::new(PARAM_GATE_THRESH),
                Pckn::match_all(),
                -90.0,
                Cookie::empty(),
            );
            input_events_buffer.push(&gate_event);

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
                .expect("Failure in base case process");

            for (i, &val) in bufs.out_l.iter().enumerate().take(n) {
                assert!(
                    (val - 0.5).abs() < 1e-4,
                    "Base case L sample {}: {}",
                    i,
                    val
                );
            }
        }

        // 2. Modulation
        {
            let mut input_events_buffer = EventBuffer::new();
            let mod_event = ParamModEvent::new(
                0,
                ClapId::new(PARAM_INPUT_GAIN),
                Pckn::match_all(),
                6.0,
                Cookie::empty(),
            );
            input_events_buffer.push(&mod_event);

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

            let mut output_events_buffer = EventBuffer::new();
            let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

            test_util::assert_zero_alloc("process with modulation", || {
                started_processor
                    .process(
                        &input_audio,
                        &mut output_audio,
                        &input_events,
                        &mut output_events,
                        None,
                        None,
                    )
                    .expect("Failure in process with modulation");
            });

            assert!(
                bufs.out_l[n - 1] > 0.6,
                "Modulation not applied, last sample: {}",
                bufs.out_l[n - 1]
            );
        }

        // 3. Gate Modulation
        {
            let mut input_events_buffer = EventBuffer::new();
            // Modulates gate threshold by +120dB (bringing effective threshold to +30dB)
            let mod_event = ParamModEvent::new(
                0,
                ClapId::new(PARAM_GATE_THRESH),
                Pckn::match_all(),
                120.0,
                Cookie::empty(),
            );
            input_events_buffer.push(&mod_event);

            let input_events = InputEvents::from_buffer(&input_events_buffer);
            let mut input_channels = [bufs.in_l.as_mut_slice(), bufs.in_r.as_mut_slice()];
            let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    input_channels.iter_mut().map(InputChannel::constant),
                ),
            }]);

            let mut out_l_gate = vec![0.0f32; n];
            let mut out_r_gate = vec![0.0f32; n];
            let output_channels = [out_l_gate.as_mut_slice(), out_r_gate.as_mut_slice()];
            let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
            }]);

            let mut output_events_buffer = EventBuffer::new();
            let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

            // Processes the first block containing the modulation event
            started_processor
                .process(
                    &input_audio,
                    &mut output_audio,
                    &input_events,
                    &mut output_events,
                    None,
                    None,
                )
                .expect("Failure in process with gate modulation");

            // Processes 5 more blocks without new modulation events to allow the gate FSM to transition:
            // Open -> hold (2048 frames) -> FadingOut (256 frames) -> Closed
            let empty_events = InputEvents::empty();
            for _ in 0..5 {
                out_l_gate.fill(0.0);
                out_r_gate.fill(0.0);

                let mut input_channels = [bufs.in_l.as_mut_slice(), bufs.in_r.as_mut_slice()];
                let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_input_only(
                        input_channels.iter_mut().map(InputChannel::constant),
                    ),
                }]);

                let output_channels = [out_l_gate.as_mut_slice(), out_r_gate.as_mut_slice()];
                let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
                }]);

                let mut output_events_buffer = EventBuffer::new();
                let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

                started_processor
                    .process(
                        &input_audio,
                        &mut output_audio,
                        &empty_events,
                        &mut output_events,
                        None,
                        None,
                    )
                    .expect("Failure in subsequent gate blocks process");
            }

            // Since the effective threshold is 0dB and the signal is -6dB, the gate should close and silence the output.
            assert_eq!(
                out_l_gate[n - 1],
                0.0,
                "Gate should have silenced the signal at the last sample. Last sample: {}",
                out_l_gate[n - 1]
            );
        }
    }

    #[test]
    fn test_host_contract_violation_block_size() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 64,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
        let rt_status = &shared.cold.rt_status;
        #[cfg(debug_assertions)]
        let _ = rt_status;

        let n = 600_usize;
        let mut in_l = vec![0.1f32; n];
        let mut in_r = vec![0.2f32; n];
        let mut out_l = vec![0.0f32; n];
        let mut out_r = vec![0.0f32; n];

        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);

        let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
        let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input_channels.iter_mut().map(InputChannel::constant),
            ),
        }]);

        let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
        let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
        }]);

        let input_events = InputEvents::empty();
        let mut output_events_buffer = EventBuffer::new();
        let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

        let result = started_processor.process(
            &input_audio,
            &mut output_audio,
            &input_events,
            &mut output_events,
            None,
            None,
        );

        #[cfg(debug_assertions)]
        {
            assert!(
                result.is_err(),
                "Expected ProcessFailed in debug builds: host sent 600 frames with max_frames_count=512"
            );
        }

        #[cfg(not(debug_assertions))]
        {
            assert!(
                result.is_ok(),
                "Expected success in release builds (debug_assert compiled out)"
            );
            assert!(
                rt_status.check_flag(
                    neural_amp_modeler_rs::common::spsc::RT_STATUS_HOST_CONTRACT_VIOLATION
                ),
                "RT_STATUS_HOST_CONTRACT_VIOLATION should be set when host sends buffer exceeding max_frames_count"
            );
        }
    }
}

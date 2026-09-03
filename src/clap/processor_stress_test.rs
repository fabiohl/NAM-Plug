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

        let models = ["wavenet_a1_standard.nam", "lstm.nam", "a2_example.nam"];

        let n = 64;
        let mut bufs = StereoTestBuffers::new(n, 0.0, 0.0);
        let state_ext = test_util::get_state_ext(&mut plugin_instance);

        for i in 0..1000 {
            if i % 50 == 0 {
                let model_name = models[(i / 50) % models.len()];
                let path = crate::clap::test_util::model_path(model_name);

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

        assert!(
            result.is_err(),
            "Expected Err(PluginError) across all build profiles when host sends 600 frames with max_frames_count=512"
        );
        assert!(
            rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HOST_CONTRACT_VIOLATION),
            "RT_STATUS_HOST_CONTRACT_VIOLATION must be set on block size contract violation"
        );
        // Verify fail-safe deterministic zeroing of output buffer
        for &s in &out_l {
            assert_eq!(
                s, 0.0,
                "Output channel L must be zeroed on contract violation"
            );
        }
        for &s in &out_r {
            assert_eq!(
                s, 0.0,
                "Output channel R must be zeroed on contract violation"
            );
        }
    }

    #[test]
    fn test_non_finite_input_containment_and_recovery() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 1,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
        let rt_status = &shared.cold.rt_status;

        let n = 256_usize;
        let mut in_l = vec![0.0f32; n];
        let mut in_r = vec![0.0f32; n];
        in_l[10] = f32::NAN;
        in_l[50] = f32::INFINITY;
        in_r[20] = f32::NEG_INFINITY;

        let mut out_l = vec![1.0f32; n];
        let mut out_r = vec![1.0f32; n];

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

        assert!(
            result.is_ok(),
            "Non-finite block should be contained and process cleanly"
        );
        assert!(
            rt_status.check_flag(
                neural_amp_modeler_rs::common::spsc::RT_STATUS_NON_FINITE_INPUT_DETECTED
            ),
            "RT_STATUS_NON_FINITE_INPUT_DETECTED should be set when input contains NaN/Inf"
        );

        for i in 0..n {
            assert!(
                out_l[i].is_finite(),
                "Output L sample {} must be finite even after hostile NaN/Inf input",
                i
            );
            assert!(
                out_r[i].is_finite(),
                "Output R sample {} must be finite even after hostile NaN/Inf input",
                i
            );
        }

        // Verify recovery with subsequent clean audio
        let mut clean_in_l = vec![0.1f32; n];
        let mut clean_in_r = vec![0.1f32; n];
        let mut clean_out_l = vec![0.0f32; n];
        let mut clean_out_r = vec![0.0f32; n];

        let mut clean_input_channels = [clean_in_l.as_mut_slice(), clean_in_r.as_mut_slice()];
        let clean_input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                clean_input_channels.iter_mut().map(InputChannel::constant),
            ),
        }]);

        let clean_output_channels = [clean_out_l.as_mut_slice(), clean_out_r.as_mut_slice()];
        let mut clean_output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(clean_output_channels.into_iter()),
        }]);

        let result_clean = started_processor.process(
            &clean_input_audio,
            &mut clean_output_audio,
            &input_events,
            &mut output_events,
            None,
            None,
        );
        assert!(
            result_clean.is_ok(),
            "Clean audio after recovery should process successfully"
        );
        for i in 0..n {
            assert!(clean_out_l[i].is_finite());
            assert!(clean_out_r[i].is_finite());
        }
    }

    /// The non-finite reset must use the effective model rate of the
    /// active chain (post-resample), never a hard-coded 48 kHz. Exercised at
    /// host rates 44.1 kHz and 96 kHz with a NaN burst followed by a sine;
    /// output must be finite and the subsequent valid block must have a
    /// defined response. No assertion pins the internal rate to 48000.
    #[test]
    fn test_non_finite_reset_uses_effective_model_rate() {
        let _mutex_guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        for &host_rate in &[44100.0_f64, 96000.0_f64] {
            let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

            // Load a real model BEFORE activate so it is installed by
            // flush_pending_model() and the non-finite reset branch runs.
            let path = crate::clap::test_util::model_path("wavenet_a1_standard.nam");
            let params = test_util::make_default_params(Some(path));
            test_util::load_plugin_state(&mut plugin_instance, &params);

            let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
            let rt_status = &shared.cold.rt_status;
            assert!(
                shared.cold.model_load_counter.load(Ordering::Relaxed) > 0,
                "model must be loaded before activate at {} Hz",
                host_rate
            );
            let effective_model_rate = shared.cold.model_sample_rate.load(Ordering::Relaxed);
            assert!(
                effective_model_rate > 0,
                "model_sample_rate must be set when a model is loaded"
            );

            let audio_config = PluginAudioConfiguration {
                sample_rate: host_rate,
                min_frames_count: 64,
                max_frames_count: 512,
            };
            let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
            let mut started_processor = stopped_processor.start_processing().unwrap();

            let n = 256_usize;

            let fill_sine = |buf_l: &mut [f32], buf_r: &mut [f32]| {
                for i in 0..n {
                    let s = 0.2 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).sin();
                    buf_l[i] = s;
                    buf_r[i] = s;
                }
            };

            // Pre-block: finite sine so the pipeline is warm (lazy statics,
            // smoothers) before the NaN burst and the model is installed.
            let mut warm = StereoTestBuffers::new(n, 0.0, 0.0);
            fill_sine(&mut warm.in_l, &mut warm.in_r);
            {
                let mut input_channels = [warm.in_l.as_mut_slice(), warm.in_r.as_mut_slice()];
                let input_audio = warm.input_ports.with_input_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_input_only(
                        input_channels.iter_mut().map(InputChannel::constant),
                    ),
                }]);
                let output_channels = [warm.out_l.as_mut_slice(), warm.out_r.as_mut_slice()];
                let mut output_audio = warm.output_ports.with_output_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
                }]);
                let input_events = InputEvents::empty();
                let mut output_events = OutputEvents::from_buffer(&mut warm.output_events_buffer);
                started_processor
                    .process(
                        &input_audio,
                        &mut output_audio,
                        &input_events,
                        &mut output_events,
                        None,
                        None,
                    )
                    .expect("warm block must process cleanly");
            }
            assert!(
                !rt_status
                    .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED),
                "loaded model must be installed at {} Hz",
                host_rate
            );

            // NaN burst block: containment + reset with the effective rate.
            let mut hostile = StereoTestBuffers::new(n, 0.0, 0.0);
            fill_sine(&mut hostile.in_l, &mut hostile.in_r);
            hostile.in_l[10] = f32::NAN;
            hostile.in_l[50] = f32::INFINITY;
            hostile.in_r[20] = f32::NEG_INFINITY;
            {
                let mut input_channels = [hostile.in_l.as_mut_slice(), hostile.in_r.as_mut_slice()];
                let input_audio = hostile.input_ports.with_input_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_input_only(
                        input_channels.iter_mut().map(InputChannel::constant),
                    ),
                }]);
                let output_channels = [hostile.out_l.as_mut_slice(), hostile.out_r.as_mut_slice()];
                let mut output_audio =
                    hostile.output_ports.with_output_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
                    }]);
                let input_events = InputEvents::empty();
                let mut output_events =
                    OutputEvents::from_buffer(&mut hostile.output_events_buffer);

                test_util::assert_zero_alloc("non-finite reset block", || {
                    started_processor
                        .process(
                            &input_audio,
                            &mut output_audio,
                            &input_events,
                            &mut output_events,
                            None,
                            None,
                        )
                        .expect("NaN burst must be contained");
                });
            }
            assert!(
                rt_status.check_flag(
                    neural_amp_modeler_rs::common::spsc::RT_STATUS_NON_FINITE_INPUT_DETECTED
                ),
                "RT_STATUS_NON_FINITE_INPUT_DETECTED must be set at {} Hz",
                host_rate
            );
            for i in 0..n {
                assert!(
                    hostile.out_l[i].is_finite(),
                    "post-NaN L[{}] = {} must be finite at {} Hz",
                    i,
                    hostile.out_l[i],
                    host_rate
                );
                assert!(
                    hostile.out_r[i].is_finite(),
                    "post-NaN R[{}] = {} must be finite at {} Hz",
                    i,
                    hostile.out_r[i],
                    host_rate
                );
            }

            // Recovery: a subsequent valid sine must produce a defined,
            // finite response from the freshly reset model.
            let mut recovery = StereoTestBuffers::new(n, 0.0, 0.0);
            fill_sine(&mut recovery.in_l, &mut recovery.in_r);
            {
                let mut input_channels =
                    [recovery.in_l.as_mut_slice(), recovery.in_r.as_mut_slice()];
                let input_audio = recovery.input_ports.with_input_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_input_only(
                        input_channels.iter_mut().map(InputChannel::constant),
                    ),
                }]);
                let output_channels =
                    [recovery.out_l.as_mut_slice(), recovery.out_r.as_mut_slice()];
                let mut output_audio =
                    recovery.output_ports.with_output_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
                    }]);
                let input_events = InputEvents::empty();
                let mut output_events =
                    OutputEvents::from_buffer(&mut recovery.output_events_buffer);
                started_processor
                    .process(
                        &input_audio,
                        &mut output_audio,
                        &input_events,
                        &mut output_events,
                        None,
                        None,
                    )
                    .expect("recovery sine must process cleanly");
            }
            for i in 0..n {
                assert!(
                    recovery.out_l[i].is_finite(),
                    "recovery L[{}] = {} must be finite at {} Hz",
                    i,
                    recovery.out_l[i],
                    host_rate
                );
                assert!(
                    recovery.out_r[i].is_finite(),
                    "recovery R[{}] = {} must be finite at {} Hz",
                    i,
                    recovery.out_r[i],
                    host_rate
                );
            }
            let rms_l: f64 = recovery
                .out_l
                .iter()
                .map(|&x| (x as f64).powi(2))
                .sum::<f64>()
                / n as f64;
            let rms_r: f64 = recovery
                .out_r
                .iter()
                .map(|&x| (x as f64).powi(2))
                .sum::<f64>()
                / n as f64;
            assert!(
                rms_l.sqrt() > 0.0001,
                "recovery L RMS must be non-trivial at {} Hz (got {})",
                host_rate,
                rms_l.sqrt()
            );
            assert!(
                rms_r.sqrt() > 0.0001,
                "recovery R RMS must be non-trivial at {} Hz (got {})",
                host_rate,
                rms_r.sqrt()
            );
        }
    }

    #[test]
    fn test_parameter_sanitization_host_fuzzing() {
        use crate::clap::extensions::params::{
            PARAM_ACTIVATION, PARAM_ACTIVE_MODEL, PARAM_ADAPTIVE_COMPUTE, PARAM_BYPASS,
            PARAM_GATE_THRESH, PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN, PARAM_OVERSAMPLE,
            PARAM_SLIM_OVERRIDE, sanitize_param_value,
        };

        // NaN sanitization
        assert_eq!(sanitize_param_value(PARAM_INPUT_GAIN, f32::NAN), 0.0);
        assert_eq!(sanitize_param_value(PARAM_OUTPUT_GAIN, f32::NAN), 0.0);
        assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, f32::NAN), -90.0);
        assert_eq!(sanitize_param_value(PARAM_BYPASS, f32::NAN), 0.0);
        assert_eq!(sanitize_param_value(PARAM_ACTIVE_MODEL, f32::NAN), 0.0);
        assert_eq!(sanitize_param_value(PARAM_ADAPTIVE_COMPUTE, f32::NAN), 1.0);
        assert_eq!(sanitize_param_value(PARAM_SLIM_OVERRIDE, f32::NAN), 0.0);
        assert_eq!(sanitize_param_value(PARAM_OVERSAMPLE, f32::NAN), 0.0);
        assert_eq!(sanitize_param_value(PARAM_ACTIVATION, f32::NAN), 1.0);

        // Non-finite (NaN/Inf) sanitization replaces with defaults
        assert_eq!(sanitize_param_value(PARAM_INPUT_GAIN, f32::INFINITY), 0.0);
        assert_eq!(
            sanitize_param_value(PARAM_INPUT_GAIN, f32::NEG_INFINITY),
            0.0
        );
        assert_eq!(
            sanitize_param_value(PARAM_GATE_THRESH, f32::INFINITY),
            -90.0
        );
        assert_eq!(
            sanitize_param_value(PARAM_GATE_THRESH, f32::NEG_INFINITY),
            -90.0
        );
        assert_eq!(sanitize_param_value(PARAM_BYPASS, f32::INFINITY), 0.0);
        assert_eq!(sanitize_param_value(PARAM_BYPASS, f32::NEG_INFINITY), 0.0);

        // Finite out-of-bounds sanitization clamps to valid range
        assert_eq!(sanitize_param_value(PARAM_INPUT_GAIN, 1000.0), 30.0);
        assert_eq!(sanitize_param_value(PARAM_INPUT_GAIN, -1000.0), -96.0);
        assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, 10.0), -40.0);
        assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, -150.0), -90.0);
        assert_eq!(sanitize_param_value(PARAM_BYPASS, 10.0), 1.0);
        assert_eq!(sanitize_param_value(PARAM_BYPASS, -10.0), 0.0);

        // Stepped & clamping bounds
        assert_eq!(sanitize_param_value(PARAM_ADAPTIVE_COMPUTE, 50.0), 2.0);
        assert_eq!(sanitize_param_value(PARAM_ADAPTIVE_COMPUTE, -10.0), 0.0);
        assert_eq!(sanitize_param_value(PARAM_ADAPTIVE_COMPUTE, 1.4), 1.0);
        assert_eq!(sanitize_param_value(PARAM_ADAPTIVE_COMPUTE, 1.6), 2.0);
    }
}

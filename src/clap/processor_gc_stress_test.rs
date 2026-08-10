// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::test_util::{self, StereoTestBuffers};
    use clack_host::prelude::*;
    use std::sync::atomic::Ordering;

    #[test]
    #[ignore]
    fn test_gc_stress_1000_swaps() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let state_ext = test_util::get_state_ext(&mut plugin_instance);

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 64,
            max_frames_count: 64,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let models = ["wavenet_a1_standard.nam", "lstm.nam", "a2_example.nam"];

        let n = 64;
        let mut bufs = StereoTestBuffers::new(n, 0.0, 0.0);

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let rt_status = &shared.cold.rt_status;

        // Perform exactly 24 model swaps first to test limit of SPSC + parking lot (48 slots).
        // CLAP is native mono — model_r was removed.
        // 1st swap pushes 1 item (old_resampler, since model_l is initially None).
        // Subsequent swaps push 2 items each (old_model_l + old_resampler).
        // Total items pushed for 24 swaps (i = 0 to 23) is exactly 1 + 23 * 2 = 47 items.
        for i in 0..24 {
            let model_name = models[i % models.len()];
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
        }

        // Verify that no GC overflow/leak occurred as we have not exceeded SPSC + parking lot (47 <= 48)
        assert!(
            !rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_OVERFLOW),
            "GC overflow flag was set prematurely!"
        );

        // Perform 1 more swap (the 25th swap). This pushes 2 more items.
        // Total items pushed = 47 + 2 = 49 items.
        // This exceeds SPSC + parking lot limit of 48 items, so 1 item spills into the
        // overflow buffer. RT_STATUS_GC_OVERFLOW is NOT triggered here: the flag is
        // conditioned on `push` returning `true` (actual overwrite/leak), and the 64-slot
        // buffer is still far from full.
        {
            let model_name = models[24 % models.len()];
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
        }

        // Verify that the GC overflow flag is NOT set: the overflow buffer has 64 slots,
        // so a single spill doesn't cause an overwrite.
        assert!(
            !rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_OVERFLOW),
            "GC overflow flag was set prematurely — only 1 item entered the 64-slot overflow buffer!"
        );

        // Perform a complete drain to reclaim all 49 items from the channels and overflow buffer
        plugin_instance.call_on_main_thread_callback();
        // One process cycle to move items from the parking lot to the now empty SPSC channel
        {
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
        }
        plugin_instance.call_on_main_thread_callback();

        // Clear the overflow flag manually now that the system is fully drained and clean
        rt_status.clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_OVERFLOW);

        // Perform the remaining 975 model swaps to reach 1000 model swaps in total.
        // We will drain every 10 swaps (20 items), which fits comfortably within the 32-capacity SPSC channel,
        // so no overflow should occur during this loop.
        for i in 25..1000 {
            let model_name = models[i % models.len()];
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

            // Periodic drain of the SPSC channel
            if i % 10 == 0 {
                plugin_instance.call_on_main_thread_callback();
            }
        }

        // Final cleanup and drainage of any leftover items
        for _ in 0..5 {
            plugin_instance.call_on_main_thread_callback();
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
        }
        plugin_instance.call_on_main_thread_callback();

        // Verify that the GC overflow flag was not set again
        assert!(
            !rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_OVERFLOW),
            "GC overflow / leak occurred during the remaining model swaps!"
        );
    }

    #[test]
    #[ignore]
    fn test_gc_drain_on_destroy_no_leak() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let state_ext = test_util::get_state_ext(&mut plugin_instance);

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 64,
            max_frames_count: 64,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let models = ["wavenet_a1_standard.nam", "lstm.nam", "a2_example.nam"];

        let n = 64;
        let mut bufs = StereoTestBuffers::new(n, 0.0, 0.0);

        let _shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // Swap models multiple times without calling housekeeping/drain on main thread
        // to accumulate items in the GC-cascade channels.
        for i in 0..5 {
            let model_name = models[i % models.len()];
            let path = crate::clap::test_util::model_path(model_name);

            let params = test_util::make_default_params(Some(path));
            let state_bytes = serde_json::to_vec(&params).unwrap();
            let mut handle = plugin_instance.plugin_handle();

            state_ext
                .load(&mut handle, &mut state_bytes.as_slice())
                .expect("Failed to load state");

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
        }

        // Deactivating and dropping the plugin instance.
        // deactivation calls `_main_thread.drain_gc_final()`, which drains the channels.
        let stopped = started_processor.stop_processing();
        plugin_instance.deactivate(stopped);

        // Under ASAN/Valgrind or leak checks (tests-long), dropping plugin_instance here
        // will verify that all models in transit are fully released and do not leak.
        drop(plugin_instance);
    }
}

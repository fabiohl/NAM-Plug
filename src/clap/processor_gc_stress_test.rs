// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::test_util::{self, StereoTestBuffers};
    use clack_host::prelude::*;
    use std::sync::atomic::Ordering;

    // on-demand: execute manually or in extended CI
    #[test]
    #[ignore = "heavy GC stress: 1000 swaps"]
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
        // 1st swap pushes 2 items (old_resampler + old_stream, since model_l is initially None).
        // Subsequent swaps push 3 items each (old_model_l + old_resampler + old_stream).
        // Total items pushed for 24 swaps (i = 0 to 23) is exactly 2 + 23 * 3 = 71 items.
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

        // Verify that no actual overwrite/leak occurred: the GC overflow flag is
        // only set on slot overwrite in the overflow buffer, not on first entry.
        assert!(
            !rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_OVERFLOW),
            "GC overflow flag was set prematurely!"
        );

        // Perform 1 more swap (the 25th swap). This pushes 3 more items.
        // Total items pushed = 71 + 3 = 74 items.
        // This exceeds SPSC + parking lot limit of 48 items, so items spill into the
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
        // so spills don't yet cause an overwrite.
        assert!(
            !rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_OVERFLOW),
            "GC overflow flag was set prematurely — only 1 item entered the 64-slot overflow buffer!"
        );

        // Perform a complete drain to reclaim all 74 items from the channels and overflow buffer
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
        // We will drain every 10 swaps (30 items: 10 * 3), which fits comfortably within the 32-capacity SPSC channel,
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

    // on-demand: execute manually or in extended CI
    #[test]
    #[ignore = "heavy GC drain-on-destroy leak check"]
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

        // Under ASAN/Valgrind or on-demand leak checks (extended CI), dropping
        // plugin_instance here will verify that all models in transit are fully
        // released and do not leak.
        drop(plugin_instance);
    }

    // Teardown contract: plugin teardown must hand the RT parking lot to the final off-RT
    // drain. With current GcItem layout (Model + Resampler + Streaming = 3 per swap):
    // swap #1 pushes 2 (model None → no Model, only resampler + stream), each later
    // swap pushes 3: 2 + 24 * 3 = 74 items (SPSC 32 + lot 16 + overflow 26).
    // Before the parking-lot handoff fix only SPSC + overflow were drained (58 items).
    #[test]
    #[ignore = "Teardown stress: 25 model swaps"]
    fn test_teardown_drains_rt_parking_lot_off_rt() {
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

        // 25 swaps WITHOUT main-thread housekeeping: the GC SPSC (32) fills up,
        // then the 16-slot RT parking lot parks items (gc_cascade), and remaining
        // items spill into the 64-slot overflow buffer. Total in flight:
        // 32 (SPSC) + 16 (lot) + 26 (overflow) = 74 GcItems.
        // Swap #1 pushes 2 items (old resampler + old stream — model was None); each later
        // swap pushes 3 (old model + old resampler + old stream): 2 + 24 * 3 = 74.
        for i in 0..25 {
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

        // Confirm the cascade reached the parking lot: no overwrite
        // occurred in the overflow buffer (still far from 64-slot overwrite), so the flag stays clear.
        assert!(
            !rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_OVERFLOW),
            "GC overflow flag was set prematurely — items spilled into the overflow buffer but none were overwritten!"
        );

        let drains_before = rt_status.drains.load(Ordering::Relaxed);

        // Shutdown: stop the audio thread and deactivate. deactivate() hands
        // `&mut processor.parking_lot` to drain_gc_final — the single-owner
        // handoff — so one call drops SPSC + overflow + 16 slots off-RT.
        let stopped = started_processor.stop_processing();
        plugin_instance.deactivate(stopped);

        let drains_delta = rt_status.drains.load(Ordering::Relaxed) - drains_before;
        assert_eq!(
            drains_delta, 74,
            "deactivate must account for all 74 in-flight GcItems \
             (32 SPSC + 16 RT parking lot + 26 overflow); before the fix the \
             parking lot was invisible and only 58 were drained"
        );

        // The last quantum must not have allocated on the audio thread.
        assert!(
            !rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC),
            "RT_STATUS_HEAP_ALLOC was set — a GcItem drop happened on the audio thread"
        );

        // Dropping the instance must not drop any remaining GcItem (all were
        // released by the drain above); plugin_instance drop is a leak check.
        drop(plugin_instance);
    }
}

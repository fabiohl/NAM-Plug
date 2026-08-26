// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! T1.3 — Strict Cardinality: 100% host output write coverage.
//!
//! The streaming resample adapter (T1.2/F-PERF-002) guarantees that **every**
//! audio callback writes exactly `frames_count` samples to each active output
//! channel. These tests fill the output ports with a distinctive non-zero
//! sentinel before `process()` and assert that no sentinel survives in
//! `[0..frames_count)` — proving the previous buffer suffix residue / clamp
//! behavior in `output_offset` is gone.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::params::{PARAM_GATE_THRESH, PARAM_INPUT_GAIN};
    use crate::clap::test_util::{self, StereoTestBuffers, TestHost};
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::{ClapId, Cookie};
    use clack_host::prelude::*;
    use std::sync::atomic::Ordering;

    type Started = StartedPluginAudioProcessor<TestHost>;

    /// Distinctive sentinel: far outside any audio range, never produced by the
    /// DSP chain (finite, negative, ~ -6.3e18). A surviving sentinel sample
    /// proves the plugin never wrote that output slot.
    const SENTINEL: f32 = f32::from_bits(0xDEADBEEF);

    /// Irregular host block sizes (from the T1.2 acceptance matrix) plus the
    /// maximum supported sub-block, covering single-sample through 8192.
    const IRREGULAR_BLOCKS: &[usize] = &[1, 7, 31, 63, 64, 65, 127, 256, 512, 1024, 8192];

    /// Full sample-rate matrix from the T1.3 acceptance criteria.
    const RATES: &[u32] = &[44100, 48000, 88200, 96000, 176400, 192000];

    /// Builds a test plugin with a real 48 kHz model loaded (so every host rate
    /// != 48000 exercises the resampler) and returns the started processor.
    ///
    /// `max_block` is the host `max_frames_count`; `warmup_blocks` of 64-sample
    /// silence drain the SPSC model/stream swap and converge the gate so the
    /// sentinel probes run on a settled pipeline.
    fn setup_with_model(
        instance: &mut PluginInstance<TestHost>,
        sample_rate: f64,
        max_block: u32,
        warmup: usize,
    ) -> Started {
        let audio_config = PluginAudioConfiguration {
            sample_rate,
            min_frames_count: 1,
            max_frames_count: max_block,
        };
        let stopped = instance.activate(|_, _| (), audio_config).unwrap();
        let mut started = stopped.start_processing().unwrap();

        // Load a real model via state restore (atomic commit builds the stream
        // on the main thread with the real host rate + buffer size).
        let state_ext = test_util::get_state_ext(instance);
        let params = test_util::make_default_params(Some(test_util::model_path("a2_example.nam")));
        let state_bytes = serde_json::to_vec(&params).unwrap();
        let mut handle = instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut state_bytes.as_slice())
            .expect("Failed to load model state");

        // Drain the SPSC (model + stream swap) and warm the gate/smoothers.
        let mut bufs = StereoTestBuffers::new(64, 0.05, 0.03);
        for _ in 0..warmup {
            process_stereo_block(&mut started, &mut bufs, InputEvents::empty());
        }
        started
    }

    /// Runs one `process()` with the given input/event buffers and returns the
    /// post-process output slices.
    fn process_with_sentinel_outputs(
        started: &mut Started,
        in_l: &mut [f32],
        in_r: &mut [f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
        input_events: &InputEvents<'_>,
    ) {
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
        let mut output_events_buffer = EventBuffer::new();
        let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

        started
            .process(
                &input_audio,
                &mut output_audio,
                input_events,
                &mut output_events,
                None,
                None,
            )
            .expect("process() failed");
    }

    /// Runs one stereo `process()` on the shared `StereoTestBuffers`.
    fn process_stereo_block(
        started: &mut Started,
        bufs: &mut StereoTestBuffers,
        input_events: InputEvents<'_>,
    ) {
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
        started
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .expect("process() failed");
    }

    /// Asserts 100% write coverage: no sentinel residue and only finite samples
    /// in `[0..n)` of both output channels.
    fn assert_full_coverage(out_l: &[f32], out_r: &[f32], n: usize, rate: u32) {
        assert!(out_l.len() >= n && out_r.len() >= n);
        for i in 0..n {
            assert_ne!(
                out_l[i], SENTINEL,
                "L sample {i}@[{n}] left unwritten at {rate} Hz (sentinel residue)"
            );
            assert_ne!(
                out_r[i], SENTINEL,
                "R sample {i}@[{n}] left unwritten at {rate} Hz (sentinel residue)"
            );
            assert!(
                out_l[i].is_finite(),
                "L sample {i}@[{n}] non-finite at {rate} Hz"
            );
            assert!(
                out_r[i].is_finite(),
                "R sample {i}@[{n}] non-finite at {rate} Hz"
            );
        }
    }

    #[test]
    fn test_sentinel_full_write_coverage_rates_matrix() {
        // Acceptance criterion: sentinel tests over the full sample-rate matrix
        // (44.1/48/88.2/96/176.4/192 kHz) with irregular blocks prove 100%
        // write coverage — no sentinel survives in any output suffix.
        for &rate in RATES {
            let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
            let mut started = setup_with_model(&mut plugin_instance, rate as f64, 512, 8);

            for &n in IRREGULAR_BLOCKS {
                // Skip oversized blocks when the configured max is 512: those are
                // covered by the dedicated large-block test.
                if n > 512 {
                    continue;
                }
                let mut in_l = vec![0.05f32; n];
                let mut in_r = vec![0.03f32; n];
                let mut out_l = vec![SENTINEL; n];
                let mut out_r = vec![SENTINEL; n];

                // A few iterations per size so priming + phase round-trips settle.
                for _ in 0..8 {
                    out_l.fill(SENTINEL);
                    out_r.fill(SENTINEL);
                    process_with_sentinel_outputs(
                        &mut started,
                        &mut in_l,
                        &mut in_r,
                        &mut out_l,
                        &mut out_r,
                        &InputEvents::empty(),
                    );
                    assert_full_coverage(&out_l, &out_r, n, rate);
                }
            }
        }
    }

    #[test]
    fn test_sentinel_full_write_coverage_large_block_8192() {
        // Blocks at the MAX_RESAMP_BUF boundary exercise the sub-block
        // recursion + the streaming adapter's internal chunking.
        for &rate in &[44100u32, 96000, 192000] {
            let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
            let mut started = setup_with_model(&mut plugin_instance, rate as f64, 8192, 4);

            let n = 8192;
            let mut in_l = vec![0.05f32; n];
            let mut in_r = vec![0.03f32; n];
            let mut out_l = vec![SENTINEL; n];
            let mut out_r = vec![SENTINEL; n];
            for _ in 0..4 {
                out_l.fill(SENTINEL);
                out_r.fill(SENTINEL);
                process_with_sentinel_outputs(
                    &mut started,
                    &mut in_l,
                    &mut in_r,
                    &mut out_l,
                    &mut out_r,
                    &InputEvents::empty(),
                );
                assert_full_coverage(&out_l, &out_r, n, rate);
            }
        }
    }

    #[test]
    fn test_sentinel_full_write_coverage_in_place() {
        // CLAP in-place pair (host reuses the same buffer for input/output):
        // the plugin must still overwrite every sample of the output region.
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let mut started = setup_with_model(&mut plugin_instance, 44100.0, 256, 8);

        let n = 256;
        let mut buf_l = vec![0.1f32; n];
        let mut buf_r = vec![0.2f32; n];
        let l_ptr = buf_l.as_mut_ptr();
        let r_ptr = buf_r.as_mut_ptr();

        // SAFETY: mirrors the CLAP FFI contract — input and output share the
        // same buffer; the slices only register raw pointers, never alias here.
        unsafe {
            let in_l = std::slice::from_raw_parts_mut(l_ptr, n);
            let out_l = std::slice::from_raw_parts_mut(l_ptr, n);
            let in_r = std::slice::from_raw_parts_mut(r_ptr, n);
            let out_r = std::slice::from_raw_parts_mut(r_ptr, n);

            // Prime with the sentinel so unwritten slots are detectable, then
            // overwrite with the input so the in-place probe is meaningful.
            buf_l.fill(SENTINEL);
            buf_r.fill(SENTINEL);

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

            started
                .process(
                    &input_audio,
                    &mut output_audio,
                    &input_events,
                    &mut output_events,
                    None,
                    None,
                )
                .expect("process() failed");
        }

        for i in 0..n {
            assert_ne!(
                buf_l[i], SENTINEL,
                "InPlace L sample {i} left unwritten at 44100 Hz"
            );
            assert_ne!(
                buf_r[i], SENTINEL,
                "InPlace R sample {i} left unwritten at 44100 Hz"
            );
            assert!(buf_l[i].is_finite() && buf_r[i].is_finite());
        }
    }

    #[test]
    fn test_sentinel_automation_subblock_full_coverage() {
        // Sample-accurate automation splits the block into sub-blocks: every
        // sub-block must still deliver its exact share so the union covers the
        // whole host buffer (previously the `output_offset` clamp could leave
        // the suffix unwritten).
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let mut started = setup_with_model(&mut plugin_instance, 44100.0, 256, 8);

        let n = 256;
        let mut in_l = vec![0.05f32; n];
        let mut in_r = vec![0.03f32; n];
        let mut out_l = vec![SENTINEL; n];
        let mut out_r = vec![SENTINEL; n];

        // Open the gate low so the wet path runs throughout the block.
        let mut input_events_buffer = EventBuffer::new();
        input_events_buffer.push(&ParamValueEvent::new(
            0,
            ClapId::new(PARAM_GATE_THRESH),
            Pckn::match_all(),
            -90.0,
            Cookie::empty(),
        ));
        input_events_buffer.push(&ParamValueEvent::new(
            64,
            ClapId::new(PARAM_INPUT_GAIN),
            Pckn::match_all(),
            0.0,
            Cookie::empty(),
        ));
        input_events_buffer.push(&ParamValueEvent::new(
            128,
            ClapId::new(PARAM_INPUT_GAIN),
            Pckn::match_all(),
            -12.0,
            Cookie::empty(),
        ));

        out_l.fill(SENTINEL);
        out_r.fill(SENTINEL);
        let input_events = InputEvents::from_buffer(&input_events_buffer);
        process_with_sentinel_outputs(
            &mut started,
            &mut in_l,
            &mut in_r,
            &mut out_l,
            &mut out_r,
            &input_events,
        );
        assert_full_coverage(&out_l, &out_r, n, 44100);
    }

    #[test]
    fn test_streaming_no_glitch_stress_fractional() {
        // Long stress at a fractional rate (44.1 kHz → 48 kHz model) over
        // irregular blocks: no NaN/Inf, bounded RMS (no exploded/glitchy
        // output) and full write coverage on every callback.
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
        let mut started = setup_with_model(&mut plugin_instance, 44100.0, 512, 8);

        let shared_model = shared;
        assert!(
            shared_model.cold.model_load_counter.load(Ordering::Relaxed) > 0,
            "model must be loaded for the streaming stress"
        );

        let sizes = [1, 7, 31, 63, 64, 65, 127, 256, 512, 1, 1];
        let mut total_samples = 0usize;
        let mut sum_sq = 0.0f64;
        let mut non_silent_blocks = 0usize;

        for &n in &sizes {
            let mut in_l = vec![0.05f32; n];
            let mut in_r = vec![0.03f32; n];
            let mut out_l = vec![SENTINEL; n];
            let mut out_r = vec![SENTINEL; n];

            for _ in 0..64 {
                out_l.fill(SENTINEL);
                out_r.fill(SENTINEL);
                process_with_sentinel_outputs(
                    &mut started,
                    &mut in_l,
                    &mut in_r,
                    &mut out_l,
                    &mut out_r,
                    &InputEvents::empty(),
                );
                assert_full_coverage(&out_l, &out_r, n, 44100);
                total_samples += n;
                for &s in out_l.iter().chain(out_r.iter()) {
                    sum_sq += (s as f64) * (s as f64);
                }
                if out_l.iter().any(|&s| s.abs() > 1e-5) {
                    non_silent_blocks += 1;
                }
            }
        }

        assert!(
            non_silent_blocks > 0,
            "wet path never produced non-silent output at 44.1 kHz"
        );
        let rms = (sum_sq / (2 * total_samples) as f64).sqrt();
        assert!(
            rms > 1e-4 && rms < 10.0,
            "44.1 kHz streaming output RMS out of band: {rms}"
        );
    }
}

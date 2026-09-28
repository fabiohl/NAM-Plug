// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Block-size-invariant cab-sim stage (`process_block` adoption).
//!
//! Validates that the plugin's cab-sim stage drives the convolution engine
//! through the block-agnostic [`CabSimAdapter::process_block`] driver, so the
//! audio output is invariant to the host block size and the algorithmic
//! latency tracks the fixed partition instead of the host quantum:
//!
//! * The same input signal delivered in different block sizes (partition-sized,
//!   odd 333, and smaller quanta) produces the same wet sample sequence, with
//!   exact stream cardinality, no `RT_STATUS_CABSIM_CONTRACT_VIOLATION`, and
//!   the documented causal underrun prefix `block·(⌈P/block⌉−1)` (0 when the
//!   host block equals the partition) — mirroring the engine's own
//!   `process_block` certification. Host blocks that tile the partition lock
//!   the output FIFO phase after the prefix; non-tiling sizes (333) leave
//!   periodic causal underrun gaps whose removal restores the aligned stream.
//! * The latency announced to the host (`PluginLatency::get()`, refreshed via
//!   `HostLatency::changed()` from the main-thread housekeeping cycle) equals
//!   the fixed cab-sim partition, and the wet impulse peak physically lands at
//!   `declared + underrun_prefix` for both partition-sized and odd-sized host
//!   blocks.
//! * A DAW quantum renegotiation (activate 256 → 512, rate constant) reuses
//!   the deactivated adapter (fixed partition, no FFT rebuild): the partition
//!   latency and tail telemetry are unchanged, no new latency notification
//!   fires, oversize host blocks (512 > partition 256) process without the
//!   partition-cap contract flag, and the rendered stream stays bit-identical
//!   to the pre-renegotiation configuration.

#[cfg(test)]
mod tests {
    use crate::clap::host_harness::{
        extract_plugin_main_thread, extract_plugin_shared, make_harness_audio_processor,
        make_test_plugin_with_harness, perform_restart, process_block_harness,
    };
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::common::spsc::RT_STATUS_CABSIM_CONTRACT_VIOLATION;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    const SR: f64 = 48000.0;
    /// Host `max_frames_count` at install time — also the fixed partition of
    /// the installed cab-sim adapter (quantum-sized install policy).
    const PARTITION: usize = 512;
    const SIGNAL_LEN: usize = 4096;
    const IR_LEN: usize = 8192;

    fn audio_config(max_frames_count: u32) -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: SR,
            min_frames_count: 64,
            max_frames_count,
        }
    }

    /// Writes a deterministic pseudo-noise IR with a slow exponential decay
    /// (every partition carries measurable energy, mirroring the
    /// `processor_tail_rearm_test` fixture).
    fn write_decay_ir(name: &str) -> PathBuf {
        let mut ir = vec![0.0f32; IR_LEN];
        let mut x = 0x1234_5678u32;
        for (i, s) in ir.iter_mut().enumerate() {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = (x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            *s = n * 0.999f32.powi(i as i32);
        }
        let path = crate::clap::test_util::tmp_path(&format!("{name}.wav"));
        neural_amp_modeler_rs::testing::wav::write_wav_f32(&path, &ir, SR as u32)
            .expect("write decay IR WAV");
        path
    }

    /// Writes an IR whose impulse sits exactly `partition` samples in — a pure
    /// `partition`-sample pre-delay, so the wet response of an input impulse
    /// lands exactly `partition` samples after the input.
    fn write_pre_delay_ir(name: &str) -> PathBuf {
        let mut ir = vec![0.0f32; IR_LEN];
        ir[PARTITION] = 1.0;
        let path = crate::clap::test_util::tmp_path(&format!("{name}.wav"));
        neural_amp_modeler_rs::testing::wav::write_wav_f32(&path, &ir, SR as u32)
            .expect("write pre-delay IR WAV");
        path
    }

    /// Sustained pseudo-noise well above the default -70 dB gate threshold, so
    /// the gate FSM stays steadily Open and the wet chain is free of
    /// block-energy-dependent transitions for the invariance comparisons.
    fn test_signal(n: usize) -> Vec<f32> {
        let mut x = 0x9E37_79B9u32;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                ((x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * 0.25
            })
            .collect()
    }

    /// Activates the plugin, installs the staged IR and restarts the host per
    /// the Strict Restart Policy. Returns the started processor; the caller
    /// owns the IR path for cleanup.
    fn setup_with_ir(
        instance: &mut PluginInstance<crate::clap::host_harness::CompleteHost>,
        state: &crate::clap::host_harness::CompleteHostState,
        config: PluginAudioConfiguration,
        ir: &Path,
    ) -> StartedPluginAudioProcessor<crate::clap::host_harness::CompleteHost> {
        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(state), config)
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");
        let mt = unsafe { &*extract_plugin_main_thread(instance) };
        mt.load_cabsim(ir).expect("load cab IR");
        perform_restart(instance, started, state, config)
    }

    /// Renders `signal` in fixed `block`-sample host blocks, returning the
    /// concatenated left-channel output.
    fn render_in_blocks(
        started: &mut StartedPluginAudioProcessor<crate::clap::host_harness::CompleteHost>,
        signal: &[f32],
        block: usize,
    ) -> Vec<f32> {
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(block) {
            let mut il = chunk.to_vec();
            let mut ir = chunk.to_vec();
            let mut ol = vec![0.0f32; chunk.len()];
            let mut or = vec![0.0f32; chunk.len()];
            let _ = process_block_harness(started, &mut il, &mut ir, &mut ol, &mut or, None);
            out.extend_from_slice(&ol);
        }
        out
    }

    /// Documented causal underrun prefix of the block-agnostic driver: the
    /// wet sample for input time 0 only flows after the first full partition
    /// is accumulated and delivered — `block·(⌈P/block⌉−1)` silent samples
    /// (0 when the host block equals or exceeds the partition).
    fn underrun_prefix(block: usize, partition: usize) -> usize {
        if block >= partition {
            0
        } else {
            block * (partition.div_ceil(block) - 1)
        }
    }

    /// Reads the cab-sim contract-violation flag of the given instance.
    fn cab_contract_flag(
        instance: &mut PluginInstance<crate::clap::host_harness::CompleteHost>,
    ) -> bool {
        let shared = unsafe { &*extract_plugin_shared(instance) };
        shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_CABSIM_CONTRACT_VIOLATION)
    }

    #[test]
    fn test_cabsim_output_bit_invariant_across_host_block_sizes() {
        let signal = test_signal(SIGNAL_LEN);
        let mut reference: Option<Vec<f32>> = None;

        // 512 = partition-sized quantum; 333 = odd size spanning partition
        // boundaries; smaller sizes exercise the input FIFO partial-tail
        // accumulation and the causal underrun delivery.
        for block in [PARTITION, 256usize, 128, 64, 333] {
            let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
            let ir = write_decay_ir(&format!("cabsim_invariance_{block}"));
            let mut started =
                setup_with_ir(&mut instance, &state, audio_config(PARTITION as u32), &ir);

            let out = render_in_blocks(&mut started, &signal, block);

            // Exact stream cardinality: every host sample yields exactly one
            // output sample regardless of the delivered block size.
            assert_eq!(
                out.len(),
                signal.len(),
                "block {block}: output cardinality must match the input stream"
            );
            // The wet path actually ran (non-silent convolution output).
            assert!(
                out.iter().any(|&s| s.abs() > 1e-3),
                "block {block}: wet output must be non-silent"
            );
            // The block-agnostic driver never flags a partition-cap contract
            // violation — including odd sizes that straddle partition bounds.
            assert!(
                !cab_contract_flag(&mut instance),
                "block {block}: block-agnostic cab-sim stage must not flag a contract violation"
            );

            // The causal underrun prefix precedes the aligned wet stream.
            let prefix = underrun_prefix(block, PARTITION);
            assert!(
                out[..prefix].iter().all(|&s| s.abs() < 1e-9),
                "block {block}: first {prefix} samples must be the silent underrun prefix"
            );

            // Strip the documented underrun prefix and any residual FIFO-phase
            // underrun gaps (a constant sub-quantum block size that does not
            // tile the partition leaves the output queue phase-shifted, exactly
            // like the legacy consumer path), then compare the wet sample
            // sequences bit-exactly across all chunkings.
            let tail: Vec<f32> = out[prefix..]
                .iter()
                .copied()
                .filter(|&s| s.abs() >= 1e-9)
                .collect();
            match reference.as_ref() {
                None => reference = Some(tail),
                Some(expected) => {
                    let k = expected.len().min(tail.len());
                    assert!(k > 0, "block {block}: aligned wet stream must not be empty");
                    assert_eq!(
                        &tail[..k],
                        &expected[..k],
                        "block {block}: wet stream must be bit-identical to the reference chunking"
                    );
                }
            }
            let _ = std::fs::remove_file(&ir);
        }
    }

    #[test]
    fn test_cabsim_declared_latency_matches_partition_and_peak_alignment() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        // No IR yet: 0-sample latency announced, no notification.
        let stopped = instance
            .activate(
                |_, _| make_harness_audio_processor(&state),
                audio_config(PARTITION as u32),
            )
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");
        instance.call_on_main_thread_callback();
        assert_eq!(
            state.latency_changed_count.load(Ordering::SeqCst),
            0,
            "initial 0-sample latency must not notify"
        );

        // Install a pre-delay IR (impulse exactly at the partition index) and
        // restart: the announced latency is the fixed partition.
        let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };
        let ir = write_pre_delay_ir("cabsim_declared_latency");
        mt.load_cabsim(&ir).expect("load pre-delay IR");
        let _started = perform_restart(
            &mut instance,
            started,
            &state,
            audio_config(PARTITION as u32),
        );

        instance.call_on_main_thread_callback();
        let handle = instance.plugin_handle();
        let ext = handle
            .get_extension::<clack_extensions::latency::PluginLatency>()
            .expect("PluginLatency extension must be registered");
        let announced = ext.get(&handle);
        assert_eq!(
            announced, PARTITION as u32,
            "PluginLatency::get() must announce the fixed partition"
        );
        assert_eq!(
            shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
            announced,
            "cab-sim partition telemetry"
        );
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            announced,
            "published combined latency must equal the announced figure"
        );
        assert_eq!(
            state.latency_changed_count.load(Ordering::SeqCst),
            1,
            "exactly one HostLatency::changed() when the partition latency lands"
        );

        // The wet impulse peak physically lands at declared + underrun_prefix:
        // the IR pre-delay (== partition) plus the documented causal buffering
        // of the delivered host blocks.
        let mut impulse = vec![0.0f32; PARTITION];
        impulse[0] = 1.0;
        let tail = vec![0.0f32; PARTITION * 4];
        for block in [PARTITION, 333usize] {
            let (_entry2, _host2, mut inst2, state2) = make_test_plugin_with_harness();
            let mut started2 =
                setup_with_ir(&mut inst2, &state2, audio_config(PARTITION as u32), &ir);
            let mut stream = render_in_blocks(&mut started2, &impulse, block);
            stream.extend_from_slice(&render_in_blocks(&mut started2, &tail, block));
            let prefix = underrun_prefix(block, PARTITION);
            let expected_peak = PARTITION + prefix;
            let peak = stream
                .iter()
                .position(|&s| s.abs() >= 0.5)
                .expect("impulse must propagate through the wet path");
            assert_eq!(
                peak, expected_peak,
                "block {block}: wet impulse peak must land at declared latency + underrun prefix"
            );
            assert!(
                !cab_contract_flag(&mut inst2),
                "block {block}: impulse alignment must not flag a contract violation"
            );
        }

        let _ = std::fs::remove_file(&ir);
    }

    #[test]
    fn test_cabsim_adapter_reused_across_quantum_renegotiation() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };
        let ir = write_decay_ir("cabsim_quantum_reneg");

        // Install at quantum 256 → partition 256.
        let stopped = instance
            .activate(
                |_, _| make_harness_audio_processor(&state),
                audio_config(256),
            )
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");
        let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };
        mt.load_cabsim(&ir).expect("load decay IR");
        let mut started = perform_restart(&mut instance, started, &state, audio_config(256));
        instance.call_on_main_thread_callback();
        assert_eq!(
            shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
            256,
            "partition must track the install-time quantum"
        );
        assert_eq!(
            shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed),
            IR_LEN as u32,
            "tail telemetry must reflect 32 partitions of 256"
        );
        assert_eq!(
            state.latency_changed_count.load(Ordering::SeqCst),
            1,
            "one notification for the 0 → 256 install"
        );

        // Reference render at the original quantum (block == partition → zero
        // underrun prefix, stream time-aligned).
        let signal = test_signal(SIGNAL_LEN);
        let before = render_in_blocks(&mut started, &signal, 256);

        // Quantum renegotiation 256 → 512 (rate constant): the block-agnostic
        // adapter is reused, keeping its fixed partition — a rebuild would
        // produce partition 512.
        let mut started = perform_restart(&mut instance, started, &state, audio_config(512));
        instance.call_on_main_thread_callback();
        assert_eq!(
            shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
            256,
            "quantum renegotiation must reuse the adapter with its fixed partition"
        );
        assert_eq!(
            shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed),
            IR_LEN as u32,
            "tail telemetry must be unchanged by the quantum renegotiation"
        );
        assert_eq!(
            state.latency_changed_count.load(Ordering::SeqCst),
            1,
            "constant latency across the renegotiation must not re-notify"
        );

        // Oversize host block (512 > partition 256): the block-agnostic driver
        // chunks internally — no partition-cap contract flag, non-silent
        // output.
        assert!(
            !cab_contract_flag(&mut instance),
            "no contract violation before the oversize render"
        );
        let after = render_in_blocks(&mut started, &signal, 512);
        assert!(
            !cab_contract_flag(&mut instance),
            "oversize host blocks over the fixed partition must not flag a contract violation"
        );
        assert!(
            after.iter().any(|&s| s.abs() > 1e-3),
            "renegotiated wet output must be non-silent"
        );

        // The in-place reset on reinstall makes the restarted timeline fresh:
        // with the fixed partition the wet stream is bit-identical across the
        // quantum renegotiation and its changed host block size (both
        // chunkings have a zero underrun prefix: 256 == partition, 512 ≥
        // partition).
        assert_eq!(
            after, before,
            "output must be bit-identical across a quantum renegotiation with a reused adapter"
        );

        let _ = std::fs::remove_file(&ir);
    }
}

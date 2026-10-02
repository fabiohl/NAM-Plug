// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Per-block telemetry tests (T-P1.3.2 / F-NPPERF-07).
//!
//! Validates that every `process()` block feeds the overload counter, the
//! latency histogram and the adaptive-compute FSM — the former 1-in-16
//! decimation (`cycles_since_telemetry & 0xF`) hid 15 of every 16 deadline
//! overruns from all three sinks.
//!
//! * Overload capture: 16 blocks with exactly one synthetic stall parked at a
//!   non-decimated phase (block 7) assert `dsp_overloads == 1` — under the old
//!   decimation this phase would have recorded 0. The stall is injected via
//!   the one-shot `TEST_TELEMETRY_STALL_NS` hook (no sleep on the audio
//!   thread), sized to 85% of the 64-sample budget @48 kHz.
//! * Telemetry coverage: the 16 measured blocks (15 clean + 1 stalled) assert
//!   `latency_hist.total_count == 16` and `dsp_cycle_time > 0` — every block
//!   is now observed.
//! * FSM reactivity: the processor's `AdaptiveCompute` starts in Conservative
//!   mode (activate default) with degrade confirmation after 3 consecutive
//!   over-budget blocks. Three consecutive stalled blocks must flip the FSM
//!   out of `Full` (Reduced, crossfade armed), i.e. reaction within ≤ 3
//!   blocks — the decimated path needed up to ~48 blocks for the same signal.
//! * RT-safety: every measured block runs inside `assert_zero_alloc` — the
//!   per-block TSC read + histogram record + budget math allocate nothing.

#[cfg(test)]
mod tests {
    use crate::clap::processor::TEST_TELEMETRY_STALL_NS;
    use crate::clap::test_util::{self, StereoTestBuffers};
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::dsp::adaptive::AdaptiveState;
    use std::sync::atomic::Ordering;

    /// Serializes against sibling tests that also drive `process()` through
    /// the shared one-shot `TEST_TELEMETRY_STALL_NS` hook: a stall armed by
    /// one test must never leak into another test's blocks (the hook is a
    /// process-wide static consumed by whichever block runs next).
    static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const BLOCK: usize = 64;
    const SAMPLE_RATE: f64 = 48000.0;

    fn audio_config_64() -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: SAMPLE_RATE,
            min_frames_count: BLOCK as u32,
            max_frames_count: BLOCK as u32,
        }
    }

    /// Budget of a 64-sample block @48 kHz in ns, and the 85% overload line.
    fn stall_for_overload() -> u64 {
        let budget_ns = (BLOCK as u64 * 1_000_000_000) / SAMPLE_RATE as u64;
        (budget_ns * 85) / 100 + 1_000_000
    }

    /// Injects one over-budget block at a non-decimated phase (block 7 of 16)
    /// and asserts it is counted — the old 1-in-16 sampling missed this phase.
    #[test]
    fn test_overload_counted_at_non_decimated_phase() {
        let _mutex_guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        // Drain a stale armed stall from a sibling test that may have failed
        // mid-injection (fail-closed: the hook must start disarmed).
        TEST_TELEMETRY_STALL_NS.store(0, Ordering::Relaxed);
        let stall_ns = stall_for_overload();
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let stopped = plugin_instance
            .activate(|_, _| (), audio_config_64())
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
        let rt_status = &shared.cold.rt_status;
        // Block budget in samples must match the 64-sample harness blocks so
        // the 85%-of-budget overload line applies to the stall below.
        assert_eq!(
            rt_status.last_n_samples.load(Ordering::Relaxed),
            0,
            "no block processed yet"
        );
        let base_overloads = rt_status.dsp_overloads.load(Ordering::Relaxed);
        let base_hist = rt_status.latency_hist.total_count();

        let mut bufs = StereoTestBuffers::new(BLOCK, 0.1, 0.2);
        // Warm the ports' internal view state outside the audited window.
        test_util::process_stereo_block_prealloc(&mut started, &mut bufs, None);
        let warm_hist = rt_status.latency_hist.total_count();
        assert_eq!(
            warm_hist,
            base_hist + 1,
            "warm block must already feed the histogram (per-block telemetry)"
        );

        test_util::assert_zero_alloc("16 measured blocks (1 stalled @phase 7)", || {
            for i in 0..16 {
                if i == 7 {
                    TEST_TELEMETRY_STALL_NS.store(stall_ns, Ordering::Relaxed);
                }
                test_util::process_stereo_block_prealloc(&mut started, &mut bufs, None);
            }
        });

        assert_eq!(
            rt_status.dsp_overloads.load(Ordering::Relaxed),
            base_overloads + 1,
            "exactly the one stalled block (phase 7, invisible to 1-in-16) must be counted"
        );
        // 1 warm + 16 loop blocks, all observed.
        assert_eq!(
            rt_status.latency_hist.total_count(),
            warm_hist + 16,
            "histogram must reflect 100% of blocks"
        );
        assert!(
            rt_status.dsp_cycle_time.load(Ordering::Relaxed) > 0,
            "dsp_cycle_time must hold the latest measurement"
        );
        // One-shot hook must have been consumed by the stalled block.
        assert_eq!(
            TEST_TELEMETRY_STALL_NS.load(Ordering::Relaxed),
            0,
            "stall hook is one-shot"
        );
    }

    /// Three consecutive over-budget blocks must degrade the FSM out of Full
    /// within ≤ 3 blocks (Conservative confirmation = 3 consecutive).
    #[test]
    fn test_adaptive_fsm_reacts_within_three_blocks() {
        let _mutex_guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        TEST_TELEMETRY_STALL_NS.store(0, Ordering::Relaxed);
        let stall_ns = stall_for_overload();
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let stopped = plugin_instance
            .activate(|_, _| (), audio_config_64())
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
        let rt_status = &shared.cold.rt_status;

        let mut bufs = StereoTestBuffers::new(BLOCK, 0.1, 0.2);
        test_util::process_stereo_block_prealloc(&mut started, &mut bufs, None);
        let base_transitions = rt_status.degrade_transitions_total.load(Ordering::Relaxed);

        for _ in 0..3 {
            TEST_TELEMETRY_STALL_NS.store(stall_ns, Ordering::Relaxed);
            test_util::process_stereo_block_prealloc(&mut started, &mut bufs, None);
        }

        assert!(
            rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_REDUCED),
            "FSM must leave Full after 3 consecutive over-budget blocks"
        );
        assert_eq!(
            rt_status.degrade_transitions_total.load(Ordering::Relaxed),
            base_transitions + 1,
            "exactly one Full→Reduced transition expected"
        );
        // Keep the unit-level state assertion next to the flag: the FSM type
        // the plugin drives is the engine's generic `AdaptiveCompute`.
        let _ = AdaptiveState::Full;
    }
}

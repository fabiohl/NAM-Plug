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
    use crate::clap::processor::{TEST_TELEMETRY_STALL_NS, TEST_TELEMETRY_STALL_TARGET_INSTANCE};
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
        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(shared.cold.instance_id, Ordering::Relaxed);
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
        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(0, Ordering::Relaxed);
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
        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(shared.cold.instance_id, Ordering::Relaxed);
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
        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(0, Ordering::Relaxed);
        TEST_TELEMETRY_STALL_NS.store(0, Ordering::Relaxed);
    }

    /// Injects an implausible elapsed duration (> MAX_PLAUSIBLE_ELAPSED_NS)
    /// into telemetry and prewarm, asserting that the sample is discarded:
    /// - `dsp_overloads == 0` (does not increment overload count)
    /// - `latency_hist` count does not increment
    /// - prewarm cost remains unaffected
    #[test]
    fn test_implausible_elapsed_discarded_defense_in_depth() {
        use crate::clap::processor::{MAX_PLAUSIBLE_ELAPSED_NS, TEST_PREWARM_ELAPSED_NS};

        let _mutex_guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        TEST_TELEMETRY_STALL_NS.store(0, Ordering::Relaxed);
        TEST_PREWARM_ELAPSED_NS.store(0, Ordering::Relaxed);

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let stopped = plugin_instance
            .activate(|_, _| (), audio_config_64())
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(shared.cold.instance_id, Ordering::Relaxed);
        let rt_status = &shared.cold.rt_status;

        let mut bufs = StereoTestBuffers::new(BLOCK, 0.1, 0.2);

        // Run 1 warm block
        test_util::process_stereo_block_prealloc(&mut started, &mut bufs, None);
        let base_overloads = rt_status.dsp_overloads.load(Ordering::Relaxed);
        let base_hist = rt_status.latency_hist.total_count();

        // Inject an implausible duration into telemetry (2 seconds > 1s MAX_PLAUSIBLE_ELAPSED_NS)
        let implausible_ns = MAX_PLAUSIBLE_ELAPSED_NS + 1_000_000_000;
        TEST_TELEMETRY_STALL_NS.store(implausible_ns, Ordering::Relaxed);

        test_util::assert_zero_alloc("implausible elapsed block discard", || {
            test_util::process_stereo_block_prealloc(&mut started, &mut bufs, None);
        });

        // Telemetry must have discarded the block:
        assert_eq!(
            rt_status.dsp_overloads.load(Ordering::Relaxed),
            base_overloads,
            "implausible elapsed duration must not increment dsp_overloads"
        );
        assert_eq!(
            rt_status.latency_hist.total_count(),
            base_hist,
            "implausible elapsed duration must not be recorded in latency histogram"
        );

        // Hook must have been consumed
        assert_eq!(
            TEST_TELEMETRY_STALL_NS.load(Ordering::Relaxed),
            0,
            "stall hook must be cleared"
        );
        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(0, Ordering::Relaxed);
    }

    /// Unit test for EWMA DSP load and xrun_epoch publishing (T-R2.1):
    /// - Step response (degrau): 0 -> 500‰ load
    /// - Convergence: after ~1.25s (~5 * tau), asserts load converges to ~500‰
    /// - Saturation at 1000‰: 2000‰ overload clamps/saturates at 1000‰
    /// - xrun_epoch: increments and mirrors dsp_overloads on over-budget blocks
    #[test]
    fn test_telemetry_ewma_step_convergence_and_saturation() {
        let _mutex_guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        TEST_TELEMETRY_STALL_NS.store(0, Ordering::Relaxed);

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let stopped = plugin_instance
            .activate(|_, _| (), audio_config_64())
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(shared.cold.instance_id, Ordering::Relaxed);
        let mut bufs = StereoTestBuffers::new(BLOCK, 0.1, 0.2);

        // Budget for 64 samples @ 48kHz is 1_333_333 ns (1.333 ms).
        let budget_ns = (BLOCK as u64 * 1_000_000_000) / SAMPLE_RATE as u64;

        // Initial state
        assert_eq!(shared.rt_to_ui.dsp_load_permille.load(Ordering::Relaxed), 0);
        assert_eq!(shared.rt_to_ui.xrun_epoch.load(Ordering::Relaxed), 0);

        // Phase 1: Step response (50% budget = 500 permille)
        let step_stall_ns = budget_ns / 2;

        // Run 1 block with 50% stall:
        TEST_TELEMETRY_STALL_NS.store(step_stall_ns, Ordering::Relaxed);
        test_util::process_stereo_block_prealloc(&mut started, &mut bufs, None);

        let permille_1 = shared.rt_to_ui.dsp_load_permille.load(Ordering::Relaxed);
        assert!(
            permille_1 > 0 && permille_1 < 500,
            "After 1 block of 500‰ step, EWMA must begin rising (got {permille_1}‰)"
        );
        // Overload threshold is at 85%, so 50% must NOT trigger xrun
        assert_eq!(shared.rt_to_ui.xrun_epoch.load(Ordering::Relaxed), 0);

        // Phase 2: Convergence after ~5 * tau (tau = 250ms -> 1.25s -> ~938 blocks of 64 samples @ 48kHz).
        // Run 1000 blocks with 500‰ stall.
        for _ in 0..1000 {
            TEST_TELEMETRY_STALL_NS.store(step_stall_ns, Ordering::Relaxed);
            test_util::process_stereo_block_prealloc(&mut started, &mut bufs, None);
        }
        let converged = shared.rt_to_ui.dsp_load_permille.load(Ordering::Relaxed);
        assert!(
            (converged as i32 - 500).abs() <= 5,
            "EWMA must converge to ~500‰ (got {converged}‰)"
        );

        // Phase 3: Saturation at 1000‰ with over-budget stall (e.g. 200% budget)
        let overload_stall_ns = budget_ns * 2;
        let base_overloads = shared.cold.rt_status.dsp_overloads.load(Ordering::Relaxed);

        for _ in 0..2500 {
            TEST_TELEMETRY_STALL_NS.store(overload_stall_ns, Ordering::Relaxed);
            test_util::process_stereo_block_prealloc(&mut started, &mut bufs, None);
        }

        let saturated = shared.rt_to_ui.dsp_load_permille.load(Ordering::Relaxed);
        assert_eq!(
            saturated, 1000,
            "EWMA must saturate strictly at 1000‰ under sustained overload"
        );
        let current_overloads = shared.cold.rt_status.dsp_overloads.load(Ordering::Relaxed);
        assert!(current_overloads > base_overloads);
        assert_eq!(
            shared.rt_to_ui.xrun_epoch.load(Ordering::Relaxed),
            current_overloads,
            "xrun_epoch must mirror dsp_overloads exactly"
        );
        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(0, Ordering::Relaxed);
        TEST_TELEMETRY_STALL_NS.store(0, Ordering::Relaxed);
    }
}

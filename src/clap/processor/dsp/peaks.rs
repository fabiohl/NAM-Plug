// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::plugin::NamClapShared;
use std::sync::atomic::Ordering;

/// Stores audio peak and clipping telemetry into the shared RT-to-UI struct.
///
/// Uses `AtomicU32::fetch_max` with `f32::to_bits()` to atomically track the maximum
/// peak observed across audio blocks without TOCTOU races against the UI thread's
/// periodic `swap(0.0)`.
///
/// For non-negative IEEE 754 floats, the bit pattern maintains strict monotonic
/// ordering with unsigned integer comparison. We sanitize values (clamp negative,
/// -0.0, and NaN to +0.0) to ensure bit representations have sign bit 0.
#[inline(always)]
pub(super) fn store_peaks(shared: &NamClapShared, peak_l: f32, peak_r: f32) {
    let safe_peak_l = if peak_l <= 0.0 || peak_l.is_nan() {
        0.0
    } else {
        peak_l
    };
    shared
        .rt_to_ui
        .ui_peak_l
        .fetch_max(safe_peak_l.to_bits(), Ordering::Relaxed);

    let safe_peak_r = if peak_r <= 0.0 || peak_r.is_nan() {
        0.0
    } else {
        peak_r
    };
    shared
        .rt_to_ui
        .ui_peak_r
        .fetch_max(safe_peak_r.to_bits(), Ordering::Relaxed);

    if peak_l > 1.0 || peak_r > 1.0 {
        shared.rt_to_ui.ui_clipped.store(true, Ordering::Relaxed);
        shared
            .rt_to_ui
            .ui_clip_indicator
            .store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clap::plugin::shared::GuiSharedState;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    fn make_test_shared() -> NamClapShared {
        NamClapShared {
            gui: GuiSharedState::new_test(),
        }
    }

    #[test]
    fn test_store_peaks_monotonic_fetch_max() {
        let shared = make_test_shared();

        // Initial peaks are 0.0
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed)),
            0.0
        );
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed)),
            0.0
        );

        // First block: 0.25 L, 0.50 R
        store_peaks(&shared, 0.25, 0.50);
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed)),
            0.25
        );
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed)),
            0.50
        );

        // Second block: smaller L (0.10), larger R (0.75) -> L stays 0.25, R grows to 0.75
        store_peaks(&shared, 0.10, 0.75);
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed)),
            0.25
        );
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed)),
            0.75
        );

        // Third block: larger L (0.90), smaller R (0.20) -> L grows to 0.90, R stays 0.75
        store_peaks(&shared, 0.90, 0.20);
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed)),
            0.90
        );
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed)),
            0.75
        );

        // UI poll: swap(0.0) reads the max peak and atomically resets to 0.0
        let read_l = f32::from_bits(
            shared
                .rt_to_ui
                .ui_peak_l
                .swap(0.0f32.to_bits(), Ordering::Relaxed),
        );
        let read_r = f32::from_bits(
            shared
                .rt_to_ui
                .ui_peak_r
                .swap(0.0f32.to_bits(), Ordering::Relaxed),
        );
        assert_eq!(read_l, 0.90);
        assert_eq!(read_r, 0.75);

        // After reset, atomic holds 0.0
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed)),
            0.0
        );
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed)),
            0.0
        );

        // Subsequent audio block cleanly updates from 0.0
        store_peaks(&shared, 0.40, 0.30);
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed)),
            0.40
        );
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed)),
            0.30
        );
    }

    #[test]
    fn test_store_peaks_sanitizes_negative_and_nan() {
        let shared = make_test_shared();

        // -0.0 and negative numbers must not produce bit patterns with sign bit 1
        store_peaks(&shared, -0.0, -1.5);
        assert_eq!(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed), 0);
        assert_eq!(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed), 0);
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed)),
            0.0
        );

        // NaN and negative infinity must be clamped to 0.0
        store_peaks(&shared, f32::NAN, f32::NEG_INFINITY);
        assert_eq!(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed), 0);
        assert_eq!(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed), 0);

        // Store valid positive value
        store_peaks(&shared, 0.65, 0.45);
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed)),
            0.65
        );
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed)),
            0.45
        );

        // Subsequent NaN or negative inputs must not corrupt or overwrite higher valid peaks
        store_peaks(&shared, f32::NAN, -10.0);
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed)),
            0.65
        );
        assert_eq!(
            f32::from_bits(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed)),
            0.45
        );
    }

    #[test]
    fn test_store_peaks_clipping_indicators() {
        let shared = make_test_shared();

        store_peaks(&shared, 0.99, 1.0);
        assert!(!shared.rt_to_ui.ui_clipped.load(Ordering::Relaxed));
        assert!(!shared.rt_to_ui.ui_clip_indicator.load(Ordering::Relaxed));

        // Signal exceeding 1.0 triggers clipping flags
        store_peaks(&shared, 1.02, 0.5);
        assert!(shared.rt_to_ui.ui_clipped.load(Ordering::Relaxed));
        assert!(shared.rt_to_ui.ui_clip_indicator.load(Ordering::Relaxed));

        // UI acknowledges and resets sticky clipped flag
        shared.rt_to_ui.ui_clipped.store(false, Ordering::Relaxed);
        assert!(!shared.rt_to_ui.ui_clipped.load(Ordering::Relaxed));
    }

    #[test]
    fn test_peaks_concurrency_ui_rt_no_systematic_loss() {
        let shared = Arc::new(make_test_shared());
        let running = Arc::new(AtomicBool::new(true));

        const NUM_BLOCKS: usize = 20_000;
        const SPIKE_INTERVAL: usize = 50;
        const SPIKE_VALUE: f32 = 0.95;
        const BASE_VALUE: f32 = 0.10;

        let shared_rt = Arc::clone(&shared);
        let running_rt = Arc::clone(&running);

        let rt_thread = thread::spawn(move || {
            for i in 0..NUM_BLOCKS {
                let peak = if i % SPIKE_INTERVAL == 0 {
                    SPIKE_VALUE
                } else {
                    BASE_VALUE
                };
                store_peaks(&shared_rt, peak, peak);
                if i % 10 == 0 {
                    std::hint::spin_loop();
                }
            }
            running_rt.store(false, Ordering::Release);
        });

        let shared_ui = Arc::clone(&shared);
        let running_ui = Arc::clone(&running);

        let ui_thread = thread::spawn(move || {
            let mut spikes_observed = 0usize;
            let mut max_observed = 0.0f32;

            while running_ui.load(Ordering::Acquire) {
                let val_l = f32::from_bits(
                    shared_ui
                        .rt_to_ui
                        .ui_peak_l
                        .swap(0.0f32.to_bits(), Ordering::Relaxed),
                );
                let val_r = f32::from_bits(
                    shared_ui
                        .rt_to_ui
                        .ui_peak_r
                        .swap(0.0f32.to_bits(), Ordering::Relaxed),
                );

                assert!(!val_l.is_nan(), "Observed NaN peak in L channel");
                assert!(!val_r.is_nan(), "Observed NaN peak in R channel");
                assert!(
                    val_l >= 0.0,
                    "Observed negative peak in L channel: {}",
                    val_l
                );
                assert!(
                    val_r >= 0.0,
                    "Observed negative peak in R channel: {}",
                    val_r
                );

                if val_l >= SPIKE_VALUE {
                    spikes_observed += 1;
                }
                max_observed = max_observed.max(val_l).max(val_r);
                std::hint::spin_loop();
            }

            // Drain any remainder after RT finishes
            let final_l = f32::from_bits(
                shared_ui
                    .rt_to_ui
                    .ui_peak_l
                    .swap(0.0f32.to_bits(), Ordering::Relaxed),
            );
            let final_r = f32::from_bits(
                shared_ui
                    .rt_to_ui
                    .ui_peak_r
                    .swap(0.0f32.to_bits(), Ordering::Relaxed),
            );
            if final_l >= SPIKE_VALUE {
                spikes_observed += 1;
            }
            max_observed = max_observed.max(final_l).max(final_r);

            (spikes_observed, max_observed)
        });

        rt_thread.join().expect("RT thread panic");
        let (spikes_observed, max_observed) = ui_thread.join().expect("UI thread panic");

        // The maximum peak seen across all UI ticks must reach exactly SPIKE_VALUE
        assert_eq!(
            max_observed, SPIKE_VALUE,
            "Max peak observed must equal the highest emitted spike value"
        );
        // Spikes must have been reliably captured across the concurrent execution
        assert!(
            spikes_observed > 0,
            "Concurrent UI thread must observe spikes without systematic loss (got {})",
            spikes_observed
        );
    }
}

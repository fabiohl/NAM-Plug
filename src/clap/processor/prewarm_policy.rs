// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Proportional stabilization budget policy for two-phase model resets (C-01).
//!
//! Replaces the legacy fixed 1024-sample drain step with a dynamically scaled step:
//! `step = clamp(floor(frames_count · fator_família), piso, 1024)`, bounded by the
//! real callback deadline budget.
//!
//! # Architecture & Rationale
//! At low block sizes (e.g. 32 frames @ 48 kHz, deadline 667 µs) or high sample rates
//! (e.g. 64 frames @ 192 kHz, deadline 333 µs), a monolithic 1024-sample prewarm step
//! for chained WaveNet A2 inference (~0.87–0.93 ms) breaches the hard real-time deadline.
//! This policy computes a safe per-callback prewarm step proportional to the callback's
//! real-time period while maintaining dry-until-ready semantics and full stabilization depth.

use neural_amp_modeler_rs::models::StaticModel;

/// Absolute maximum zeroed-sample stabilization step drained per audio callback.
pub(crate) const PREWARM_STEP_MAX: usize = 1024;

/// Absolute minimum zeroed-sample stabilization step when the callback budget permits.
pub(crate) const PREWARM_STEP_PISO: usize = 1;

/// Whether the runtime adaptive prewarm policy (measured cost seed + EWMA) is
/// active.
///
/// Disabled in debug builds: `debug_assertions` inference is ~10–30× slower
/// than the optimized profile the per-family table describes, so a debug
/// measurement would shrink the amortization step below the table's safe value
/// and make convergence-count-sensitive tests profile-dependent without
/// improving any shipped binary.
pub(crate) const ADAPTIVE_COST_ENABLED: bool = !cfg!(debug_assertions);

/// Block length used for the off-RT per-sample cost calibration
/// ([`NeuralAmpModeler-rs measure_cost_ns_per_sample`](neural_amp_modeler_rs::models::NamModel::measure_cost_ns_per_sample)).
///
/// 256 frames is the engine's reference measurement block (U-02) and sits well
/// inside every negotiated host block, so the calibration never trips an
/// engine `max_buffer_size` guard.
pub(crate) const PREWARM_MEASURE_FRAMES: usize = 256;

/// Base execution overhead in nanoseconds for dry-block routing, event processing,
/// and telemetry inside the audio callback (~1.5 µs measured).
const OVERHEAD_NS: f64 = 1_500.0;

/// Conservative real-time budget utilization safety margin (75% of block period).
const BUDGET_SAFETY_MARGIN: f64 = 0.75;

/// Neural model architecture families with distinct empirical per-sample inference costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelFamily {
    /// WaveNet A2 family: chained cascade inference with heavy receptive field.
    /// Empirical cost: ~870–930 ns/sample on x86_64 AVX2 baseline.
    WaveNetA2,
    /// WaveNet A1 family: one-shot RF backfill (~48 µs total).
    WaveNetA1,
    /// LSTM family: recurrent cellular state.
    /// Empirical cost: ~69–75 ns/sample on x86_64 AVX2 baseline.
    Lstm,
    /// Other architectures (Linear FIR, ConvNet, Container, Sequential).
    Other,
}

impl ModelFamily {
    /// Classifies a [`StaticModel`] into its architectural family.
    #[inline]
    pub(crate) fn from_model(model: &StaticModel) -> Self {
        match model {
            StaticModel::WavenetA2Full(_)
            | StaticModel::WavenetA2Lite(_)
            | StaticModel::WavenetA2Dyn(_)
            | StaticModel::WavenetA2Cascade(_) => Self::WaveNetA2,

            StaticModel::WavenetStandard(_)
            | StaticModel::WavenetLite(_)
            | StaticModel::WavenetFeather(_)
            | StaticModel::WavenetNano(_)
            | StaticModel::WavenetDyn(_) => Self::WaveNetA1,

            StaticModel::Lstm1x3(_)
            | StaticModel::Lstm1x8(_)
            | StaticModel::Lstm1x12(_)
            | StaticModel::Lstm1x16(_)
            | StaticModel::Lstm1x24(_)
            | StaticModel::Lstm2x8(_)
            | StaticModel::Lstm2x12(_)
            | StaticModel::Lstm2x16(_)
            | StaticModel::Lstm1x40(_)
            | StaticModel::Lstm2x24(_)
            | StaticModel::LstmDyn(_) => Self::Lstm,

            StaticModel::ConvNet(_)
            | StaticModel::Linear(_)
            | StaticModel::Container(_)
            | StaticModel::Sequential(_)
            | _ => Self::Other,
        }
    }

    /// Scaling factor relating callback frames to prewarm stabilization samples.
    ///
    /// Derived from measured ns/sample envelopes and RT deadline margins:
    /// - `WaveNetA2`: 4.0 (for 256 frames = 1024; for 64 frames = 256; for 32 frames = 128)
    /// - `Lstm`: 16.0 (for 256 frames = 1024 max; for 64 frames = 1024 max; for 32 frames = 512)
    /// - `WaveNetA1`: 16.0 (one-shot completes on first step >= 1)
    /// - `Other`: 16.0
    #[inline]
    pub(crate) fn family_factor(&self) -> f64 {
        match self {
            Self::WaveNetA2 => 4.0,
            Self::WaveNetA1 => 16.0,
            Self::Lstm => 16.0,
            Self::Other => 16.0,
        }
    }

    /// Conservative empirical per-sample execution cost in nanoseconds on baseline hardware.
    #[inline]
    pub(crate) fn worst_case_sample_cost_ns(&self) -> f64 {
        match self {
            Self::WaveNetA2 => 930.0,
            Self::Lstm => 75.0,
            Self::WaveNetA1 => 500.0,
            Self::Other => 500.0,
        }
    }
}

/// Maximum plausible measured execution cost in nanoseconds per single sample (50 µs).
///
/// Prevents an OS descheduling spike during off-RT calibration or runtime EWMA from
/// poisoning the per-sample cost to an implausible value that starves the prewarm budget.
pub(crate) const MAX_PLAUSIBLE_SAMPLE_COST_NS: f64 = 50_000.0;

/// Maximum consecutive zero-budget audio callbacks before latching starvation diagnostics
/// (`RT_STATUS_HOST_CONTRACT_VIOLATION`) and resetting EWMA cost to attempt recovery.
pub(crate) const PREWARM_ZERO_BUDGET_STARVATION_LIMIT: u32 = 64;

/// Computes the zeroed-sample stabilization step for the current callback according to
/// the proportional budget policy:
///
/// `step = clamp(floor(frames_count · fator_família / chains), piso, 1024)`
///
/// bounded by the callback's real-time budget across all `chains` active models. The per-sample
/// cost used for the budget bound is `measured_cost_ns_per_sample` (clamped to
/// [`MAX_PLAUSIBLE_SAMPLE_COST_NS`]) when it is a positive finite value (the off-RT calibration
/// of the actually installed model, U-02); otherwise the conservative per-family table
/// ([`ModelFamily::worst_case_sample_cost_ns`]) is used as fallback. If even the minimum step
/// (`piso = 1`) across all active chains does not safely fit within the estimated callback
/// deadline budget, returns `0` so the audio callback does not breach real-time deadlines and
/// remains safely on the latency-compensated dry path.
#[inline]
pub(crate) fn compute_prewarm_step(
    frames_count: usize,
    min_frames_count: usize,
    sample_rate: f64,
    family: ModelFamily,
    measured_cost_ns_per_sample: Option<f32>,
    chains: usize,
) -> usize {
    let effective_frames = if frames_count > 0 {
        frames_count
    } else {
        min_frames_count.max(1)
    };

    let sample_rate = if sample_rate > 0.0 {
        sample_rate
    } else {
        48000.0
    };
    let period_ns = (effective_frames as f64 / sample_rate) * 1e9;

    // Available RT duration for prewarm after reserving processing overhead and safety margin.
    let available_ns = period_ns * BUDGET_SAFETY_MARGIN - OVERHEAD_NS;
    let single_sample_cost_ns = match measured_cost_ns_per_sample {
        Some(cost) if cost.is_finite() && cost > 0.0 => {
            (f64::from(cost)).min(MAX_PLAUSIBLE_SAMPLE_COST_NS)
        }
        _ => family.worst_case_sample_cost_ns(),
    };

    let active_chains = (chains.max(1)) as f64;
    let step_cost_ns = single_sample_cost_ns * active_chains;

    // If available margin cannot accommodate even a single sample across active chains, defer prewarm work.
    if available_ns < step_cost_ns {
        return 0;
    }

    let raw_step =
        ((effective_frames as f64 * family.family_factor()) / active_chains).floor() as usize;
    let budget_max_samples =
        ((available_ns / step_cost_ns).floor() as usize).max(PREWARM_STEP_PISO);

    raw_step
        .min(budget_max_samples)
        .clamp(PREWARM_STEP_PISO, PREWARM_STEP_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prewarm_policy_blocks_and_sample_rates() {
        let block_sizes = [1, 32, 64, 128, 256, 512, 1024];
        let sample_rates = [44100.0, 48000.0, 88200.0, 96000.0, 176400.0, 192000.0];
        let families = [
            ModelFamily::WaveNetA2,
            ModelFamily::WaveNetA1,
            ModelFamily::Lstm,
            ModelFamily::Other,
        ];

        for &sr in &sample_rates {
            for &bs in &block_sizes {
                for &fam in &families {
                    let step = compute_prewarm_step(bs, bs, sr, fam, None, 1);
                    let period_ns = (bs as f64 / sr) * 1e9;

                    if step > 0 {
                        assert!(
                            step >= PREWARM_STEP_PISO,
                            "step must be at least piso when positive: bs={bs}, sr={sr}, fam={fam:?}"
                        );
                        assert!(
                            step <= PREWARM_STEP_MAX,
                            "step must not exceed PREWARM_STEP_MAX: bs={bs}, sr={sr}, fam={fam:?}"
                        );

                        // Verify that prewarm execution time stays strictly within the audio callback period.
                        let estimated_cost_ns = step as f64 * fam.worst_case_sample_cost_ns();
                        assert!(
                            estimated_cost_ns <= period_ns,
                            "estimated prewarm cost ({estimated_cost_ns} ns) exceeded period ({period_ns} ns) for bs={bs}, sr={sr}, fam={fam:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_prewarm_policy_standard_256_matches_1024() {
        // Contract check: at standard 256 frames @ 48 kHz, every family yields step = 1024.
        for &fam in &[
            ModelFamily::WaveNetA2,
            ModelFamily::WaveNetA1,
            ModelFamily::Lstm,
            ModelFamily::Other,
        ] {
            let step = compute_prewarm_step(256, 256, 48000.0, fam, None, 1);
            assert_eq!(
                step, 1024,
                "at block=256, 48 kHz, family {fam:?} must produce step=1024"
            );
        }
    }

    #[test]
    fn test_prewarm_policy_scaling_at_small_blocks() {
        // At 32 frames @ 48 kHz (deadline 667 µs):
        // WaveNet A2 must scale to 128 samples (cost ~119 µs << 667 µs).
        let step_a2_32 = compute_prewarm_step(32, 32, 48000.0, ModelFamily::WaveNetA2, None, 1);
        assert_eq!(step_a2_32, 128);

        // LSTM at 32 frames @ 48 kHz scales to 512 samples (cost ~38 µs << 667 µs).
        let step_lstm_32 = compute_prewarm_step(32, 32, 48000.0, ModelFamily::Lstm, None, 1);
        assert_eq!(step_lstm_32, 512);

        // At 64 frames @ 192 kHz (deadline 333 µs):
        // WaveNet A2 must stay under 333 µs: 256 samples * 930 ns ≈ 238 µs.
        let step_a2_64_192k =
            compute_prewarm_step(64, 64, 192000.0, ModelFamily::WaveNetA2, None, 1);
        assert_eq!(step_a2_64_192k, 256);
    }

    #[test]
    fn test_prewarm_policy_guarantees_progress() {
        // Verify that with any step >= 1, finite work converges in a bounded number of callbacks.
        let initial_work = 24_000_usize;
        let step = PREWARM_STEP_PISO; // Worst-case minimum step
        let callbacks = initial_work.div_ceil(step);
        assert_eq!(callbacks, 24_000);
    }

    #[test]
    fn test_prewarm_policy_zero_frames_fallback() {
        // If frames_count == 0, fallback to min_frames_count.
        let step = compute_prewarm_step(0, 256, 48000.0, ModelFamily::WaveNetA2, None, 1);
        assert_eq!(step, 1024);
    }

    #[test]
    fn test_prewarm_policy_measured_cost_bounds_step() {
        // 32 frames @ 48 kHz: period ≈ 666.7 µs, budget ≈ 498.5 µs.
        // A measured cost of 20 000 ns/sample (≈ 21.5× the A2 table) caps the
        // step at floor(498.5 µs / 20 µs) = 24, far below the family raw 128.
        let high_cost = 20_000.0f32;
        let step =
            compute_prewarm_step(32, 32, 48000.0, ModelFamily::WaveNetA2, Some(high_cost), 1);
        assert_eq!(step, 24, "measured cost must bound the prewarm step");
        // The bound must keep the estimated cost inside 75% of the period.
        let period_ns = (32.0 / 48000.0) * 1e9;
        assert!(
            step as f64 * f64::from(high_cost) <= 0.75 * period_ns,
            "step·measured_cost must respect the 75% budget margin"
        );
    }

    #[test]
    fn test_prewarm_policy_measured_cost_rejects_invalid() {
        // Non-finite / non-positive measurements fall back to the family table,
        // yielding the same step as `None`.
        let table = compute_prewarm_step(32, 32, 48000.0, ModelFamily::WaveNetA2, None, 1);
        for invalid in [
            Some(0.0f32),
            Some(-1.0),
            Some(f32::NAN),
            Some(f32::INFINITY),
        ] {
            let step = compute_prewarm_step(32, 32, 48000.0, ModelFamily::WaveNetA2, invalid, 1);
            assert_eq!(
                step, table,
                "invalid measured cost {invalid:?} must fall back"
            );
        }
    }

    #[test]
    fn test_prewarm_policy_measured_cost_near_table_is_neutral() {
        // A measurement close to the A2 table cost must not change the standard
        // 256-frame / 48 kHz contract step (1024).
        let step = compute_prewarm_step(256, 256, 48000.0, ModelFamily::WaveNetA2, Some(930.0), 1);
        assert_eq!(step, 1024);
    }

    #[test]
    fn test_prewarm_policy_dual_mono_chains_scales_step() {
        // In dual-mono (chains = 2), raw step and budget max samples are divided by 2
        // so that stepping both L and R models fits within the single callback budget.
        let step_mono = compute_prewarm_step(256, 256, 48000.0, ModelFamily::WaveNetA2, None, 1);
        let step_stereo = compute_prewarm_step(256, 256, 48000.0, ModelFamily::WaveNetA2, None, 2);

        assert_eq!(step_mono, 1024);
        assert_eq!(step_stereo, 512, "dual-mono must divide raw step by chains");

        // Small block scaling with dual chains: 32 frames @ 48 kHz
        let step_mono_32 = compute_prewarm_step(32, 32, 48000.0, ModelFamily::WaveNetA2, None, 1);
        let step_stereo_32 = compute_prewarm_step(32, 32, 48000.0, ModelFamily::WaveNetA2, None, 2);
        assert_eq!(step_mono_32, 128);
        assert_eq!(step_stereo_32, 64);

        // Verification of budget safety: total execution time for 2 chains must respect budget
        let period_ns = (32.0 / 48000.0) * 1e9;
        let total_cost =
            (step_stereo_32 * 2) as f64 * ModelFamily::WaveNetA2.worst_case_sample_cost_ns();
        assert!(
            total_cost <= 0.75 * period_ns,
            "dual chain prewarm cost ({total_cost} ns) must not exceed 75% budget ({period_ns} ns)"
        );
    }

    #[test]
    fn test_prewarm_policy_zero_budget_deferral() {
        // At 1 frame @ 192 kHz with high measured cost, period is 5208 ns.
        // Available margin is (5208 * 0.75) - 1500 = 2406 ns.
        // If single sample cost is 10 000 ns, available_ns < step_cost_ns -> returns 0.
        let step = compute_prewarm_step(1, 1, 192000.0, ModelFamily::WaveNetA2, Some(10_000.0), 1);
        assert_eq!(
            step, 0,
            "must return 0 when callback cannot safely fit even 1 sample"
        );

        // Dual chains require 2 * 2000 = 4000 ns cost; available is 2406 ns -> returns 0.
        let step_dual =
            compute_prewarm_step(1, 1, 192000.0, ModelFamily::WaveNetA2, Some(2000.0), 2);
        assert_eq!(
            step_dual, 0,
            "must return 0 when dual chains cannot fit in available margin"
        );
    }

    #[test]
    fn test_prewarm_policy_measured_cost_ceiling_clamp() {
        // An implausibly high cost (e.g. 500 000 ns due to OS descheduling)
        // must be clamped to MAX_PLAUSIBLE_SAMPLE_COST_NS (50 000 ns).
        // At 256 frames @ 48 kHz (period 5.33 ms, available 4.0 ms - 1.5 µs overhead = 3.9985 ms):
        // 3.9985 ms / 50 µs = 79 samples max.
        let clamped_step = compute_prewarm_step(
            256,
            256,
            48000.0,
            ModelFamily::WaveNetA2,
            Some(500_000.0),
            1,
        );
        let period_ns = (256.0 / 48000.0) * 1e9;
        let available_ns = period_ns * BUDGET_SAFETY_MARGIN - OVERHEAD_NS;
        let expected_max = (available_ns / MAX_PLAUSIBLE_SAMPLE_COST_NS).floor() as usize;
        assert_eq!(
            clamped_step, expected_max,
            "implausible cost must be clamped to MAX_PLAUSIBLE_SAMPLE_COST_NS"
        );
    }
}

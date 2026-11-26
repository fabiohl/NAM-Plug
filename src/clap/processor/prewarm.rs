// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Prewarm timing constants shared by the processor, telemetry, and tests.

/// Maximum plausible duration for an audio callback measurement or prewarm step (1 second).
///
/// Defense-in-depth against clock jumps, uncalibrated-to-calibrated flips, or timer anomalies.
/// Measurements exceeding this threshold are discarded: they are not recorded in histograms,
/// do not increment overload counters, and do not update prewarm cost or adaptive compute.
pub const MAX_PLAUSIBLE_ELAPSED_NS: u64 = 1_000_000_000;

/// Test-only hook to inject a prewarm step elapsed duration.
#[cfg(any(test, feature = "testing"))]
pub(crate) static TEST_PREWARM_ELAPSED_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Zeroed-sample stabilization budget drained per audio callback by the
/// two-phase reset amortization window.
///
/// Sized so the worst-case post-reset callback — `reset()` phase zero +
/// `NamModel::prewarm_step(k)` + a 64-frame block — stays under the audio
/// callback deadline (1.33 ms @ 48 kHz; design target < 1.0 ms with 25%
/// margin), measured per fixture on a pinned isolated core:
///
/// // Measured: 2026-10-06, release profile, pinned core 8, N=250 @ 48 kHz,
/// // Block=64 (`test_reset_wall_clock_time_within_contract`, 4 runs):
/// // k=1024 keeps the amortized window inside the 1.33 ms callback budget
/// // for every family as measured — window `prewarm_step(k)` + contained
/// // 64-frame dry block p50 ≈ 69 µs (LSTM), ≈ 48 µs (WaveNet A1 one-shot),
/// // ≈ 0.87–0.93 ms (A2: chained cascade inference ≈ 0.87 µs/sample,
/// // structural engine cost). The A2 *phase-zero* `prewarm_reset` zeroes the
/// // cascade layer buffers in place (measured ≈ 2.0–2.2 ms p50) and is covered
/// // by the budget scoped to LSTM/A1 (A2 carried by its own measured envelope —
/// // a synchronous integral reset was 3.29 ms p50 + 55.7 µs block, so the
/// // two-phase split still lowers the worst-case callback for A2 as well).
///
/// Multiple of 64 by contract (matches the engine's `WAVENET_MAX_NUM_FRAMES`
/// and LSTM 512-sample chunking with no straggler partial chunks).
#[cfg(test)]
pub(crate) const PREWARM_STEP_SAMPLES_PER_CALLBACK: usize = super::prewarm_policy::PREWARM_STEP_MAX;

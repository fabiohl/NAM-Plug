// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Pre-allocated circular dry delay line for bypass time-alignment.
//!
//! The wet DSP chain applies a fixed algorithmic latency
//! (`cached_effective_latency` = streaming resampler + oversampling + cab-sim
//! partition, in host-rate samples). The dry (bypass/crossfade) path must
//! match this latency: otherwise the bypass crossfade blends two
//! signals representing *different* temporal instants of the input, producing
//! comb filtering and transient cancellation during the ramp, and the
//! fully-bypassed state returns to zero physical latency while the plugin keeps
//! announcing the wet latency — a PDC inconsistency.
//!
//! [`DryDelayLine`] is a thin stereo wrapper over two engine
//! [`DelayLine<f32>`] rings (one per channel) that delays the dry signal by
//! exactly the applied wet latency, reusing the engine's RT-safe ring
//! implementation instead of duplicating ring storage and pointer arithmetic
//! in the plugin. Dry and wet then represent the same input instant at every
//! output sample, and the fully-bypassed path keeps the latency the plugin
//! declares to the host (the host's PDC compensates it).
//!
//! # RT-safety
//!
//! The rings are allocated once at [`DryDelayLine::new`] with capacity sized
//! for the maximum possible latency; the hot path
//! ([`DryDelayLine::process_block`]) performs zero allocations.
//! [`DryDelayLine::set_delay`] and [`DryDelayLine::reset`] are also
//! allocation-free and are called from the audio thread only inside cold
//! resource-swap handlers.

use neural_amp_modeler_rs::dsp::utils::DelayLine;

/// Maximum additional dry-delay headroom beyond the host block size.
///
/// The dry delay must cover the worst-case latency the DSP chain can apply —
/// streaming resampler (host↔model) group delay, oversampling half-band delay,
/// and cab-sim partition. The cab-sim partition equals the host buffer size,
/// so the ring capacity is `max_frames_count + DRY_DELAY_MAX_EXTRA`.
///
/// The resampler term is bounded by the engine's supported rate range
/// (4 kHz ..= 384 kHz): the linear-phase worst case is
/// `TAPS_PER_PHASE/2 × (1 + host_rate/nam_rate)` ≈ 32 × 97 ≈ 3104 host-rate
/// samples (host 384 kHz / model 4 kHz); production minimum-phase banks and
/// realistic NAM rates (44.1/48 kHz models) are far below. `+32` covers the
/// oversampling X4 half-band delay (2 × HB_DELAY = 24) with margin.
pub(crate) const DRY_DELAY_MAX_EXTRA: usize = 3200;

/// Stereo dry delay line: one bounded engine ring per channel.
///
/// The two [`DelayLine<f32>`] instances are independent — no dry signal ever
/// leaks between L and R — and share the same capacity/delay contract.
pub(crate) struct DryDelayLine {
    line_l: DelayLine<f32>,
    line_r: DelayLine<f32>,
}

impl DryDelayLine {
    /// Allocates the two per-channel engine rings (off-RT only — the single
    /// plugin allocation site is conventionally `activate()`).
    ///
    /// The engine ring holds `capacity + 1` aligned slots, so the effective
    /// maximum latency is exactly `capacity`; a `delay` above that is clamped.
    #[cold]
    pub(crate) fn new(capacity: usize, delay: usize) -> Self {
        Self {
            line_l: DelayLine::with_capacity(capacity, delay),
            line_r: DelayLine::with_capacity(capacity, delay),
        }
    }

    /// Sets the applied delay on both channels, clamped to `capacity`.
    ///
    /// RT-safe (no allocation). The ring history is retained, so changing the
    /// delay mid-stream does not create a discontinuity in the buffered signal
    /// — only in its read alignment, which must follow the wet latency exactly
    /// (called from the cold handlers that swap latency-affecting resources).
    ///
    /// Capacity is sized at `activate()` to cover the maximum possible latency,
    /// so a legitimate delay never exceeds the ring; the clamp is a fail-safe
    /// for pathological inputs.
    #[inline(always)]
    pub(crate) fn set_delay(&mut self, delay: usize) {
        self.line_l.set_delay(delay);
        self.line_r.set_delay(delay);
    }

    /// Resets both rings to their zeroed initial state. RT-safe.
    #[inline(always)]
    pub(crate) fn reset(&mut self) {
        self.line_l.reset();
        self.line_r.reset();
    }

    /// Pushes `n` host input samples into each ring and writes the
    /// `delay`-delayed dry signal into `out_l`/`out_r`.
    ///
    /// Invariant: `out[i] == in[i - delay]` for `i >= delay` (with `in[j] == 0`
    /// for `j < 0` during the initial zero-priming), so the delayed dry is
    /// time-aligned with the wet chain output, which is itself delayed by the
    /// same `delay` (stream + oversample + cab-sim).
    ///
    /// RT-safe: zero allocations.
    #[inline(always)]
    pub(crate) fn process_block(
        &mut self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
        n: usize,
    ) {
        let n = n
            .min(in_l.len())
            .min(in_r.len())
            .min(out_l.len())
            .min(out_r.len());
        for i in 0..n {
            self.line_l.push(in_l[i]);
            self.line_r.push(in_r[i]);
            out_l[i] = self.line_l.pop();
            out_r[i] = self.line_r.pop();
        }
    }
}

#[cfg(test)]
mod dry_delay_test {
    //! Wrapper-contract tests only. The raw ring mechanics (dynamic retarget,
    //! zero-priming, clamping, reset, zero-allocation) are unit-tested by the
    //! engine's `DelayLine`, and the end-to-end dry/wet alignment by the
    //! integration tests in `src/clap/processor_dry_delay_test.rs`; these tests
    //! cover what is unique here: slice bridging, stereo independence and
    //! per-channel delegation.
    use super::*;

    /// Builds a fresh `DryDelayLine` with `capacity` samples per channel.
    fn make_line(capacity: usize, delay: usize) -> DryDelayLine {
        DryDelayLine::new(capacity, delay)
    }

    /// Runs `input` through `line` in fixed-size blocks, returning the
    /// concatenated delayed L output (R is asserted equal for equal inputs).
    fn run_on(line: &mut DryDelayLine, input: &[f32], block: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; input.len()];
        let mut out_r = vec![0.0f32; input.len()];
        let mut offset = 0;
        for chunk in input.chunks(block) {
            let n = chunk.len();
            line.process_block(
                chunk,
                chunk,
                &mut out[offset..offset + n],
                &mut out_r[offset..offset + n],
                n,
            );
            offset += n;
        }
        assert_eq!(out, out_r, "L/R must be identical for identical inputs");
        out
    }

    /// Runs `input` through a fresh `DryDelayLine` in fixed-size blocks.
    fn run_blocks(capacity: usize, delay: usize, input: &[f32], block: usize) -> Vec<f32> {
        run_on(&mut make_line(capacity, delay), input, block)
    }

    #[test]
    fn test_block_chunked_exact_shift_survives_wraps_and_passthrough() {
        // `process_block` bridges host slices to the engine rings: the exact
        // `in[i - delay]` shift must survive arbitrary block boundaries and
        // ring wraps, and `delay == 0` must be an exact passthrough.
        let n = 2048;
        let input: Vec<f32> = (0..n).map(|i| (i % 997) as f32 * 0.001).collect();
        for &(cap, delay, block) in &[
            (128usize, 0usize, 16usize),
            (160, 128, 7),
            (69, 37, 23),
            (1, 1, 1),
        ] {
            let out = run_blocks(cap, delay, &input, block);
            assert_eq!(out.len(), n);
            for i in 0..n {
                let expected = if i >= delay { input[i - delay] } else { 0.0 };
                assert_eq!(
                    out[i], expected,
                    "cap={cap} delay={delay} block={block} mismatch at {i}"
                );
            }
        }

        // `n` above the shortest slice is clamped instead of panicking.
        let mut line = make_line(16, 4);
        let mut out_l = [0.0f32; 8];
        let mut out_r = [0.0f32; 8];
        line.process_block(&[1.0; 32], &[1.0; 32], &mut out_l, &mut out_r, 1024);
    }

    #[test]
    fn test_stereo_channels_remain_independent() {
        let delay = 16usize;
        let mut line = make_line(32, delay);
        let in_l = vec![0.1f32; 64];
        let in_r = vec![0.9f32; 64];
        let mut out_l = vec![0.0f32; 64];
        let mut out_r = vec![0.0f32; 64];
        line.process_block(&in_l, &in_r, &mut out_l, &mut out_r, 64);
        for i in 0..64 {
            if i < delay {
                assert_eq!(out_l[i], 0.0, "L zero-priming at {i}");
                assert_eq!(out_r[i], 0.0, "R zero-priming at {i}");
            } else {
                assert!(
                    (out_l[i] - 0.1).abs() < 1e-7,
                    "L channel must not leak into R at {i}"
                );
                assert!((out_r[i] - 0.9).abs() < 1e-7, "R must stay on R at {i}");
            }
        }
    }

    #[test]
    fn test_set_delay_and_reset_delegate_to_both_channels() {
        let capacity = 64usize;
        let mut line = make_line(capacity, 8);

        // Fill with distinct L/R history, then reset: both channels re-prime.
        let in_l = vec![0.1f32; capacity + 8];
        let in_r = vec![0.9f32; capacity + 8];
        let mut out_l = vec![0.0f32; capacity + 8];
        let mut out_r = vec![0.0f32; capacity + 8];
        line.process_block(&in_l, &in_r, &mut out_l, &mut out_r, capacity + 8);
        assert!((out_l[8] - 0.1).abs() < 1e-7, "L aligned before reset");
        assert!((out_r[8] - 0.9).abs() < 1e-7, "R aligned before reset");

        line.reset();
        line.process_block(&in_l, &in_r, &mut out_l, &mut out_r, capacity + 8);
        for i in 0..8 {
            assert_eq!(out_l[i], 0.0, "reset must clear L pre-history at {i}");
            assert_eq!(out_r[i], 0.0, "reset must clear R pre-history at {i}");
        }
        assert!((out_l[8] - 0.1).abs() < 1e-7, "post-reset L intact");
        assert!((out_r[8] - 0.9).abs() < 1e-7, "post-reset R intact");

        // An oversized retarget clamps to `capacity` on both channels: an
        // impulse must reappear at exactly `capacity`.
        line.set_delay(usize::MAX);
        line.reset();
        let mut impulse = vec![0.0f32; capacity + 16];
        impulse[0] = 1.0;
        let out = run_on(&mut line, &impulse, 13);
        assert_eq!(
            out.iter().position(|&s| s.abs() >= 0.5),
            Some(capacity),
            "oversized delay must clamp to capacity and still align exactly"
        );
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Pre-allocated circular dry delay line for bypass time-alignment (T4.1 /
//! F-DSP-008).
//!
//! The wet DSP chain applies a fixed algorithmic latency
//! (`cached_effective_latency` = streaming resampler + oversampling + cab-sim
//! partition, in host-rate samples). Before T4.1 the dry (bypass/crossfade)
//! path bypassed this latency entirely: the bypass crossfade blended two
//! signals representing *different* temporal instants of the input, producing
//! comb filtering and transient cancellation during the ramp, and the
//! fully-bypassed state returned to zero physical latency while the plugin kept
//! announcing the wet latency — a PDC inconsistency.
//!
//! [`DryDelayLine`] is a bounded ring buffer that delays the dry signal by
//! exactly the applied wet latency. Dry and wet then represent the same input
//! instant at every output sample, and the fully-bypassed path keeps the
//! latency the plugin declares to the host (the host's PDC compensates it).
//!
//! # RT-safety
//!
//! The ring is allocated once at `activate()` with capacity sized for the
//! maximum possible latency; the hot path ([`DryDelayLine::process_block`])
//! performs zero allocations. [`DryDelayLine::set_delay`] and
//! [`DryDelayLine::reset`] are also allocation-free and are called from the
//! audio thread only inside cold resource-swap handlers.

use neural_amp_modeler_rs::math::common::AlignedVec;

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

/// Bounded circular dry delay line (host-rate samples, per channel).
pub(crate) struct DryDelayLine {
    buf_l: AlignedVec<f32>,
    buf_r: AlignedVec<f32>,
    /// Index of the next sample to write.
    head: usize,
    /// Applied delay in samples (`< capacity`).
    delay: usize,
}

impl DryDelayLine {
    /// Wraps the two pre-allocated ring buffers (off-RT only — the buffers
    /// must be allocated by the caller, conventionally in `activate()`, the
    /// plugin's single documented allocation site).
    #[cold]
    pub(crate) fn new(buf_l: AlignedVec<f32>, buf_r: AlignedVec<f32>, delay: usize) -> Self {
        let capacity = buf_l.len().min(buf_r.len()).max(1);
        debug_assert_eq!(
            buf_l.len(),
            buf_r.len(),
            "dry delay ring channels must have equal capacity"
        );
        let mut line = Self {
            buf_l,
            buf_r,
            head: 0,
            delay: 0,
        };
        line.set_delay(delay.min(capacity - 1));
        line
    }

    /// Sets the applied delay, clamped to `capacity - 1`.
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
        self.delay = delay.min(self.buf_l.len() - 1);
    }

    /// Resets the ring to its zeroed initial state. RT-safe.
    #[inline(always)]
    pub(crate) fn reset(&mut self) {
        self.buf_l.fill(0.0);
        self.buf_r.fill(0.0);
        self.head = 0;
    }

    /// Pushes `n` host input samples into the ring and writes the
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
        let cap = self.buf_l.len();
        let d = self.delay;
        let n = n
            .min(in_l.len())
            .min(in_r.len())
            .min(out_l.len())
            .min(out_r.len());
        if n == 0 {
            return;
        }
        let bl = &mut self.buf_l;
        let br = &mut self.buf_r;
        let mut head = self.head;
        for i in 0..n {
            bl[head] = in_l[i];
            br[head] = in_r[i];
            head += 1;
            if head == cap {
                head = 0;
            }
            // Read position: `delay` samples behind the just-written slot.
            let mut rp = head + cap - 1 - d;
            if rp >= cap {
                rp -= cap;
            }
            out_l[i] = bl[rp];
            out_r[i] = br[rp];
        }
        self.head = head;
    }
}

#[cfg(test)]
mod dry_delay_test {
    use super::*;

    /// Builds a fresh `DryDelayLine` with `capacity` samples per channel.
    fn make_line(capacity: usize, delay: usize) -> DryDelayLine {
        let capacity = capacity.max(1);
        DryDelayLine::new(
            AlignedVec::new(capacity, 0.0f32).unwrap(),
            AlignedVec::new(capacity, 0.0f32).unwrap(),
            delay,
        )
    }

    /// Runs `input` through a fresh `DryDelayLine` in fixed-size blocks,
    /// returning the concatenated delayed output (L channel; R asserted equal).
    fn run_blocks(capacity: usize, delay: usize, input: &[f32], block: usize) -> Vec<f32> {
        let mut line = make_line(capacity, delay);
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

    #[test]
    fn test_delay_zero_is_passthrough() {
        let input: Vec<f32> = (0..64).map(|i| i as f32 * 0.01).collect();
        let out = run_blocks(128, 0, &input, 16);
        for i in 0..input.len() {
            assert_eq!(out[i], input[i], "delay=0 must be exact passthrough at {i}");
        }
    }

    #[test]
    fn test_impulse_peak_aligned_at_delay() {
        // Impulse at index 0 must reappear exactly `delay` samples later —
        // the dry/wet time-alignment invariant (F-DSP-008 acceptance).
        for &delay in &[1usize, 12, 64, 256, 511] {
            let cap = delay + 16;
            let mut input = vec![0.0f32; delay + 64];
            input[0] = 1.0;
            let out = run_blocks(cap, delay, &input, 1);
            for (i, &o) in out.iter().enumerate() {
                let expected = if i == delay { 1.0 } else { 0.0 };
                assert!(
                    (o - expected).abs() < 1e-7,
                    "delay={delay} impulse misaligned at {i}: got {o}, expected {expected}",
                );
            }
        }
    }

    #[test]
    fn test_steady_state_gain_and_dc_unchanged() {
        // Rollback condition (T4.1): the delay line must not alter gain or DC
        // in steady state. A constant signal comes out unchanged after the
        // initial zero-priming.
        let delay = 37usize;
        let cap = delay + 32;
        let input = vec![0.5f32; delay + 128];
        let out = run_blocks(cap, delay, &input, 23);
        for (i, &o) in out.iter().take(delay).enumerate() {
            assert_eq!(o, 0.0, "zero-priming at {i}");
        }
        for (i, &o) in out.iter().enumerate().skip(delay) {
            assert!(
                (o - 0.5).abs() < 1e-7,
                "steady-state gain altered at {i}: {o}",
            );
        }
    }

    #[test]
    fn test_multi_block_wrap_around_exact_shift() {
        // A ramp input spanning many ring wraps must reproduce the exact
        // `in[i - delay]` shift at every sample.
        let delay = 128usize;
        let cap = 160;
        let n = 2048;
        let input: Vec<f32> = (0..n).map(|i| (i % 997) as f32 * 0.001).collect();
        let out = run_blocks(cap, delay, &input, 7);
        for i in 0..n {
            let expected = if i >= delay { input[i - delay] } else { 0.0 };
            assert_eq!(out[i], expected, "wrap shift mismatch at {i}");
        }
    }

    #[test]
    fn test_set_delay_mid_stream_shifts_alignment() {
        // Changing the delay mid-stream must only change the read alignment —
        // the ring history is preserved, so the output continues to represent
        // the exact input shift for the *new* delay.
        let delay_a = 8usize;
        let delay_b = 32usize;
        let cap = 64;
        let n = 512;
        let input: Vec<f32> = (0..n).map(|i| i as f32 * 0.001).collect();
        let mut line = make_line(cap, delay_a);
        let mut out = vec![0.0f32; n];
        let mut out_r = vec![0.0f32; n];
        let split = 200usize;

        line.process_block(
            &input[..split],
            &input[..split],
            &mut out[..split],
            &mut out_r[..split],
            split,
        );
        line.set_delay(delay_b);
        line.process_block(
            &input[split..],
            &input[split..],
            &mut out[split..],
            &mut out_r[split..],
            n - split,
        );

        for (i, &o) in out.iter().enumerate() {
            let d = if i < split { delay_a } else { delay_b };
            let expected = if i >= d { input[i - d] } else { 0.0 };
            assert_eq!(o, expected, "mid-stream delay switch mismatch at {i}");
        }
    }

    #[test]
    fn test_delay_clamped_to_capacity() {
        let mut line = make_line(64, 10);
        line.set_delay(64);
        assert_eq!(line.delay, 63, "delay must clamp to capacity - 1");
        line.set_delay(usize::MAX);
        assert_eq!(line.delay, 63, "saturating clamp for oversized delays");
    }

    #[test]
    fn test_reset_clears_history() {
        let delay = 16usize;
        let mut line = make_line(32, delay);
        let input = vec![0.5f32; 64];
        let mut out = vec![0.0f32; 64];
        let mut out_r = vec![0.0f32; 64];
        line.process_block(&input, &input, &mut out, &mut out_r, 64);
        assert!((out[delay] - 0.5).abs() < 1e-7);

        line.reset();
        let input2 = vec![0.25f32; 64];
        line.process_block(&input2, &input2, &mut out, &mut out_r, 64);
        for (i, &o) in out.iter().take(delay).enumerate() {
            assert_eq!(o, 0.0, "reset must clear pre-history at {i}");
        }
        assert!((out[delay] - 0.25).abs() < 1e-7, "post-reset signal intact");
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
}

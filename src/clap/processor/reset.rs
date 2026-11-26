// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

impl<'a> NamClapProcessor<'a> {
    pub(crate) fn reset_state(&mut self) {
        // If already poisoned, avoid running DSP reset state transitions.
        if self.poisoned {
            return;
        }

        // Full in-place, zero-alloc DSP state reset on timeline discontinuity
        // (seek / loop relocation, `steady_time` regression).
        //
        // Reset target: "a freshly-initialized instance with the same
        // configuration". Every *temporal* state (filter/delay-line history,
        // phases, FIFOs, gate/adaptive FSM state, smoothers, scratch buffers)
        // is cleared in place; every *configuration* (model, IR, params,
        // applied oversampling factor, latency, preset) is preserved — nothing
        // is deallocated or recreated on the audio thread.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(any(test, feature = "testing"))]
            if self
                .shared
                .cold
                .test_reset_panic_injection
                .swap(false, Ordering::Relaxed)
            {
                panic!("Test injected reset panic");
            }

            // 1. Recurrent/FIR neural model state (WaveNet / LSTM / ConvNet /
            //    Linear / Container): two-phase reset, phase zero —
            //    `NamModel::prewarm_reset` clears the temporal state to the
            //    freshly-built condition WITHOUT the stabilization feed
            //    (ring/cepstral clearing only: in-place, zero-alloc, O(state);
            //    the integral prewarm's inline zero feed measured 1.81 ms p99
            //    for LSTM and 4.18 ms p99 for WaveNet A2 — moves
            //    off the audio callback into the per-callback amortization
            //    window below `process()`). The latch arms that window: every
            //    following callback drains a fixed zeroed-sample budget and
            //    holds its block on the latency-compensated dry path until
            //    the model reports convergence, then resumes normal wet
            //    processing with the first wet block equal to a fresh
            //    instance's first block (engine split-vs-integral bit-exact
            //    guarantee). Rate/buffer bookkeeping (`sample_rate`,
            //    `max_buffer_size`) is session-constant while active, so the
            //    integral reset's config re-statement is a no-op here.
            if let Some(model) = &mut self.model_l {
                model.prewarm_reset();
                self.prewarm_pending = true;
            }
            let is_stereo_config = self
                .shared
                .cold
                .selected_port_config
                .load(Ordering::Relaxed)
                == crate::clap::plugin::CONFIG_ID_STEREO;
            if is_stereo_config && let Some(model) = &mut self.model_r {
                model.prewarm_reset();
                self.prewarm_pending = true;
            }
            if self.prewarm_pending {
                self.shared
                    .rt_to_ui
                    .prewarm_pending
                    .store(true, Ordering::Relaxed);
            }
            self.prewarm_zero_budget_count = 0;

            // 2. Streaming resample adapter: clears its FIFOs, resets the
            //    inner resampler and re-arms the priming budget — post-reset
            //    cardinality matches a fresh activation.
            self.stream.reset();

            // 3. Polyphase resampler: clears phase accumulators and delay
            //    lines (keeps host/model rates — config).
            self.resampler.reset();

            // 4. Oversampling engines (L/R): clears every half-band delay line,
            //    phase counter and inter-stage scratch (keeps the applied
            //    factor — config).
            self.os_l.reset();
            self.os_r.reset();

            // 5. Cabinet slot (WAV IR convolution or neural NAM model): WAV IRs
            //    clear FDL/FIFO memory synchronously in place, whereas neural
            //    NAM slots never run `prewarm_reset`/`prewarm_step` in RT —
            //    if dirty, RT latches `cab_needs_reset` and requests a fresh
            //    off-RT converged slot while holding delayed dry.
            match &mut self.cab_slot {
                Some(cab @ crate::clap::extensions::cabinet::CabSlotDsp::WavIr(_)) => {
                    cab.reset_wav_ir();
                    self.cab_needs_reset = false;
                }
                Some(crate::clap::extensions::cabinet::CabSlotDsp::NamModel(_))
                    if self.cab_nam_dirty =>
                {
                    self.cab_needs_reset = true;
                    if !self.cab_bypass && !self.cab_nam_rebuild_in_flight {
                        self.cab_nam_generation = self.cab_nam_generation.wrapping_add(1);
                        self.shared
                            .cold
                            .cab_nam_rebuild_req
                            .store((self.cab_nam_generation << 1) | 1, Ordering::Release);
                        self.cab_nam_rebuild_in_flight = true;
                    }
                }
                Some(crate::clap::extensions::cabinet::CabSlotDsp::NamModel(_)) | None => {
                    self.cab_needs_reset = false;
                }
            }
            self.cab_bypass_xfade = BypassCrossfader::new(self.cab_bypass);
            self.cab_dry_delay.reset();

            // 6. Noise-gate hysteresis FSMs back to the initial Open state
            //    (unity multiplier, no ramp).
            self.silence_hyst.reset();
            self.mono_hyst.reset();

            // 7. Adaptive compute FSM back to Full, aborting any in-flight
            //    crossfade; preserves the user mode/override/slimmable config.
            self.adaptive_compute.reset(&self.rt_status);

            // 8. Parameter smoothers snap to their current target — the
            //    current params are reapplied without a ramp that originates
            //    in pre-reset state.
            self.smoother_in.snap_to_target();
            self.smoother_out.snap_to_target();
            self.smoother_cal_in.snap_to_target();
            self.smoother_cal_out.snap_to_target();

            // 9. Bypass crossfader snaps to the current bypass state (no
            //    mid-ramp blend left over from the pre-reset timeline).
            self.bypass_xfade = BypassCrossfader::new(self.params.bypass);

            // 10. Dry delay ring: clears pre-reset history while keeping the
            //     current applied delay (`cached_effective_latency` did not
            //     change — no resource was swapped).
            self.dry_delay.reset();

            // 11. Mutable intermediate/scratch buffers zeroed so no pre-reset
            //     sample survives in a stale slot. `buf_cab_zero_*` is omitted:
            //     it is a never-written all-zero source.
            self.buf_host_l.fill(0.0);
            self.buf_host_r.fill(0.0);
            self.buf_out_l.fill(0.0);
            self.buf_out_r.fill(0.0);
            self.buf_os_in_l.fill(0.0);
            self.buf_os_in_r.fill(0.0);
            self.buf_os_model_l.fill(0.0);
            self.buf_os_model_r.fill(0.0);
            self.buf_xfade_dry_l.fill(0.0);
            self.buf_xfade_dry_r.fill(0.0);
            self.buf_xfd_scratch_l.fill(0.0);
            self.buf_xfd_scratch_r.fill(0.0);
            self.buf_cab_dry_l.fill(0.0);
            self.buf_cab_dry_r.fill(0.0);
            self.buf_cab_nam_in_l.fill(0.0);
            self.buf_cab_nam_in_r.fill(0.0);

            // 12. Per-block caches a fresh activation starts with: gate cache
            //     invalidated (recomputed from the preserved params on the
            //     next block), modulation offsets zeroed and mono flag at the
            //     default. The one-shot RT-thread setup is deliberately not
            //     re-armed here: it is bound to the audio thread's lifetime
            //     (`start_processing`/`stop_processing`), so a seek or loop
            //     relocation never issues scheduler or CPU queries on the RT
            //     path. FTZ/DAZ is asserted per block by `FtzDazGuard`.
            self.scheduled_events.clear();
            self.process_mono = true;
            self.mod_input_gain = 0.0;
            self.mod_output_gain = 0.0;
            self.mod_gate_thresh = 0.0;
            self.cached_threshold_open_sq = 0.0;
            self.cached_threshold_close_sq = 0.0;
            self.cached_gate_params = GateParams::default();
            self.gate_dirty = true;
            self.musical_gate.reset();
        }));
        if let Err(err) = result {
            self.latch_poisoned();
            drop(err);
        }
    }
}

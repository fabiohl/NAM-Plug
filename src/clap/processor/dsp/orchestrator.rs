// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! DSP Orchestrator: manages parameter event scheduling, channel routing, and chunked audio processing.

pub mod audio_loop;
mod dry;
mod events;
mod telemetry;
mod wet;
pub mod worker;

pub(crate) use worker::ScheduledEvent;

use super::super::NamClapProcessor;
pub(crate) use crate::clap::extensions::gate_mode::GateMode;
pub(crate) use crate::clap::processor::dsp::{channels, peaks};
pub(crate) use clack_plugin::events::event_types::{ParamModEvent, ParamValueEvent};
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_EVENT_TIMING_ANOMALY, RT_STATUS_HOST_CONTRACT_VIOLATION,
};
use neural_amp_modeler_rs::dsp::gate::GateState;
pub(crate) use neural_amp_modeler_rs::dsp::gate_flags;
pub(crate) use neural_amp_modeler_rs::dsp::pipeline::DspPipelineContext;
pub(crate) use neural_amp_modeler_rs::models::NamModel;
use std::sync::atomic::Ordering;

/// Maximum number of host input events scheduled within a single block.
///
/// Saturation past this budget is **explicit, not invisible**: the scheduler
/// stops draining, raises `RT_STATUS_SPSC_DRAIN_TRUNCATED`,
/// and the main thread logs the overflow off-RT via `emit_pending_logs()`.
/// The remaining events of the block are rejected — the plugin never silently
/// drops a partial automation envelope in release without a flag.
const MAX_SCHEDULED_EVENTS: usize = 4096;

/// Minimum sub-block length retained when fusing gain-only event boundaries.
///
/// Gain parameters (`PARAM_INPUT_GAIN`/`PARAM_OUTPUT_GAIN`, value or
/// modulation) are already continuously smoothed by [`ParamSmoother`], so a
/// gain event does **not** need to end the inference sub-block. When this
/// constant is `> 1`, consecutive gain events inside a window are fused into
/// one inference call and applied at their exact sample offsets through the
/// piecewise IIR ramp ([`worker::GainStep`]) — numerically identical to the
/// legacy per-event split (the neural model is block-invariant). Boundaries
/// that must stay sample-accurate (`gate`, `bypass`, `oversample`, `slim`,
/// adaptive/activation) are unaffected.
///
/// A value of `1` disables fusion entirely and reproduces the un-fused behavior
/// exactly (rollback switch). The window is additionally capped by
/// [`MAX_GAIN_STEPS`]; exceeding it forces a boundary so the fused window can
/// never grow unbounded.
const MIN_SPLIT_SAMPLES: usize = 16;

/// Upper bound on gain targeting steps staged per fused sub-block. Sized to
/// cover a dense 64-sample automation flood without forcing extra boundaries;
/// larger windows fall back to a boundary (never a correctness issue, only a
/// smaller optimization). Bounded so the on-stack step buffer stays tiny.
const MAX_GAIN_STEPS: usize = 64;

/// Metrics accumulated across sub-blocks within an audio processing cycle.
#[derive(Default, Clone, Copy, Debug)]
pub(crate) struct BlockMetricsAccumulator {
    pub in_peak_l: f32,
    pub in_peak_r: f32,
    pub in_sum_sq_l: f32,
    pub in_sum_sq_r: f32,
    pub out_peak_l: f32,
    pub out_peak_r: f32,
    pub out_sum_sq_l: f32,
    pub out_sum_sq_r: f32,
    pub total_samples: usize,
}

/// Whether an event is allowed to end the current inference sub-block.
/// Only the smoothed gain parameters are boundary-free; everything else
/// (gate threshold, bypass, oversample, slim override, adaptive compute,
/// activation precision) keeps an exact sample-accurate boundary.
#[inline(always)]
fn event_requires_boundary(param_id: u32) -> bool {
    !worker::is_fusable_gain_param(param_id)
}

impl<'a> NamClapProcessor<'a> {
    #[inline(always)]
    pub(crate) fn process_dsp_audio(
        &mut self,
        audio: &mut Audio,
        input_events: &InputEvents,
        start_nanos: u64,
    ) -> Result<ProcessStatus, PluginError> {
        // Implementation note: when `prewarm_pending` is armed the
        // caller (`process()` above) drains the amortization window and
        // contains the block (dry via `dry_delay`, crossfade/gate/FSM held)
        // in `process_dry_contained_block` — this function is not reached
        // while the non-converged model is outstanding.
        //
        // Non-finite containment: a non-finite input sample poisons the model. The
        // engine's input stage raises `RT_STATUS_NON_FINITE_INPUT_DETECTED`
        // (and sanitizes only its own slices — never the downstream scratch
        // buffers); the containment here restores the model to the
        // freshly-built state in O(1) (`prewarm_reset`, no stabilization feed)
        // and arms the *same* amortization window used by `reset()`, holding
        // this and the following callbacks on the latency-compensated dry path
        // until the model reconverges. This keeps the poisoning callback free
        // of the multi-millisecond synchronous reset (measured 1.81 ms p99 LSTM
        // / 4.18 ms p99 WaveNet A2) and guarantees the first post-convergence
        // wet block equals a fresh instance's first block. The flag is cleared
        // here so the containment runs once per poisoning — never once per
        // block.
        if self
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_NON_FINITE_INPUT_DETECTED)
        {
            self.shared
                .rt_to_ui
                .non_finite_epoch
                .fetch_add(1, Ordering::Relaxed);
            self.rt_status.clear_flag(
                neural_amp_modeler_rs::common::spsc::RT_STATUS_NON_FINITE_INPUT_DETECTED,
            );
            let is_stereo_config = self
                .shared
                .cold
                .selected_port_config
                .load(Ordering::Relaxed)
                == crate::clap::plugin::CONFIG_ID_STEREO;
            if let Some(model) = self.model_l.as_mut() {
                model.prewarm_reset();
                self.prewarm_pending = true;
            }
            if is_stereo_config && let Some(model) = self.model_r.as_mut() {
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
            // The musical gate envelope may have ingested the non-finite block
            // (its trigger runs before the input stage sanitizes); restore its
            // freshly-built channel state so the reconverged wet path starts clean.
            self.musical_gate.reset();
            // The dry delay line must not replay pre-fault history (which would
            // leak pre-containment audio through the bypass/crossfade path), and
            // the parameter ramps must not originate in pre-fault state.
            self.dry_delay.reset();
            self.smoother_in.snap_to_target();
            self.smoother_out.snap_to_target();
            self.smoother_cal_in.snap_to_target();
            self.smoother_cal_out.snap_to_target();
            // Scratch hygiene identical to `reset()`: no pre-fault sample
            // survives in a mutable slot the wet path could read before
            // rewriting after reconvergence (`buf_cab_zero_*` is never written
            // and stays all-zero).
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
            return self.process_dry_contained_block(audio, input_events, start_nanos);
        }

        // F-PLUG-06 / T-07.3: The orchestrator is the single owner of
        // `request_restart()` at block boundaries. Every oversampling writer
        // latches `restart_wanted`, consumed once at the end of this callback
        // (coalescing multiple changes within the block, including GUI/SPSC
        // changes applied before the DSP block began).
        self.schedule_input_events(input_events);

        let event_count = self.scheduled_events.len();
        let mut event_idx = 0;
        let mut metrics = BlockMetricsAccumulator::default();
        let mut last_gate_state = GateState::Open;
        let mut any_active = false;
        let mut input_clipped = false;

        self.for_each_port_slice(
            audio,
            |proc, slice_start, slice_len, is_last, out_l, out_r| {
                if slice_start == 0 {
                    proc.configure_channel_gate_and_model_flag();
                    proc.bypass_xfade.trigger(proc.params.bypass);
                    proc.cab_bypass_xfade.trigger(proc.cab_bypass);
                    metrics = BlockMetricsAccumulator::default();
                    last_gate_state = GateState::Open;
                    any_active = false;
                    input_clipped = false;
                }

                proc.process_wet_sub_blocks(
                    slice_start,
                    slice_len,
                    out_l,
                    out_r,
                    &mut event_idx,
                    &mut metrics,
                    &mut last_gate_state,
                    &mut any_active,
                    &mut input_clipped,
                    is_last,
                );

                if is_last {
                    proc.report_gate_telemetry(any_active, last_gate_state);
                    proc.update_rms_and_telemetry(
                        &metrics,
                        proc.shared.cold.sample_rate.load(Ordering::Relaxed),
                    );
                }
            },
        )?;

        // Defensive: if audio port pairs was empty or any remaining events were not processed
        // (e.g. n_samples == 0 or 0 audio channels), apply all pending events now to guarantee
        // zero event loss.
        if event_idx < event_count {
            let mut remaining_applied = false;
            while event_idx < event_count {
                core::hint::cold_path();
                self.rt_status.set_flag(RT_STATUS_EVENT_TIMING_ANOMALY);
                self.scheduled_events[event_idx].time = 0;
                let evt = self.scheduled_events[event_idx];
                self.param_target()
                    .apply(evt.param_id, evt.value, evt.is_mod, true);
                event_idx += 1;
                remaining_applied = true;
            }
            if remaining_applied {
                self.refresh_gate_config();
                self.bypass_xfade.trigger(self.params.bypass);
                self.cab_bypass_xfade.trigger(self.cab_bypass);
            }
        }

        // F-PLUG-06 / T-07.3: Request host restart once at block boundary if
        // oversampling was latched during this callback.
        self.maybe_request_restart();

        self.process_telemetry(start_nanos);

        #[cfg(feature = "heap-audit")]
        if crate::clap::processor::heap_audit::is_armed()
            && crate::clap::processor::heap_audit::window_violations() > 0
        {
            self.rt_status
                .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC);
            return Ok(ProcessStatus::Sleep);
        }

        Ok(ProcessStatus::Continue)
    }

    fn for_each_port_slice<F>(
        &mut self,
        audio: &mut Audio,
        mut on_slice: F,
    ) -> Result<(), PluginError>
    where
        F: FnMut(&mut Self, usize, usize, bool, Option<&mut [f32]>, Option<&mut [f32]>),
    {
        for mut port_pair in audio {
            let n_samples_raw = port_pair.frames_count() as usize;
            if n_samples_raw > self.max_frames_count {
                self.rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
                let n = self.max_frames_count as u32;
                if self.rt_status.last_n_samples.load(Ordering::Relaxed) != n {
                    self.rt_status.last_n_samples.store(n, Ordering::Relaxed);
                }
                let Some(channel_pairs) = port_pair.channels()?.into_f32() else {
                    continue;
                };
                let mut channel_iter = channel_pairs.into_iter();
                let mut pair_l = channel_iter.next();
                let mut pair_r = channel_iter.next();

                let has_r_input = matches!(
                    pair_r,
                    Some(
                        ChannelPair::InputOutput(..)
                            | ChannelPair::InPlace(_)
                            | ChannelPair::InputOnly(_)
                    )
                );
                let channel_count = if pair_r.is_some() { 2 } else { 1 };
                if self
                    .shared
                    .rt_to_ui
                    .active_channel_count
                    .load(Ordering::Relaxed)
                    != channel_count
                {
                    self.shared
                        .rt_to_ui
                        .active_channel_count
                        .store(channel_count, Ordering::Relaxed);
                }
                #[cfg(feature = "dual-mono")]
                {
                    self.process_mono = !has_r_input;
                }
                #[cfg(not(feature = "dual-mono"))]
                {
                    let _ = has_r_input;
                    self.process_mono = true;
                }

                let mut slice_start = 0usize;
                while slice_start < n_samples_raw {
                    let slice_len = (n_samples_raw - slice_start).min(self.max_frames_count);
                    let is_last_slice = slice_start + slice_len >= n_samples_raw;
                    let port_input_active =
                        self.shared.cold.port_input_active.load(Ordering::Relaxed);
                    let port_output_active =
                        self.shared.cold.port_output_active.load(Ordering::Relaxed);

                    channels::copy_channel_input(
                        pair_l.as_ref(),
                        &mut self.buf_host_l,
                        slice_start,
                        slice_len,
                    );
                    if !self.process_mono {
                        channels::copy_channel_input(
                            pair_r.as_ref(),
                            &mut self.buf_host_r,
                            slice_start,
                            slice_len,
                        );
                    }
                    if !port_input_active {
                        self.buf_host_l[..slice_len].fill(0.0);
                        self.buf_host_r[..slice_len].fill(0.0);
                    }
                    let out_l = if port_output_active {
                        channels::get_channel_output(pair_l.as_mut(), slice_start, slice_len)
                    } else {
                        None
                    };
                    let out_r = if port_output_active {
                        channels::get_channel_output(pair_r.as_mut(), slice_start, slice_len)
                    } else {
                        None
                    };

                    on_slice(self, slice_start, slice_len, is_last_slice, out_l, out_r);
                    slice_start += slice_len;
                }
                continue;
            }

            let n_samples = n_samples_raw.min(self.max_frames_count);
            if n_samples == 0 {
                continue;
            }
            let n = n_samples as u32;
            if self.rt_status.last_n_samples.load(Ordering::Relaxed) != n {
                self.rt_status.last_n_samples.store(n, Ordering::Relaxed);
            }

            let port_input_active = self.shared.cold.port_input_active.load(Ordering::Relaxed);
            let port_output_active = self.shared.cold.port_output_active.load(Ordering::Relaxed);

            let Some((out_l, out_r)) = channels::extract_channels(
                &mut port_pair,
                &mut self.buf_host_l,
                &mut self.buf_host_r,
                &self.shared.rt_to_ui.active_channel_count,
                &mut self.process_mono,
                n_samples,
            )?
            else {
                continue;
            };

            if !port_input_active {
                self.buf_host_l[..n_samples].fill(0.0);
                self.buf_host_r[..n_samples].fill(0.0);
            }
            let (out_l, out_r) = if port_output_active {
                (out_l, out_r)
            } else {
                (None, None)
            };
            on_slice(self, 0, n_samples, true, out_l, out_r);
        }
        Ok(())
    }
}

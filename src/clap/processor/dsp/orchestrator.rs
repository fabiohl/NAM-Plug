// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! DSP Orchestrator: manages parameter event scheduling, channel routing, and chunked audio processing.

pub mod audio_loop;
pub mod worker;

pub(crate) use worker::ScheduledEvent;

use super::super::NamClapProcessor;
use crate::clap::extensions::gate_mode::GateMode;
use crate::clap::plugin::PendingRestartOs;
use crate::clap::processor::dsp::{channels, peaks};
use clack_plugin::events::event_types::{ParamModEvent, ParamValueEvent};
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_EVENT_TIMING_ANOMALY, RT_STATUS_HOST_CONTRACT_VIOLATION,
};
use neural_amp_modeler_rs::dsp::gate::GateState;
use neural_amp_modeler_rs::dsp::gate_flags;
use neural_amp_modeler_rs::dsp::pipeline::DspPipelineContext;
use neural_amp_modeler_rs::models::NamModel;
use std::sync::atomic::Ordering;

/// Maximum number of host input events scheduled within a single block.
///
/// Saturation past this budget is **explicit, not invisible**: the scheduler
/// stops draining, raises `RT_STATUS_SPSC_DRAIN_TRUNCATED`,
/// and the main thread logs the overflow off-RT via `emit_pending_logs()`.
/// The remaining events of the block are rejected — the plugin never silently
/// drops a partial automation envelope in release without a flag.
const MAX_SCHEDULED_EVENTS: usize = 4096;

/// Minimum sub-block length retained when fusing gain-only event boundaries
/// (F-UX-03).
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
        // Non-finite containment (F-UX-04): a non-finite input sample poisons the model. The
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
            // The dry delay line must not replay pre-fault history (which would
            // leak pre-containment audio through the bypass/crossfade path), and
            // the parameter ramps must not originate in pre-fault state.
            self.dry_delay.reset();
            self.smoother_in.snap_to_target();
            self.smoother_out.snap_to_target();
            self.smoother_cal_in.snap_to_target();
            self.smoother_cal_out.snap_to_target();
            // Scratch hygiene identical to `reset()`: no pre-fault sample
            // survives in a slot the wet path could read before rewriting after
            // reconvergence.
            self.buf_host_l.fill(0.0);
            self.buf_host_r.fill(0.0);
            self.buf_mid_l.fill(0.0);
            self.buf_mid_r.fill(0.0);
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

        // F-PLUG-06 / T-07.3: Track pending restart factor for latency-policy enforcement.
        // The orchestrator is the single owner of request_restart() at block boundaries,
        // coalescing multiple changes within the same block into a single request.
        let pending_before = PendingRestartOs::load(
            &self.shared.cold.pending_restart_os_factor,
            Ordering::Relaxed,
        );
        self.schedule_input_events(input_events);

        let event_count = self.scheduled_events.len();
        let mut event_idx = 0;

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
                let is_stereo_config = self
                    .shared
                    .cold
                    .selected_port_config
                    .load(Ordering::Relaxed)
                    == crate::clap::plugin::CONFIG_ID_STEREO;
                #[cfg(feature = "dual-mono")]
                {
                    self.process_mono = !has_r_input;
                }
                #[cfg(not(feature = "dual-mono"))]
                {
                    let _ = has_r_input;
                    self.process_mono = true;
                }
                if self
                    .shared
                    .ui_to_rt
                    .host_r_deactivated
                    .load(Ordering::Acquire)
                {
                    self.process_mono = true;
                }
                if !self.process_mono {
                    self.cached_gate_params.mono_epsilon =
                        if is_stereo_config { -1.0 } else { 1e-4 };
                } else {
                    self.cached_gate_params.mono_epsilon = 1e-4;
                }

                self.refresh_gate_config();

                let model_load_fail = self.model_l.is_none() && !self.params.bypass;
                let current_fail_flag = self
                    .rt_status
                    .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED);
                if model_load_fail != current_fail_flag {
                    if model_load_fail {
                        self.rt_status.set_flag(
                            neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED,
                        );
                    } else {
                        self.rt_status.clear_flag(
                            neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED,
                        );
                    }
                }

                let mut metrics = BlockMetricsAccumulator::default();
                let mut last_gate_state = GateState::Open;
                let mut any_active = false;
                let mut input_clipped = false;

                self.bypass_xfade.trigger(self.params.bypass);
                self.cab_bypass_xfade.trigger(self.cab_bypass);

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
                    let (out_l, out_r) = if port_output_active {
                        (
                            channels::get_channel_output(pair_l.as_mut(), slice_start, slice_len),
                            channels::get_channel_output(pair_r.as_mut(), slice_start, slice_len),
                        )
                    } else {
                        (None, None)
                    };

                    self.process_wet_sub_blocks(
                        slice_start,
                        slice_len,
                        out_l,
                        out_r,
                        &mut event_idx,
                        &mut metrics,
                        &mut last_gate_state,
                        &mut any_active,
                        &mut input_clipped,
                        is_last_slice,
                    );

                    slice_start += slice_len;
                }

                self.report_gate_telemetry(any_active, last_gate_state);

                self.update_rms_and_telemetry(
                    &metrics,
                    self.shared.cold.sample_rate.load(Ordering::Relaxed),
                );
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

            let is_stereo_config = self
                .shared
                .cold
                .selected_port_config
                .load(Ordering::Relaxed)
                == crate::clap::plugin::CONFIG_ID_STEREO;
            if self
                .shared
                .ui_to_rt
                .host_r_deactivated
                .load(Ordering::Acquire)
            {
                self.process_mono = true;
            }
            if !self.process_mono {
                self.cached_gate_params.mono_epsilon = if is_stereo_config { -1.0 } else { 1e-4 };
            } else {
                self.cached_gate_params.mono_epsilon = 1e-4;
            }

            if !port_input_active {
                self.buf_host_l[..n_samples].fill(0.0);
                self.buf_host_r[..n_samples].fill(0.0);
            }
            let (out_l, out_r) = if port_output_active {
                (out_l, out_r)
            } else {
                (None, None)
            };

            self.refresh_gate_config();

            let model_load_fail = self.model_l.is_none() && !self.params.bypass;
            let current_fail_flag = self
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED);
            if model_load_fail != current_fail_flag {
                if model_load_fail {
                    self.rt_status
                        .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED);
                } else {
                    self.rt_status.clear_flag(
                        neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED,
                    );
                }
            }

            let mut metrics = BlockMetricsAccumulator::default();
            let mut last_gate_state = GateState::Open;
            let mut any_active = false;
            let mut input_clipped = false;

            // Sync crossfader with current bypass state. If bypass was changed
            // by SPSC event sync (process_events) before this block, trigger
            // the crossfade to avoid click artifacts.
            self.bypass_xfade.trigger(self.params.bypass);
            self.cab_bypass_xfade.trigger(self.cab_bypass);

            self.process_wet_sub_blocks(
                0,
                n_samples,
                out_l,
                out_r,
                &mut event_idx,
                &mut metrics,
                &mut last_gate_state,
                &mut any_active,
                &mut input_clipped,
                true,
            );

            self.report_gate_telemetry(any_active, last_gate_state);

            self.update_rms_and_telemetry(
                &metrics,
                self.shared.cold.sample_rate.load(Ordering::Relaxed),
            );
        }

        // Defensive: if audio port pairs was empty or any remaining events were not processed
        // (e.g. n_samples == 0 or 0 audio channels), apply all pending events now to guarantee
        // zero event loss.
        if event_idx < event_count {
            let mut remaining_applied = false;
            while event_idx < event_count {
                core::hint::cold_path();
                self.rt_status.set_flag(RT_STATUS_EVENT_TIMING_ANOMALY);
                self.scheduled_events[event_idx].time = 0;
                let evt = &self.scheduled_events[event_idx];
                worker::apply_scheduled_event(
                    evt.param_id,
                    evt.value,
                    evt.is_mod,
                    &mut self.params,
                    &mut self.cab_bypass,
                    &mut self.cal_config,
                    &self.model_cal_meta,
                    &mut self.model_input_mult_adj,
                    &mut self.model_output_mult_adj,
                    &mut self.smoother_in,
                    &mut self.smoother_out,
                    &mut self.smoother_cal_in,
                    &mut self.smoother_cal_out,
                    &mut self.gate_dirty,
                    &mut self.gate_mode,
                    &mut self.musical_gate,
                    &mut self.mod_input_gain,
                    &mut self.mod_output_gain,
                    &mut self.mod_gate_thresh,
                    &mut self.adaptive_compute,
                    &self.rt_status,
                    &self.shared.ui_to_rt,
                    self.gain_lut,
                    self.shared.cold.buffer_size.load(Ordering::Relaxed),
                    &self.shared.cold.pending_restart_os_factor,
                    true,
                );
                event_idx += 1;
                remaining_applied = true;
            }
            if remaining_applied {
                self.refresh_gate_config();
                self.bypass_xfade.trigger(self.params.bypass);
                self.cab_bypass_xfade.trigger(self.cab_bypass);
            }
        }

        // F-PLUG-06 / T-07.3: Request host restart once at block boundary if oversampling
        // was changed during active processing.
        let pending_after = PendingRestartOs::load(
            &self.shared.cold.pending_restart_os_factor,
            Ordering::Relaxed,
        );
        if pending_after != pending_before && pending_after != PendingRestartOs::None {
            self.host.request_restart();
        }

        self.process_telemetry(start_nanos);

        #[cfg(feature = "heap-audit")]
        if neural_amp_modeler_rs::common::alloc_audit::AUDIT_ENABLED.load(Ordering::Relaxed) {
            let allocs = neural_amp_modeler_rs::common::alloc_audit::get_alloc_count();
            if allocs > 0 {
                self.rt_status
                    .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC);
                return Ok(ProcessStatus::Sleep);
            }
        }

        Ok(ProcessStatus::Continue)
    }

    #[inline(always)]
    fn update_rms_and_telemetry(&mut self, metrics: &BlockMetricsAccumulator, sample_rate: u32) {
        let sr = if sample_rate > 0 {
            sample_rate as f32
        } else {
            48000.0
        };
        let n = metrics.total_samples;
        if n > 0 {
            let n_f = n as f32;
            let block_in_rms_l = (metrics.in_sum_sq_l / n_f).sqrt();
            let block_in_rms_r = (metrics.in_sum_sq_r / n_f).sqrt();
            let block_out_rms_l = (metrics.out_sum_sq_l / n_f).sqrt();
            let block_out_rms_r = (metrics.out_sum_sq_r / n_f).sqrt();

            let alpha = (1.0 - (-n_f / (sr * 0.300)).exp()).clamp(0.0, 1.0);
            self.in_rms_ewma_l += alpha * (block_in_rms_l - self.in_rms_ewma_l);
            self.in_rms_ewma_r += alpha * (block_in_rms_r - self.in_rms_ewma_r);
            self.out_rms_ewma_l += alpha * (block_out_rms_l - self.out_rms_ewma_l);
            self.out_rms_ewma_r += alpha * (block_out_rms_r - self.out_rms_ewma_r);
        }

        peaks::store_peaks(self.shared, metrics.out_peak_l, metrics.out_peak_r);
        peaks::store_input_peaks(self.shared, metrics.in_peak_l, metrics.in_peak_r);
        peaks::store_rms(
            self.shared,
            self.in_rms_ewma_l,
            self.in_rms_ewma_r,
            self.out_rms_ewma_l,
            self.out_rms_ewma_r,
        );
    }

    #[expect(clippy::too_many_arguments)]
    fn process_wet_sub_blocks(
        &mut self,
        slice_base: usize,
        n_samples: usize,
        mut out_l: Option<&mut [f32]>,
        mut out_r: Option<&mut [f32]>,
        event_idx: &mut usize,
        metrics: &mut BlockMetricsAccumulator,
        last_gate_state: &mut GateState,
        any_active: &mut bool,
        input_clipped: &mut bool,
        is_last_slice: bool,
    ) {
        let mut gain_steps = [worker::GainStep {
            offset: 0,
            target: 0.0,
            is_input: true,
        }; MAX_GAIN_STEPS];

        let mut block_offset = 0usize;
        let mut output_offset = 0usize;
        let shared_sample_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        let event_count = self.scheduled_events.len();

        while block_offset < n_samples {
            let current_abs = slice_base + block_offset;
            let mut out_of_order_applied = false;
            while *event_idx < event_count && self.scheduled_events[*event_idx].time < current_abs {
                // Out-of-order or late timestamp (arrived with time < current_abs).
                // Defensive containment: clamp time to current_abs and apply immediately
                // so no parameter update is silently discarded (zero event loss).
                core::hint::cold_path();
                self.rt_status.set_flag(RT_STATUS_EVENT_TIMING_ANOMALY);
                self.scheduled_events[*event_idx].time = current_abs;
                let evt = &self.scheduled_events[*event_idx];
                worker::apply_scheduled_event(
                    evt.param_id,
                    evt.value,
                    evt.is_mod,
                    &mut self.params,
                    &mut self.cab_bypass,
                    &mut self.cal_config,
                    &self.model_cal_meta,
                    &mut self.model_input_mult_adj,
                    &mut self.model_output_mult_adj,
                    &mut self.smoother_in,
                    &mut self.smoother_out,
                    &mut self.smoother_cal_in,
                    &mut self.smoother_cal_out,
                    &mut self.gate_dirty,
                    &mut self.gate_mode,
                    &mut self.musical_gate,
                    &mut self.mod_input_gain,
                    &mut self.mod_output_gain,
                    &mut self.mod_gate_thresh,
                    &mut self.adaptive_compute,
                    &self.rt_status,
                    &self.shared.ui_to_rt,
                    self.gain_lut,
                    self.shared.cold.buffer_size.load(Ordering::Relaxed),
                    &self.shared.cold.pending_restart_os_factor,
                    true,
                );
                *event_idx += 1;
                out_of_order_applied = true;
            }

            if out_of_order_applied {
                self.refresh_gate_config();
                self.bypass_xfade.trigger(self.params.bypass);
                self.cab_bypass_xfade.trigger(self.cab_bypass);
            }

            // Resolve the next sub-block boundary: the first event that
            // requires an exact split (any non-gain param), or the
            // gain-step cap. Gain events in `[current_abs, slice_base + sub_end)` are
            // fused into this sub-block and applied at their exact sample
            // offsets through the piecewise IIR ramp, so the inference
            // call is not fragmented by automation of smoothed gains.
            let fuse = MIN_SPLIT_SAMPLES > 1
                && self.scheduled_events_sorted
                && n_samples <= neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF
                && !self.bypass_xfade.active
                && !self.params.bypass;
            let mut sub_end = n_samples;
            {
                let mut i = *event_idx;
                let mut gains = 0usize;
                while i < event_count {
                    let t = self.scheduled_events[i].time;
                    if t < current_abs {
                        i += 1;
                        continue;
                    }
                    if t >= slice_base + n_samples {
                        break;
                    }
                    let rel_t = t - slice_base;
                    if !fuse || event_requires_boundary(self.scheduled_events[i].param_id) {
                        sub_end = rel_t;
                        break;
                    }
                    gains += 1;
                    if gains > MAX_GAIN_STEPS {
                        sub_end = rel_t;
                        break;
                    }
                    i += 1;
                }
            }

            let mut n_steps = 0usize;
            let sub_n = sub_end - block_offset;

            // Stage the fused gain events without jumping the smoother:
            // the target for each offset is replayed inside the ramp.
            if fuse && sub_n > 0 {
                while *event_idx < event_count {
                    let t = self.scheduled_events[*event_idx].time;
                    if t >= slice_base + sub_end {
                        break;
                    }
                    let param_id = self.scheduled_events[*event_idx].param_id;
                    if event_requires_boundary(param_id) {
                        break;
                    }
                    let value = self.scheduled_events[*event_idx].value;
                    let is_mod = self.scheduled_events[*event_idx].is_mod;
                    let gain_target = worker::apply_scheduled_event(
                        param_id,
                        value,
                        is_mod,
                        &mut self.params,
                        &mut self.cab_bypass,
                        &mut self.cal_config,
                        &self.model_cal_meta,
                        &mut self.model_input_mult_adj,
                        &mut self.model_output_mult_adj,
                        &mut self.smoother_in,
                        &mut self.smoother_out,
                        &mut self.smoother_cal_in,
                        &mut self.smoother_cal_out,
                        &mut self.gate_dirty,
                        &mut self.gate_mode,
                        &mut self.musical_gate,
                        &mut self.mod_input_gain,
                        &mut self.mod_output_gain,
                        &mut self.mod_gate_thresh,
                        &mut self.adaptive_compute,
                        &self.rt_status,
                        &self.shared.ui_to_rt,
                        self.gain_lut,
                        self.shared.cold.buffer_size.load(Ordering::Relaxed),
                        &self.shared.cold.pending_restart_os_factor,
                        false,
                    );
                    // `is_fusable_gain_param` is the single source of truth
                    // for both this loop and the `Some(GainTarget)` arms of
                    // `apply_scheduled_event`; a `None` here means the two
                    // drifted and an event would be silently dropped.
                    debug_assert!(
                        gain_target.is_some(),
                        "fusable gain param returned no target (classification drift)"
                    );
                    if let Some(gt) = gain_target
                        && n_steps < MAX_GAIN_STEPS
                    {
                        gain_steps[n_steps] = worker::GainStep {
                            offset: (t - current_abs) as u32,
                            target: gt.target,
                            is_input: gt.is_input,
                        };
                        n_steps += 1;
                    }
                    *event_idx += 1;
                }
            }

            if sub_n > 0 {
                let bypass = self.params.bypass;
                let process_mono = self.process_mono;
                let is_stereo_config = self
                    .shared
                    .cold
                    .selected_port_config
                    .load(Ordering::Relaxed)
                    == crate::clap::plugin::CONFIG_ID_STEREO;
                let mut dummy_none_model: Option<Box<neural_amp_modeler_rs::models::StaticModel>> =
                    None;
                let active_model_r: &mut Option<Box<neural_amp_modeler_rs::models::StaticModel>> =
                    if is_stereo_config {
                        &mut self.model_r
                    } else {
                        &mut dummy_none_model
                    };

                let sub_m = {
                    let mut ctx = DspPipelineContext {
                        resampler: &mut self.resampler,
                        os_l: &mut self.os_l,
                        os_r: &mut self.os_r,
                        active_model_l: &mut self.model_l,
                        active_model_r,
                        input_gain_mult: self.model_input_mult_adj,
                        output_gain_mult: self.model_output_mult_adj,
                        gate_params: &self.cached_gate_params,
                        silence_hysteresis: &mut self.silence_hyst,
                        mono_hysteresis: &mut self.mono_hyst,
                        threshold_open_sq: self.cached_threshold_open_sq,
                        threshold_close_sq: self.cached_threshold_close_sq,
                        process_mono: &mut self.process_mono,
                        rt_status: &self.rt_status,
                        adaptive: &mut self.adaptive_compute,
                        bridge_writer: None,
                        conv: None,
                        conv_pair: None,
                    };

                    let mut scratch = audio_loop::SubBlockScratch {
                        buf_host_l: &mut self.buf_host_l,
                        buf_host_r: &mut self.buf_host_r,
                        buf_out_l: &mut self.buf_out_l,
                        buf_out_r: &mut self.buf_out_r,
                        buf_os_in_l: &mut self.buf_os_in_l,
                        buf_os_in_r: &mut self.buf_os_in_r,
                        buf_os_model_l: &mut self.buf_os_model_l,
                        buf_os_model_r: &mut self.buf_os_model_r,
                        buf_xfade_dry_l: &mut self.buf_xfade_dry_l,
                        buf_xfade_dry_r: &mut self.buf_xfade_dry_r,
                        buf_xfd_scratch_l: &mut self.buf_xfd_scratch_l,
                        buf_xfd_scratch_r: &mut self.buf_xfd_scratch_r,
                    };
                    let mut cab_stage = audio_loop::CabinetStage {
                        cab_slot: &mut self.cab_slot,
                        cab_bypass: self.cab_bypass,
                        cab_needs_reset: &mut self.cab_needs_reset,
                        cab_bypass_xfade: &mut self.cab_bypass_xfade,
                        cab_dry_delay: &mut self.cab_dry_delay,
                        buf_cab_dry_l: &mut self.buf_cab_dry_l,
                        buf_cab_dry_r: &mut self.buf_cab_dry_r,
                        buf_cab_nam_in_l: &mut self.buf_cab_nam_in_l,
                        buf_cab_nam_in_r: &mut self.buf_cab_nam_in_r,
                        buf_cab_zero_l: &self.buf_cab_zero_l,
                        buf_cab_zero_r: &self.buf_cab_zero_r,
                    };

                    audio_loop::process_sub_block(
                        block_offset,
                        sub_n,
                        &mut out_l,
                        &mut out_r,
                        output_offset,
                        &mut ctx,
                        &mut self.stream,
                        bypass,
                        process_mono,
                        &mut self.bypass_xfade,
                        &mut self.dry_delay,
                        &mut scratch,
                        &mut cab_stage,
                        input_clipped,
                        &mut self.smoother_in,
                        &mut self.smoother_out,
                        &mut self.smoother_cal_in,
                        &mut self.smoother_cal_out,
                        shared_sample_rate,
                        &gain_steps[..n_steps],
                        self.gate_mode,
                        &mut self.musical_gate,
                    )
                };

                if *input_clipped {
                    self.shared
                        .rt_to_ui
                        .ui_in_clipped
                        .store(true, Ordering::Relaxed);
                    self.shared
                        .rt_to_ui
                        .ui_in_clip_indicator
                        .store(true, Ordering::Relaxed);
                    self.shared
                        .rt_to_ui
                        .ui_clipped
                        .store(true, Ordering::Relaxed);
                    self.shared
                        .rt_to_ui
                        .ui_clip_indicator
                        .store(true, Ordering::Relaxed);
                }

                output_offset += sub_m.n_out;

                metrics.in_peak_l = metrics.in_peak_l.max(sub_m.in_peak_l);
                metrics.in_peak_r = metrics.in_peak_r.max(sub_m.in_peak_r);
                metrics.in_sum_sq_l += sub_m.in_sum_sq_l;
                metrics.in_sum_sq_r += sub_m.in_sum_sq_r;
                metrics.out_peak_l = metrics.out_peak_l.max(sub_m.out_peak_l);
                metrics.out_peak_r = metrics.out_peak_r.max(sub_m.out_peak_r);
                metrics.out_sum_sq_l += sub_m.out_sum_sq_l;
                metrics.out_sum_sq_r += sub_m.out_sum_sq_r;
                metrics.total_samples += sub_m.n_out;

                if sub_m.gate_state != GateState::Closed {
                    *last_gate_state = sub_m.gate_state;
                    *any_active = true;
                }
            }

            while *event_idx < event_count
                && self.scheduled_events[*event_idx].time == slice_base + sub_end
                && sub_end < n_samples
            {
                let evt = &self.scheduled_events[*event_idx];
                worker::apply_scheduled_event(
                    evt.param_id,
                    evt.value,
                    evt.is_mod,
                    &mut self.params,
                    &mut self.cab_bypass,
                    &mut self.cal_config,
                    &self.model_cal_meta,
                    &mut self.model_input_mult_adj,
                    &mut self.model_output_mult_adj,
                    &mut self.smoother_in,
                    &mut self.smoother_out,
                    &mut self.smoother_cal_in,
                    &mut self.smoother_cal_out,
                    &mut self.gate_dirty,
                    &mut self.gate_mode,
                    &mut self.musical_gate,
                    &mut self.mod_input_gain,
                    &mut self.mod_output_gain,
                    &mut self.mod_gate_thresh,
                    &mut self.adaptive_compute,
                    &self.rt_status,
                    &self.shared.ui_to_rt,
                    self.gain_lut,
                    self.shared.cold.buffer_size.load(Ordering::Relaxed),
                    &self.shared.cold.pending_restart_os_factor,
                    true,
                );
                *event_idx += 1;
            }

            self.refresh_gate_config();

            // Trigger bypass crossfade if the bypass state changed via
            // a host event at this sub-block boundary.
            self.bypass_xfade.trigger(self.params.bypass);
            self.cab_bypass_xfade.trigger(self.cab_bypass);

            block_offset = sub_end;
        }

        // Process remaining events with time >= slice_base + n_samples at the block close boundary.
        // Clamped defensively to slice_base + n_samples to guarantee zero silent event loss,
        // asserting RT_STATUS_EVENT_TIMING_ANOMALY.
        if is_last_slice {
            let mut remaining_applied = false;
            let end_abs = slice_base + n_samples;
            while *event_idx < event_count {
                core::hint::cold_path();
                self.rt_status.set_flag(RT_STATUS_EVENT_TIMING_ANOMALY);
                self.scheduled_events[*event_idx].time = end_abs;
                let evt = &self.scheduled_events[*event_idx];
                worker::apply_scheduled_event(
                    evt.param_id,
                    evt.value,
                    evt.is_mod,
                    &mut self.params,
                    &mut self.cab_bypass,
                    &mut self.cal_config,
                    &self.model_cal_meta,
                    &mut self.model_input_mult_adj,
                    &mut self.model_output_mult_adj,
                    &mut self.smoother_in,
                    &mut self.smoother_out,
                    &mut self.smoother_cal_in,
                    &mut self.smoother_cal_out,
                    &mut self.gate_dirty,
                    &mut self.gate_mode,
                    &mut self.musical_gate,
                    &mut self.mod_input_gain,
                    &mut self.mod_output_gain,
                    &mut self.mod_gate_thresh,
                    &mut self.adaptive_compute,
                    &self.rt_status,
                    &self.shared.ui_to_rt,
                    self.gain_lut,
                    self.shared.cold.buffer_size.load(Ordering::Relaxed),
                    &self.shared.cold.pending_restart_os_factor,
                    true,
                );
                *event_idx += 1;
                remaining_applied = true;
            }

            if remaining_applied {
                self.refresh_gate_config();
                self.bypass_xfade.trigger(self.params.bypass);
                self.cab_bypass_xfade.trigger(self.cab_bypass);
            }
        }
    }

    /// Parses the host input events of one callback into the scheduled-event
    /// list, shared verbatim by the wet pipeline (`process_dsp_audio`) and
    /// the two-phase reset containment leg
    /// ([`Self::process_dry_contained_block`]) so both legs apply the exact
    /// same event set — zero event loss in either path.
    #[inline(always)]
    fn schedule_input_events(&mut self, input_events: &InputEvents) {
        let mut sorted = true;
        let mut last_time = 0usize;
        {
            let events = &mut self.scheduled_events;
            events.clear();

            for event in input_events {
                if events.len() >= MAX_SCHEDULED_EVENTS {
                    core::hint::cold_path();
                    // Explicit saturation: the flag is the observable signal
                    // (logged by emit_pending_logs off-RT). Never suppress it.
                    self.rt_status.set_flag(
                        neural_amp_modeler_rs::common::spsc::RT_STATUS_SPSC_DRAIN_TRUNCATED,
                    );
                    debug_assert!(
                        false,
                        "Event flood > {MAX_SCHEDULED_EVENTS} in one block; truncating"
                    );
                    break;
                }
                let time = event.header().time();
                if let Some(param_event) = event.as_event::<ParamValueEvent>() {
                    let Some(clap_id) = param_event.param_id() else {
                        core::hint::cold_path();
                        continue;
                    };
                    let t = time as usize;
                    sorted &= t >= last_time;
                    last_time = t;
                    events.push(ScheduledEvent {
                        time: t,
                        param_id: clap_id.get(),
                        value: param_event.value() as f32,
                        is_mod: false,
                    });
                } else if let Some(mod_event) = event.as_event::<ParamModEvent>() {
                    let Some(clap_id) = mod_event.param_id() else {
                        core::hint::cold_path();
                        continue;
                    };
                    let t = time as usize;
                    sorted &= t >= last_time;
                    last_time = t;
                    events.push(ScheduledEvent {
                        time: t,
                        param_id: clap_id.get(),
                        value: mod_event.amount() as f32,
                        is_mod: true,
                    });
                } else {
                    // Unknown/unsupported event type for this plugin.
                    core::hint::cold_path();
                }
            }
        }
        self.scheduled_events_sorted = sorted;
    }

    /// Containment leg of the two-phase reset amortization window: renders
    /// one callback while the split-stabilization pass armed by the
    /// phase-zero `reset()` is still outstanding, so the non-converged model
    /// can never leak wet audio into the mix.
    ///
    /// The block is held out of the DSP pipeline with bypass-leg semantics:
    /// the staged host input feeds the latency-compensated dry delay
    /// (preserving the ring history the wet path rejoins on the first
    /// post-convergence block) and the delayed dry is published as the
    /// output, while the bypass crossfader trigger, the gate/adaptive FSMs
    /// and the smoothers stay untouched — host audio neither reaches the
    /// model nor advances any temporal state, so the first wet block after
    /// convergence equals a fresh instance's first block.
    ///
    /// Host parameter events keep applying (zero event loss): every
    /// scheduled event is applied in delivery order as the callback's
    /// end-of-block parameter state, mirroring the wet pipeline's
    /// defensive-fallback semantics. Intra-block sample accuracy is not
    /// observable because the block's audio bypasses the pipeline. The
    /// bypass crossfader trigger is deliberately not fired: a crossfade
    /// ramping mid-window would blend non-converged wet audio; the first
    /// wet block re-triggers from the held crossfader state instead.
    ///
    /// Telemetry covers the window: the callback is measured like any other
    /// block, so the drain cost lands in the deadline histogram and the
    /// adaptive FSM.
    #[cold]
    // Standalone audit unit for `verify_rt_codegen.sh` (the gate watches the
    // symbol; inlining into `process()` would hide the leg's .text).
    #[inline(never)]
    pub(crate) fn process_dry_contained_block(
        &mut self,
        audio: &mut Audio,
        input_events: &InputEvents,
        start_nanos: u64,
    ) -> Result<ProcessStatus, PluginError> {
        let pending_before = PendingRestartOs::load(
            &self.shared.cold.pending_restart_os_factor,
            Ordering::Relaxed,
        );
        self.schedule_input_events(input_events);

        // Apply every scheduled event in delivery order (the wet path's
        // defensive fallback applies leftovers the same way, in ring order
        // with clamped times). The bypass crossfader trigger is held (see
        // the doc comment above).
        for evt in &self.scheduled_events {
            worker::apply_scheduled_event(
                evt.param_id,
                evt.value,
                evt.is_mod,
                &mut self.params,
                &mut self.cab_bypass,
                &mut self.cal_config,
                &self.model_cal_meta,
                &mut self.model_input_mult_adj,
                &mut self.model_output_mult_adj,
                &mut self.smoother_in,
                &mut self.smoother_out,
                &mut self.smoother_cal_in,
                &mut self.smoother_cal_out,
                &mut self.gate_dirty,
                &mut self.gate_mode,
                &mut self.musical_gate,
                &mut self.mod_input_gain,
                &mut self.mod_output_gain,
                &mut self.mod_gate_thresh,
                &mut self.adaptive_compute,
                &self.rt_status,
                &self.shared.ui_to_rt,
                self.gain_lut,
                self.shared.cold.buffer_size.load(Ordering::Relaxed),
                &self.shared.cold.pending_restart_os_factor,
                true,
            );
        }

        // Parameter applications can invalidate the gate coefficients; keep
        // the cached gate parameters current so the first wet block starts
        // with the same gate math the wet pipeline would have.
        self.refresh_gate_config();

        let mut metrics = BlockMetricsAccumulator::default();
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
                    let mut out_l = if port_output_active {
                        channels::get_channel_output(pair_l.as_mut(), slice_start, slice_len)
                    } else {
                        None
                    };
                    let mut out_r = if port_output_active {
                        channels::get_channel_output(pair_r.as_mut(), slice_start, slice_len)
                    } else {
                        None
                    };

                    self.dry_delay.process_block(
                        &self.buf_host_l[..slice_len],
                        &self.buf_host_r[..slice_len],
                        &mut self.buf_xfade_dry_l[..slice_len],
                        &mut self.buf_xfade_dry_r[..slice_len],
                        slice_len,
                        self.process_mono,
                    );
                    let (in_peak_l, in_sum_sq_l) =
                        audio_loop::compute_peak_and_sum_sq(&self.buf_host_l[..slice_len]);
                    let (in_peak_r, in_sum_sq_r) = if self.process_mono {
                        (in_peak_l, in_sum_sq_l)
                    } else {
                        audio_loop::compute_peak_and_sum_sq(&self.buf_host_r[..slice_len])
                    };
                    let (sub_peak_l, sub_peak_r, sub_sum_sq_l, sub_sum_sq_r) =
                        audio_loop::copy_delayed_dry_to_output(
                            &mut out_l,
                            &mut out_r,
                            &mut self.buf_xfade_dry_l[..slice_len],
                            &mut self.buf_xfade_dry_r[..slice_len],
                            0,
                            self.process_mono,
                        );
                    metrics.in_peak_l = metrics.in_peak_l.max(in_peak_l);
                    metrics.in_peak_r = metrics.in_peak_r.max(in_peak_r);
                    metrics.in_sum_sq_l += in_sum_sq_l;
                    metrics.in_sum_sq_r += in_sum_sq_r;
                    metrics.out_peak_l = metrics.out_peak_l.max(sub_peak_l);
                    metrics.out_peak_r = metrics.out_peak_r.max(sub_peak_r);
                    metrics.out_sum_sq_l += sub_sum_sq_l;
                    metrics.out_sum_sq_r += sub_sum_sq_r;
                    metrics.total_samples += slice_len;

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
            let (mut out_l, mut out_r) = if port_output_active {
                (out_l, out_r)
            } else {
                (None, None)
            };

            // Dry leg: feed the ring with the staged host input and publish
            // the delayed dry (single fused sanitize/peak pass). The ring
            // keeps absorbing the host input so its latency history stays
            // intact for the wet path's return.
            self.dry_delay.process_block(
                &self.buf_host_l[..n_samples],
                &self.buf_host_r[..n_samples],
                &mut self.buf_xfade_dry_l[..n_samples],
                &mut self.buf_xfade_dry_r[..n_samples],
                n_samples,
                self.process_mono,
            );
            let (in_peak_l, in_sum_sq_l) =
                audio_loop::compute_peak_and_sum_sq(&self.buf_host_l[..n_samples]);
            let (in_peak_r, in_sum_sq_r) = if self.process_mono {
                (in_peak_l, in_sum_sq_l)
            } else {
                audio_loop::compute_peak_and_sum_sq(&self.buf_host_r[..n_samples])
            };
            let (sub_peak_l, sub_peak_r, sub_sum_sq_l, sub_sum_sq_r) =
                audio_loop::copy_delayed_dry_to_output(
                    &mut out_l,
                    &mut out_r,
                    &mut self.buf_xfade_dry_l[..n_samples],
                    &mut self.buf_xfade_dry_r[..n_samples],
                    0,
                    self.process_mono,
                );
            metrics.in_peak_l = metrics.in_peak_l.max(in_peak_l);
            metrics.in_peak_r = metrics.in_peak_r.max(in_peak_r);
            metrics.in_sum_sq_l += in_sum_sq_l;
            metrics.in_sum_sq_r += in_sum_sq_r;
            metrics.out_peak_l = metrics.out_peak_l.max(sub_peak_l);
            metrics.out_peak_r = metrics.out_peak_r.max(sub_peak_r);
            metrics.out_sum_sq_l += sub_sum_sq_l;
            metrics.out_sum_sq_r += sub_sum_sq_r;
            metrics.total_samples += n_samples;
        }

        // Bypass-leg telemetry projection: gate reported open, no FSM
        // advance, peaks from the delayed dry.
        if self.gate_mode == GateMode::Musical {
            self.musical_gate.reset();
        }
        self.shared
            .rt_to_ui
            .ui_gate_active
            .store(false, Ordering::Relaxed);
        gate_flags::report_gate_flags(&self.rt_status, GateState::Open);
        self.update_rms_and_telemetry(
            &metrics,
            self.shared.cold.sample_rate.load(Ordering::Relaxed),
        );

        let pending_after = PendingRestartOs::load(
            &self.shared.cold.pending_restart_os_factor,
            Ordering::Relaxed,
        );
        if pending_after != pending_before && pending_after != PendingRestartOs::None {
            self.host.request_restart();
        }

        self.process_telemetry(start_nanos);

        Ok(ProcessStatus::Continue)
    }

    #[inline]
    fn report_gate_telemetry(&self, any_active: bool, last_gate_state: GateState) {
        match self.gate_mode {
            GateMode::Silence => {
                let is_gate_active = any_active && last_gate_state == GateState::Closed;
                self.shared
                    .rt_to_ui
                    .ui_gate_active
                    .store(is_gate_active, Ordering::Relaxed);

                if any_active {
                    gate_flags::report_gate_flags(&self.rt_status, last_gate_state);
                } else {
                    gate_flags::report_gate_flags(&self.rt_status, GateState::Closed);
                }
            }
            GateMode::Musical => {
                let attenuating =
                    !self.params.bypass && self.musical_gate.is_attenuating(self.process_mono);
                self.shared
                    .rt_to_ui
                    .ui_gate_active
                    .store(attenuating, Ordering::Relaxed);
                gate_flags::report_gate_flags(
                    &self.rt_status,
                    if attenuating {
                        GateState::Closed
                    } else {
                        GateState::Open
                    },
                );
            }
        }
    }
}

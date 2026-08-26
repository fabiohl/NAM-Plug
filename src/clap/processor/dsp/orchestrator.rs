// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

pub mod audio_loop;
pub mod worker;

pub(crate) use worker::ScheduledEvent;

use super::super::NamClapProcessor;
use crate::clap::plugin::PendingRestartOs;
use crate::clap::processor::dsp::{channels, peaks};
use clack_plugin::events::event_types::{ParamModEvent, ParamValueEvent};
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_HOST_CONTRACT_VIOLATION, RT_STATUS_NON_FINITE_INPUT_DETECTED,
};
use neural_amp_modeler_rs::dsp::gate::GateState;
use neural_amp_modeler_rs::dsp::gate_flags;
use neural_amp_modeler_rs::dsp::pipeline::DspPipelineContext;
use neural_amp_modeler_rs::models::NamModel;
use std::sync::atomic::Ordering;

/// Maximum number of host input events scheduled within a single block.
///
/// Saturation past this budget is **explicit, not invisible**: the scheduler
/// stops draining, raises `RT_STATUS_SPSC_DRAIN_TRUNCATED` (kept, see T6.3),
/// and the main thread logs the overflow off-RT via `emit_pending_logs()`.
/// The remaining events of the block are rejected — the plugin never silently
/// drops a partial automation envelope in release without a flag.
const MAX_SCHEDULED_EVENTS: usize = 4096;

impl<'a> NamClapProcessor<'a> {
    #[inline(always)]
    pub(crate) fn process_dsp_audio(
        &mut self,
        audio: &mut Audio,
        input_events: &InputEvents,
        start_nanos: u64,
    ) -> Result<ProcessStatus, PluginError> {
        // Track pending restart factor for latency-policy enforcement.
        // T3.1/F-LAT-004: decode via `PendingRestartOs` so a pending
        // transition *to Off* is observable (raw 0 == "no pending").
        let pending_before = PendingRestartOs::load(
            &self.shared.cold.pending_restart_os_factor,
            Ordering::Relaxed,
        );
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
                        "CLAP-F007: event flood > {MAX_SCHEDULED_EVENTS} in one block; truncating"
                    );
                    break;
                }
                let time = event.header().time();
                if let Some(param_event) = event.as_event::<ParamValueEvent>() {
                    let Some(clap_id) = param_event.param_id() else {
                        core::hint::cold_path();
                        continue;
                    };
                    events.push(ScheduledEvent {
                        time: time as usize,
                        param_id: clap_id.get(),
                        value: param_event.value() as f32,
                        is_mod: false,
                    });
                } else if let Some(mod_event) = event.as_event::<ParamModEvent>() {
                    let Some(clap_id) = mod_event.param_id() else {
                        core::hint::cold_path();
                        continue;
                    };
                    events.push(ScheduledEvent {
                        time: time as usize,
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

        let event_count = self.scheduled_events.len();
        let mut event_idx = 0;

        for mut port_pair in audio {
            let n_samples_raw = port_pair.frames_count() as usize;
            if n_samples_raw > self.max_frames_count {
                self.rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
                if let Ok(channels) = port_pair.channels()
                    && let Some(pairs) = channels.into_f32()
                {
                    for pair in pairs {
                        match pair {
                            ChannelPair::InputOutput(_, o)
                            | ChannelPair::OutputOnly(o)
                            | ChannelPair::InPlace(o) => {
                                let len = o.len().min(n_samples_raw);
                                o[..len].fill(0.0);
                            }
                            ChannelPair::InputOnly(_) => {}
                        }
                    }
                }
                return Err(PluginError::Message(
                    "Host block size exceeds maximum configured capacity",
                ));
            }
            let n_samples = n_samples_raw.min(self.max_frames_count);
            if n_samples == 0 {
                continue;
            }
            let n = n_samples as u32;
            if self.rt_status.last_n_samples.load(Ordering::Relaxed) != n {
                self.rt_status.last_n_samples.store(n, Ordering::Relaxed);
            }

            let Some((mut out_l, mut out_r)) = channels::extract_channels(
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

            // Non-finite input sample detection & containment (T2.3)
            let mut non_finite = false;
            for &s in &self.buf_host_l[..n_samples] {
                if !s.is_finite() {
                    non_finite = true;
                    break;
                }
            }
            #[cfg(feature = "stereo")]
            if !non_finite && !self.process_mono {
                for &s in &self.buf_host_r[..n_samples] {
                    if !s.is_finite() {
                        non_finite = true;
                        break;
                    }
                }
            }

            if non_finite {
                self.rt_status.set_flag(RT_STATUS_NON_FINITE_INPUT_DETECTED);
                self.buf_host_l[..n_samples].fill(0.0);
                self.buf_host_r[..n_samples].fill(0.0);
                if let Some(model) = &mut self.model_l {
                    // Reset the model at the effective rate of the active chain
                    // (post-resample model rate), never a hard-coded 48 kHz —
                    // models may be native 44.1/48 kHz and the host may run at
                    // 44.1/48/96 kHz (F-ROB-PLUG-04 / T2.3 residual).
                    let _ = model.reset(self.resampler.nam_rate(), n_samples);
                }
                self.buf_mid_l.fill(0.0);
                self.buf_mid_r.fill(0.0);
                self.buf_model_l.fill(0.0);
                self.buf_model_r.fill(0.0);
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
                // T4.1/F-DSP-008: the dry delay line must not replay pre-fault
                // history (which would leak the pre-containment audio through
                // the bypass/crossfade path); reset it to a zeroed ring.
                self.dry_delay.reset();
                self.smoother_in.snap_to_target();
                self.smoother_out.snap_to_target();
            };

            if self
                .shared
                .ui_to_rt
                .host_r_deactivated
                .load(Ordering::Acquire)
            {
                self.process_mono = true;
            }

            if self.gate_dirty {
                let modulated_gate_db = self.params.gate_threshold_db + self.mod_gate_thresh;
                let close_db = modulated_gate_db - 6.0;
                let open_linear = self.gain_lut.db_to_linear(modulated_gate_db);
                self.cached_threshold_open_sq = open_linear * open_linear;
                let close_linear = self.gain_lut.db_to_linear(close_db);
                self.cached_threshold_close_sq = close_linear * close_linear;
                self.cached_gate_params.threshold_open_db = modulated_gate_db;
                self.cached_gate_params.threshold_close_db = close_db;
                self.gate_dirty = false;
            }

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

            let mut block_offset = 0usize;
            let mut output_offset = 0usize;
            let mut peak_l = 0.0f32;
            let mut peak_r = 0.0f32;
            let mut last_gate_state = GateState::Open;
            let mut any_active = false;
            let model_output_mult_adj = self.model_output_mult_adj;
            let shared_sample_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);

            let mut input_clipped = false;

            // Sync crossfader with current bypass state. If bypass was changed
            // by SPSC event sync (process_events) before this block, trigger
            // the crossfade to avoid click artifacts.
            self.bypass_xfade.trigger(self.params.bypass);

            while block_offset < n_samples {
                while event_idx < event_count
                    && self.scheduled_events[event_idx].time < block_offset
                {
                    // Out-of-order timestamp (already passed this block offset).
                    core::hint::cold_path();
                    event_idx += 1;
                }

                let sub_end = if event_idx < event_count {
                    let et = self.scheduled_events[event_idx].time;
                    if et < n_samples { et } else { n_samples }
                } else {
                    n_samples
                };

                let sub_n = sub_end - block_offset;
                if sub_n > 0 {
                    let bypass = self.params.bypass;
                    let process_mono = self.process_mono;

                    let (n_out, gate_state) = {
                        let mut ctx = DspPipelineContext {
                            resampler: &mut self.resampler,
                            os_l: &mut self.os_l,
                            os_r: &mut self.os_r,
                            active_model_l: &mut self.model_l,
                            active_model_r: &mut None,
                            input_gain_mult: self.model_input_mult_adj,
                            output_gain_mult: model_output_mult_adj,
                            gate_params: &self.cached_gate_params,
                            silence_hysteresis: &mut self.silence_hyst,
                            mono_hysteresis: &mut self.mono_hyst,
                            threshold_open_sq: self.cached_threshold_open_sq,
                            threshold_close_sq: self.cached_threshold_close_sq,
                            process_mono: &mut self.process_mono,
                            rt_status: &self.rt_status,
                            adaptive: &mut self.adaptive_compute,
                            bridge_writer: None,
                            conv: self.cabsim_adapter.as_deref_mut(),
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
                            &mut self.buf_xfade_dry_l,
                            &mut self.buf_xfade_dry_r,
                            &mut self.buf_xfd_scratch_l,
                            &mut self.buf_xfd_scratch_r,
                            &mut input_clipped,
                            &mut self.smoother_in,
                            &mut self.smoother_out,
                            &mut self.buf_host_l,
                            &mut self.buf_host_r,
                            &mut self.buf_mid_l,
                            &mut self.buf_mid_r,
                            &mut self.buf_out_l,
                            &mut self.buf_out_r,
                            &mut self.buf_model_l,
                            &mut self.buf_model_r,
                            &mut self.buf_os_in_l,
                            &mut self.buf_os_in_r,
                            &mut self.buf_os_model_l,
                            &mut self.buf_os_model_r,
                            model_output_mult_adj,
                            shared_sample_rate,
                            self.gain_lut,
                            &mut self.cabsim_tail_remaining,
                        )
                    };

                    if input_clipped {
                        self.shared
                            .rt_to_ui
                            .ui_clipped
                            .store(true, Ordering::Relaxed);
                    }

                    output_offset += n_out;

                    if let (Some(o_l), Some(o_r)) = (&out_l, &out_r) {
                        let o_start = output_offset - n_out;
                        let o_end = o_start + n_out;
                        let avail_l = o_l.len().min(o_end).saturating_sub(o_start);
                        let avail_r = o_r.len().min(o_end).saturating_sub(o_start);
                        let n = avail_l.min(avail_r);
                        if n > 0 {
                            let (pl, pr) = unsafe {
                                neural_amp_modeler_rs::math::dsp::stereo::compute_peak_abs_stereo(
                                    &o_l[o_start..o_start + n],
                                    &o_r[o_start..o_start + n],
                                )
                            };
                            peak_l = peak_l.max(pl);
                            peak_r = peak_r.max(pr);
                        }
                    } else if let Some(o_l) = &out_l {
                        let o_start = output_offset - n_out;
                        let o_end = o_start + n_out;
                        let avail = o_l.len().min(o_end).saturating_sub(o_start);
                        if avail > 0 {
                            let (pl, _) = unsafe {
                                neural_amp_modeler_rs::math::dsp::stereo::compute_peak_abs_stereo(
                                    &o_l[o_start..o_start + avail],
                                    &o_l[o_start..o_start + avail],
                                )
                            };
                            peak_l = peak_l.max(pl);
                            peak_r = peak_r.max(pl);
                        }
                    }

                    if gate_state != GateState::Closed {
                        last_gate_state = gate_state;
                        any_active = true;
                    }
                }

                while event_idx < event_count && self.scheduled_events[event_idx].time == sub_end {
                    let evt = &self.scheduled_events[event_idx];
                    worker::apply_scheduled_event(
                        evt.param_id,
                        evt.value,
                        evt.is_mod,
                        &mut self.params,
                        &mut self.smoother_in,
                        &mut self.smoother_out,
                        &mut self.gate_dirty,
                        &mut self.mod_input_gain,
                        &mut self.mod_output_gain,
                        &mut self.mod_gate_thresh,
                        &mut self.adaptive_compute,
                        &self.rt_status,
                        &self.shared.ui_to_rt,
                        self.gain_lut,
                        self.shared.cold.buffer_size.load(Ordering::Relaxed),
                        &self.shared.cold.pending_restart_os_factor,
                    );
                    event_idx += 1;
                }

                if self.gate_dirty {
                    let modulated_gate_db = self.params.gate_threshold_db + self.mod_gate_thresh;
                    let close_db = modulated_gate_db - 6.0;
                    let open_linear = self.gain_lut.db_to_linear(modulated_gate_db);
                    self.cached_threshold_open_sq = open_linear * open_linear;
                    let close_linear = self.gain_lut.db_to_linear(close_db);
                    self.cached_threshold_close_sq = close_linear * close_linear;
                    self.cached_gate_params.threshold_open_db = modulated_gate_db;
                    self.cached_gate_params.threshold_close_db = close_db;
                    self.gate_dirty = false;
                }

                // Trigger bypass crossfade if the bypass state changed via
                // a host event at this sub-block boundary.
                self.bypass_xfade.trigger(self.params.bypass);

                block_offset = sub_end;
            }

            if any_active {
                gate_flags::report_gate_flags(&self.rt_status, last_gate_state);
            } else {
                gate_flags::report_gate_flags(&self.rt_status, GateState::Closed);
            }

            peaks::store_peaks(self.shared, peak_l, peak_r);
        }

        // If an oversampling change was detected during active
        // processing, request host restart so latency can be updated
        // legally during the next activate(). A pending *Off* target is
        // representable and also triggers the restart (T3.1/F-LAT-004).
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
}

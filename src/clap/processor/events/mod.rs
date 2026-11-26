// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Event draining: SPSC (Main Thread → Audio Thread), host events,
//! GUI parameter sync and latency monitoring.

mod command_handler;
mod slimmable_handler;

pub(crate) use command_handler::ClapCommandSwapHandler;
pub(crate) use slimmable_handler::SlimmableSwapHandler;

use super::{CMD_SWAP_TUNABLES, NamClapProcessor, SLIMMABLE_SWAP_TUNABLES};
use clack_plugin::prelude::OutputEvents;
use neural_amp_modeler_rs::common::params::AdaptiveComputeMode;
use neural_amp_modeler_rs::common::spsc::{RtSwapHandler, SwapBudget};
use std::sync::atomic::Ordering;

impl<'a> NamClapProcessor<'a> {
    /// Processes SPSC payloads, GUI parameter sync, latency, and render mode.
    pub(super) fn process_events(&mut self, output: &mut OutputEvents) {
        self.shared.write_gui_events(output);

        let mut budget = SwapBudget::new(CMD_SWAP_TUNABLES.swaps_per_callback);

        #[cfg(all(test, target_arch = "x86_64", feature = "heap-audit"))]
        let drain_probe_t0 = neural_amp_modeler_rs::common::tsc::rdtsc_nanos();

        let replay_pending = self.replay_recovered_commands(&mut budget);

        self.drain_parking_lot();
        self.drain_box_parking_lot();

        if !replay_pending {
            let (mut handler, mut gc) = command_swap_parts!(self);
            self.cmd_drain.drain(&mut handler, &mut budget, &mut gc);
        }
        {
            let (mut handler, mut gc) = slimmable_swap_parts!(self);
            debug_assert_eq!(
                SLIMMABLE_SWAP_TUNABLES.swaps_per_callback,
                CMD_SWAP_TUNABLES.swaps_per_callback
            );
            self.slimmable_drain
                .drain(&mut handler, &mut budget, &mut gc);
        }
        #[cfg(all(test, target_arch = "x86_64", feature = "heap-audit"))]
        super::DRAIN_PROBE_NS.store(
            neural_amp_modeler_rs::common::tsc::rdtsc_nanos().wrapping_sub(drain_probe_t0),
            Ordering::Relaxed,
        );

        let generation = self
            .shared
            .ui_to_rt
            .gui_param_generation
            .load(Ordering::Acquire);
        if generation != self.last_seen_generation {
            self.sync_params_from_gui();
            self.last_seen_generation = generation;
        }

        let effective_latency = self.cached_effective_latency;
        if effective_latency != self.shared.rt_to_ui.current_latency.load(Ordering::Relaxed) {
            self.shared
                .rt_to_ui
                .current_latency
                .store(effective_latency, Ordering::Relaxed);
        }

        self.check_tail_changed();

        let render_mode = self.shared.cold.render_mode.load(Ordering::Acquire);
        if render_mode != self.last_render_mode {
            self.last_render_mode = render_mode;
            if render_mode == crate::clap::plugin::RENDER_MODE_OFFLINE {
                self.adaptive_compute
                    .set_mode(AdaptiveComputeMode::Off, &self.rt_status);
            } else {
                let user_mode = AdaptiveComputeMode::from_f32(
                    self.shared
                        .ui_to_rt
                        .param_adaptive_compute
                        .load(Ordering::Relaxed) as f32,
                );
                self.adaptive_compute.set_mode(user_mode, &self.rt_status);
            }
            // Offline rendering is always Standard; the user choice lives in
            // `realtime_activation` and is restored when the mode returns.
            let effective =
                crate::clap::processor::effective_activation(render_mode, self.realtime_activation);
            self.params.activation_precision = effective;
            neural_amp_modeler_rs::math::activations::set_activation_tls(effective);
        }
        if render_mode == crate::clap::plugin::RENDER_MODE_OFFLINE
            && self.adaptive_compute.mode() != AdaptiveComputeMode::Off
        {
            self.adaptive_compute
                .set_mode(AdaptiveComputeMode::Off, &self.rt_status);
        }

        self.signal_slimmable_rebuild();
    }

    #[cold]
    fn replay_recovered_commands(&mut self, budget: &mut SwapBudget) -> bool {
        if self.recovered_commands.is_empty() {
            return false;
        }
        let (mut handler, mut gc) = command_swap_parts!(self);
        let mut applied = 0usize;
        while let Some(payload) = self.recovered_commands.front() {
            if applied >= CMD_SWAP_TUNABLES.pops_per_callback {
                return true;
            }
            if handler.is_structural(payload) {
                if !budget.can_apply() {
                    return true;
                }
                budget.consume();
            }
            let Some(payload) = self.recovered_commands.pop_front() else {
                break;
            };
            applied += 1;
            handler.install(payload, &mut gc);
        }
        false
    }

    fn signal_slimmable_rebuild(&mut self) {
        let Some(target_ch) = self.adaptive_compute.take_slimmable_ch_rebuild() else {
            return;
        };
        self.shared.cold.publish_slimmable_request(
            target_ch as u32,
            self.model_generation,
            self.model_r.is_some(),
        );
    }

    /// Checks if the plugin's tail length changed (pipeline latency + CabSim tail),
    /// and notifies the host on the audio thread if changed.
    #[inline(always)]
    pub(crate) fn check_tail_changed(&mut self) {
        let base = self.cached_effective_latency;
        let cabsim_tail = self
            .shared
            .rt_to_ui
            .cabsim_tail_samples
            .load(Ordering::Relaxed);
        let current_tail = base.saturating_add(cabsim_tail);
        let last_reported = self
            .shared
            .cold
            .last_reported_tail_samples
            .load(Ordering::Relaxed);
        if current_tail != last_reported {
            self.shared
                .cold
                .last_reported_tail_samples
                .store(current_tail, Ordering::Relaxed);
            if let Some(tail_ext) = self.host_tail {
                tail_ext.changed(&mut self.host);
            }
        }
    }
}

macro_rules! command_swap_parts {
    ($s:ident) => {{
        (
            $crate::clap::processor::events::ClapCommandSwapHandler {
                model_l: &mut $s.model_l,
                model_r: &mut $s.model_r,
                model_generation: &mut $s.model_generation,
                resampler: &mut $s.resampler,
                stream: &mut $s.stream,
                os_l: &mut $s.os_l,
                os_r: &mut $s.os_r,
                cab_slot: &mut $s.cab_slot,
                cab_bypass: &mut $s.cab_bypass,
                cab_needs_reset: &mut $s.cab_needs_reset,
                cab_nam_dirty: &mut $s.cab_nam_dirty,
                cab_nam_generation: &mut $s.cab_nam_generation,
                cab_nam_rebuild_in_flight: &mut $s.cab_nam_rebuild_in_flight,
                cab_bypass_xfade: &mut $s.cab_bypass_xfade,
                cab_dry_delay: &mut $s.cab_dry_delay,
                max_frames_count: $s.max_frames_count,
                params: &mut $s.params,
                realtime_activation: &mut $s.realtime_activation,
                applied_os_factor: &mut $s.applied_os_factor,
                cal_config: &mut $s.cal_config,
                model_cal_meta: &mut $s.model_cal_meta,
                model_input_mult_adj: &mut $s.model_input_mult_adj,
                model_output_mult_adj: &mut $s.model_output_mult_adj,
                adaptive_compute: &mut $s.adaptive_compute,
                cached_effective_latency: &mut $s.cached_effective_latency,
                dry_delay: &mut $s.dry_delay,
                smoother_in: &mut $s.smoother_in,
                smoother_out: &mut $s.smoother_out,
                smoother_cal_in: &mut $s.smoother_cal_in,
                smoother_cal_out: &mut $s.smoother_cal_out,
                mod_input_gain: &mut $s.mod_input_gain,
                mod_output_gain: &mut $s.mod_output_gain,
                gate_dirty: &mut $s.gate_dirty,
                gate_mode: &mut $s.gate_mode,
                musical_gate: &mut $s.musical_gate,
                silence_hyst: &mut $s.silence_hyst,
                prewarm_pending: &mut $s.prewarm_pending,
                prewarm_cost_ns_per_sample: &mut $s.prewarm_cost_ns_per_sample,
                prewarm_zero_budget_count: &mut $s.prewarm_zero_budget_count,
                shared: $s.shared,
                rt_status: &$s.rt_status,
                gain_lut: $s.gain_lut,
                host: &mut $s.host,
                host_tail: $s.host_tail,
                box_gc: $crate::clap::plugin::retired_box::RetiredBoxSink {
                    producer: &mut $s.box_gc_tx,
                    parking_lot: &mut $s.box_parking_lot,
                    parking_lot_dirty: &$s.box_parking_lot_dirty,
                    overflow: &$s.shared.cold.box_overflow,
                },
                resolved: 0,
            },
            neural_amp_modeler_rs::common::spsc::GcSink {
                producer: &mut $s.gc_tx,
                parking_lot: &mut $s.parking_lot,
                overflow: &$s.gc_overflow,
                rt_status: &$s.rt_status,
                parking_lot_dirty: Some(&$s.parking_lot_dirty),
            },
        )
    }};
}
pub(crate) use command_swap_parts;

macro_rules! slimmable_swap_parts {
    ($s:ident) => {{
        (
            $crate::clap::processor::events::SlimmableSwapHandler {
                model_l: &mut $s.model_l,
                model_r: &mut $s.model_r,
                model_generation: &$s.model_generation,
                shared: $s.shared,
                box_gc: $crate::clap::plugin::retired_box::RetiredBoxSink {
                    producer: &mut $s.box_gc_tx,
                    parking_lot: &mut $s.box_parking_lot,
                    parking_lot_dirty: &$s.box_parking_lot_dirty,
                    overflow: &$s.shared.cold.box_overflow,
                },
            },
            neural_amp_modeler_rs::common::spsc::GcSink {
                producer: &mut $s.gc_tx,
                parking_lot: &mut $s.parking_lot,
                overflow: &$s.gc_overflow,
                rt_status: &$s.rt_status,
                parking_lot_dirty: Some(&$s.parking_lot_dirty),
            },
        )
    }};
}
pub(crate) use slimmable_swap_parts;

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Event draining: SPSC (Main Thread → Audio Thread), host events,
//! GUI parameter sync and latency monitoring.

use super::{MAX_STRUCTURAL_COMMANDS_PER_CALLBACK, NamClapProcessor};
use crate::clap::plugin::{ClapParamPayload, StructuralKind};
use clack_extensions::tail::HostTail;
use clack_plugin::prelude::OutputEvents;
use neural_amp_modeler_rs::common::spsc::GcItem;
use neural_amp_modeler_rs::models::StaticModel;
use std::sync::atomic::Ordering;

impl<'a> NamClapProcessor<'a> {
    /// Processes SPSC payloads, GUI parameter sync, latency, and render mode.
    /// Host parameter events are handled later via block-splitting in
    /// `process_dsp_audio`.
    pub(super) fn process_events(&mut self, output: &mut OutputEvents) {
        self.shared.write_gui_events(output);

        // 0. Drain parking lot: re-try items parked during previous swaps
        //    when the GC SPSC channel was full.
        self.drain_parking_lot();

        // 1. Event Processing (Main Thread SPSC)
        // Command Budgeting (T2.3 / F-RT-007):
        // - Light parameter updates (Params) drain freely up to the queue cap.
        // - Structural commands (model/IR/oversample swaps, full restores) are
        //   budgeted to at most MAX_STRUCTURAL_COMMANDS_PER_CALLBACK per
        //   callback. A structural apply recomputes latency, feeds the GC
        //   cascade and may call host extensions (`HostTail::changed`), so a
        //   burst of 64 structural payloads must not all execute in one
        //   callback — the excess is deferred (parked) and the drain stops,
        //   preserving FIFO ordering and composite-transaction atomicity.
        let mut drained_count = 0u32;
        let mut structural_applied = 0u32;
        let mut processed_any = false;

        // Phase 0 — resolve a structural command deferred by the previous
        // callback. It is causally *before* everything still in the ring, so it
        // applies first. Command coalescing (latest-wins): if the ring head is
        // a newer same-kind coalescible command, the deferred one is superseded
        // — never applied, its resources discarded off-RT via the GC cascade.
        if let Some(deferred) = self.deferred_structural.take() {
            let deferred_kind = deferred.structural_kind();
            let superseded = deferred_kind.is_some_and(StructuralKind::is_coalescible)
                && self
                    .cmd_consumer
                    .peek()
                    .and_then(ClapParamPayload::structural_kind)
                    == deferred_kind;
            if superseded {
                self.discard_structural_payload(deferred);
                self.rt_status
                    .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_STRUCTURAL_SUPERSEDED);
                self.rt_status
                    .structural_superseded_total
                    .fetch_add(1, Ordering::Relaxed);
                // The superseded command's sequence slot is now consumed by the
                // discard; the superseding ring head reoccupies the next slot
                // when popped below. Consume the slot so the ack stays gapless.
                self.cmd_consumer.advance_pending();
                // structural_applied stays 0: the superseding command is
                // drained below and consumes this callback's structural slot.
            } else {
                self.apply_structural(deferred);
                self.cmd_consumer.advance_pending();
                structural_applied = 1;
                processed_any = true;
            }
        }

        // Phase 1 — drain the ring under the structural budget.
        while let Some(payload) = self.cmd_consumer.pop() {
            drained_count += 1;
            processed_any = true;
            if payload.is_structural() && structural_applied >= MAX_STRUCTURAL_COMMANDS_PER_CALLBACK
            {
                // Budget exhausted: park the command and stop draining.
                // Everything still in the ring is causally after it (FIFO), so
                // stopping preserves order; the parked command is applied (or
                // superseded by a newer same-kind head) at the next callback.
                debug_assert!(
                    self.deferred_structural.is_none(),
                    "deferred slot must be free before parking a new structural command"
                );
                self.deferred_structural = Some(payload);
                self.cmd_consumer.rollback_last_pop();
                self.rt_status
                    .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_STRUCTURAL_DEFERRED);
                self.rt_status
                    .structural_deferred_total
                    .fetch_add(1, Ordering::Relaxed);
                break;
            }
            if payload.is_structural() {
                structural_applied += 1;
            }
            self.apply_structural(payload);
            if drained_count >= 64 {
                self.rt_status
                    .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_SPSC_DRAIN_TRUNCATED);
                break;
            }
        }

        if processed_any {
            self.cmd_consumer.ack_processed();
        }

        // Sync parameters changed via GUI that were not echoed as input events by the host.
        // Single Acquire load of the generation counter avoids 5 Relaxed loads per block
        // in the common case where no GUI change occurred.
        let generation = self
            .shared
            .ui_to_rt
            .gui_param_generation
            .load(Ordering::Acquire); // pairs with Release fetch_add em plugin/shared.rs:313, gui/ui/bypass.rs:62, gui/ui/knob.rs:281
        if generation != self.last_seen_generation {
            self.last_seen_generation = generation;

            self.sync_input_gain_from_gui();
            self.sync_output_gain_from_gui();
            self.sync_gate_thresh_from_gui();
            self.sync_bypass_from_gui();
            self.sync_adaptive_compute_from_gui();
            self.sync_slim_override_from_gui();
            self.sync_oversample_from_gui();
            self.sync_activation_from_gui();
        } // generation guard

        // Dynamic latency monitoring on the Audio Thread.
        //
        // RT-safety: `host.request_callback()` is intentionally NOT called here
        // (it would write to eventfd/pipe, violating "Zero Blocking I/O").
        // The atomic store is sufficient — the main thread's housekeeping loop
        // polls `current_latency` and calls `latency_ext.changed()` on its
        // regular cycle. Worst case: one main-thread-period delay in reporting.
        // This is a cold path (model activation/swap only), not the hot path.
        //
        // The effective latency is cached (`cached_effective_latency`) and
        // recomputed only in the cold swap handlers — never on this hot path.
        let effective_latency = self.cached_effective_latency;
        if effective_latency != self.shared.rt_to_ui.current_latency.load(Ordering::Relaxed) {
            self.shared
                .rt_to_ui
                .current_latency
                .store(effective_latency, Ordering::Relaxed);
        }

        // Honor render mode override: in offline mode, force adaptive compute to Off
        // for deterministic maximum-quality output. The Main Thread writes render_mode
        // with Release ordering via `clap.render.set()`.
        let render_mode = self.shared.cold.render_mode.load(Ordering::Acquire); // pairs with Release store em extensions/render.rs:30
        if render_mode != self.last_render_mode {
            self.last_render_mode = render_mode;
            if render_mode == crate::clap::plugin::RENDER_MODE_OFFLINE {
                // ── Entering Offline ──
                // Capture an immutable snapshot of the realtime state BEFORE
                // overwriting it. This snapshot is restored when returning to
                // Realtime (CLAP-F009).
                self.realtime_activation = self.params.activation_precision;
                self.adaptive_compute.set_mode(
                    neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Off,
                    &self.rt_status,
                );
                self.params.activation_precision =
                    neural_amp_modeler_rs::common::params::ActivationPrecision::Standard;
                neural_amp_modeler_rs::math::activations::set_activation_tls(
                    neural_amp_modeler_rs::common::params::ActivationPrecision::Standard,
                );
            } else {
                // ── Returning to Realtime ──
                // Restore from the immutable snapshot captured at offline entry.
                let user_mode =
                    neural_amp_modeler_rs::common::params::AdaptiveComputeMode::from_f32(
                        self.shared
                            .ui_to_rt
                            .param_adaptive_compute
                            .load(Ordering::Relaxed) as f32,
                    );
                self.adaptive_compute.set_mode(user_mode, &self.rt_status);
                self.params.activation_precision = self.realtime_activation;
                neural_amp_modeler_rs::math::activations::set_activation_tls(
                    self.realtime_activation,
                );
            }
        }
        // Also guard against user changing adaptive compute while offline (via host events
        // or SPSC, which may have bypassed the offline constraint in this same block).
        if render_mode == crate::clap::plugin::RENDER_MODE_OFFLINE
            && self.adaptive_compute.mode()
                != neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Off
        {
            self.adaptive_compute.set_mode(
                neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Off,
                &self.rt_status,
            );
        }

        // WaveNet slimmable rebuild: check if FSM demands a different channel
        // count and signal the main thread to perform the allocation-intensive
        // slice+prewarm+mmap off the audio thread.
        self.signal_slimmable_rebuild();

        // Drain any slimmable-rebuilt models delivered from the main thread.
        self.drain_slimmable_models();
    }

    /// Applies a command drained from the SPSC ring (or a deferred structural
    /// head) under the Command Budgeting policy. Routes every variant to its
    /// cold handler; `Params` is the only light (non-budgeted) payload.
    ///
    /// `#[cold]` — structural applies (model/IR/oversample swaps, restores)
    /// are rare events, never the per-block hot path. `Params` arrives here
    /// only when the drain loop found one; the compiler cold-codes the branch.
    #[cold]
    fn apply_structural(&mut self, payload: ClapParamPayload) {
        match payload {
            ClapParamPayload::Params(new_params) => {
                self.apply_params_from_spsc(new_params);
            }
            ClapParamPayload::LoadModel {
                generation,
                model_l,
                new_resampler,
                new_stream,
                input_mult_adj,
                output_mult_adj,
            } => self.cold_load_model(
                generation,
                model_l,
                new_resampler,
                new_stream,
                input_mult_adj,
                output_mult_adj,
            ),
            ClapParamPayload::LoadCabIr { adapter } => {
                self.cold_load_cabsim(adapter);
            }
            ClapParamPayload::SetOversample { os_l, os_r } => {
                self.cold_load_os(os_l, os_r);
            }
            ClapParamPayload::RestoreTxn(txn) => {
                self.cold_apply_restore_txn(txn);
            }
        }
    }

    /// Discards the heap resources of a superseded structural command off-RT
    /// (command coalescing, T2.3 / F-RT-007).
    ///
    /// When a deferred structural command is superseded by a newer same-kind
    /// command already queued in the ring, it is never applied and never
    /// dropped on the audio thread: its resources are decomposed into
    /// [`GcItem`]s and pushed through the GC cascade, so every destructor runs
    /// on the main thread — zero allocation and zero drop on the callback.
    ///
    /// Only coalescible kinds (`Model`, `CabIr`, `Oversample`) reach this
    /// helper; `Restore` is never superseded (it is ack-gated and must apply
    /// atomically) and `Params` is never deferred.
    #[cold]
    fn discard_structural_payload(&mut self, payload: ClapParamPayload) {
        match payload {
            ClapParamPayload::LoadModel {
                model_l,
                new_resampler,
                new_stream,
                ..
            } => {
                if let Some(old_l) = model_l {
                    self.push_to_gc(GcItem::Model(old_l));
                }
                self.push_to_gc(GcItem::Resampler(new_resampler));
                self.push_to_gc(GcItem::Streaming(new_stream));
            }
            ClapParamPayload::LoadCabIr { adapter } => {
                if let Some(old) = adapter {
                    self.push_to_gc(GcItem::CabConvAdapter(old));
                }
            }
            ClapParamPayload::SetOversample { os_l, os_r } => {
                self.push_to_gc(GcItem::Oversample(os_l));
                self.push_to_gc(GcItem::Oversample(os_r));
            }
            other => {
                // Unreachable by construction: the supersede probe only fires
                // for coalescible kinds. Kept as a typed arm so the match is
                // exhaustive; a `RestoreTxn`/`Params` payload would be a logic
                // bug caught by the heap-audit lane in debug builds.
                debug_assert!(
                    !other.is_structural()
                        || other.structural_kind() == Some(StructuralKind::Restore),
                    "discard_structural_payload called for a non-coalescible command"
                );
            }
        }
    }

    #[cold]
    fn cold_load_model(
        &mut self,
        generation: u64,
        model_l: Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
        new_resampler: Box<neural_amp_modeler_rs::dsp::resampler::NamResampler>,
        new_stream: Box<neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer>,
        input_mult_adj: f32,
        output_mult_adj: f32,
    ) {
        // T3.2/F-CONC-006: bind the processor's active model generation to the
        // generation carried by the install payload. This is what the slimmable
        // staleness check compares against — it must come from the payload, not
        // a read of the shared atomic (a later load could have already bumped
        // the counter past this model).
        self.model_generation = generation;
        if let Some(old_l) = std::mem::replace(&mut self.model_l, model_l) {
            self.push_to_gc(GcItem::Model(old_l));
        }
        if let Some(ref mut model) = self.model_l {
            model.inject_rt_status(std::sync::Arc::clone(&self.shared.cold.rt_status));
            // F3: buffer sizing is guaranteed on the main thread before SPSC delivery
            // (load.rs when buffer_size > 0, or flush_pending_model() in activate/housekeeping
            // when buffer_size was 0). set_max_buffer_size is NEVER called here anymore.
            // The heap-audit CI lane catches any regression.
        }

        let old_resampler = std::mem::replace(&mut self.resampler, new_resampler);
        self.push_to_gc(GcItem::Resampler(old_resampler));

        // Swap the streaming adapter and hand the old one to GC (off-RT drop).
        let old_stream = std::mem::replace(&mut self.stream, new_stream);
        self.push_to_gc(GcItem::Streaming(old_stream));

        // T3.3/F-LAT-005: publish the stream latency contribution of the
        // *installed* stream. The main thread reads this to decide whether a
        // model swap changes the physical latency (Política A: same ⇒
        // continuous swap, different ⇒ staged + `request_restart()`).
        self.shared
            .cold
            .current_stream_latency
            .store(self.stream.latency_samples(), Ordering::Relaxed);

        self.model_input_mult_adj = input_mult_adj;
        self.model_output_mult_adj = output_mult_adj;

        if let Some(ref model) = self.model_l
            && let StaticModel::WavenetDyn(w) = model.as_ref()
        {
            self.adaptive_compute
                .set_wavenet_full_ch(w.ch, model.is_slimmable_capable());
        }

        self.recompute_effective_latency();
    }

    /// Recomputes the cached effective latency (streaming adapter + oversample +
    /// cab-sim) in host-rate samples. Cold path: called only after swapping
    /// a latency-affecting resource (model/resampler, cab-sim IR, or
    /// oversample engines) — never on the per-block hot path.
    ///
    /// T4.1/F-DSP-008: the dry delay line is re-aligned to the new latency at
    /// the exact instant the wet resources land, so the delayed dry tracks the
    /// applied wet latency continuously.
    #[cold]
    fn recompute_effective_latency(&mut self) {
        // The streaming adapter (T1.2/F-PERF-002) zero-primes exactly
        // `latency_samples()` host samples, so its value is authoritative.
        let mut effective_latency = self.stream.latency_samples();
        effective_latency += self.os_l.latency_samples() as u32;
        if let Some(ref adapter) = self.cabsim_adapter {
            effective_latency += adapter.latency_samples() as u32;
        }
        self.cached_effective_latency = effective_latency;
        self.dry_delay.set_delay(effective_latency as usize);
    }

    #[cold]
    fn cold_load_cabsim(
        &mut self,
        adapter: Option<Box<neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter>>,
    ) {
        // F-RT-003/T2.1: the incoming adapter is already heap-boxed by the main
        // thread; `mem::replace` moves the old `Box` by value into the GC queue
        // with zero allocations on the audio thread. The off-RT GC drops it.
        if let Some(old_adapter) = std::mem::replace(&mut self.cabsim_adapter, adapter) {
            self.push_to_gc(GcItem::CabConvAdapter(old_adapter));
        }
        // T3.3/F-LAT-005: publish the cabsim latency contribution of the
        // *installed* adapter (0 = no IR). The main thread reads this to decide
        // whether an IR load/clear changes the physical latency.
        self.shared.cold.current_cabsim_latency.store(
            self.cabsim_adapter
                .as_ref()
                .map_or(0, |a| a.latency_samples() as u32),
            Ordering::Relaxed,
        );
        let cabsim_tail = self
            .cabsim_adapter
            .as_ref()
            .map(|a| (a.num_partitions() * a.latency_samples()) as u32)
            .unwrap_or(0);
        self.shared
            .rt_to_ui
            .cabsim_tail_samples
            .store(cabsim_tail, Ordering::Relaxed);
        self.cabsim_tail_remaining = self.cabsim_adapter.as_ref().map_or(0, |a| a.tail_samples());

        self.recompute_effective_latency();

        // Notify the host of tail changes from the audio thread,
        // which owns the valid HostAudioProcessorHandle. Eliminates the
        // unsafe main-thread → audio-thread pointer cast previously in
        // housekeeping.rs.
        if let Some(tail_ext) = self.host.get_extension::<HostTail>() {
            tail_ext.changed(&mut self.host);
        }
    }

    #[cold]
    fn cold_load_os(
        &mut self,
        os_l: Box<neural_amp_modeler_rs::dsp::oversample::OversampleEngine>,
        os_r: Box<neural_amp_modeler_rs::dsp::oversample::OversampleEngine>,
    ) {
        // T3.1/F-LAT-004: the applied factor is derived from the engine that
        // actually landed (the main thread built it off-RT), never from the
        // requested `params.oversample` — the two may legitimately diverge
        // while a host restart is pending.
        self.applied_os_factor = os_l.factor();
        let old_l = std::mem::replace(&mut self.os_l, os_l);
        let old_r = std::mem::replace(&mut self.os_r, os_r);
        self.push_to_gc(GcItem::Oversample(old_l));
        self.push_to_gc(GcItem::Oversample(old_r));

        self.recompute_effective_latency();
    }

    /// Applies a complete [`RestoreTxn`] atomically within the current block.
    ///
    /// The whole package (model, IR, params) is applied in one call so no
    /// observer ever sees a hybrid of two restores. The transaction generation
    /// is published to `ColdShared::last_applied_generation` only after every
    /// component has been installed. `#[cold]` — this is a restore path, never
    /// the per-block hot path.
    #[cold]
    fn cold_apply_restore_txn(&mut self, txn: crate::clap::plugin::shared::RestoreTxn) {
        if let Some(model) = txn.model {
            self.cold_load_model(
                model.generation,
                model.model_l,
                model.new_resampler,
                model.new_stream,
                model.input_mult_adj,
                model.output_mult_adj,
            );
        }
        if let Some(ir) = txn.ir {
            self.cold_load_cabsim(ir);
        }
        self.apply_params_from_spsc(txn.params);
        self.shared
            .cold
            .last_applied_generation
            .store(txn.generation, Ordering::Relaxed);
    }

    /// Checks if the adaptive FSM demands a WaveNet channel count change
    /// and signals the main thread to perform the rebuild off the audio thread.
    ///
    /// The audio thread ONLY sets the atomic flag and target channel count.
    /// All allocation, prewarm, and mmap happen on the main thread.
    ///
    /// T3.2/F-CONC-006: the active model generation is recorded in the request
    /// payload (`requested_slimmable_generation`) before the Release flag is
    /// set, so the main thread tags the rebuilt delivery with the exact
    /// generation the audio thread was running when it asked for the rebuild.
    fn signal_slimmable_rebuild(&mut self) {
        let Some(target_ch) = self.adaptive_compute.take_slimmable_rebuild() else {
            return;
        };
        self.rt_status
            .requested_slimmable_ch
            .store(target_ch as u32, Ordering::Relaxed);
        self.shared
            .cold
            .requested_slimmable_generation
            .store(self.model_generation, Ordering::Relaxed);
        self.rt_status.set_flag_release(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
        );
    }

    /// Drains slimmable-rebuilt models delivered by the main thread via SPSC.
    /// The main thread has already done slice_channels, prewarm, and set_max_buffer_size.
    /// The audio thread only swaps the pointer and sends the old model to GC.
    ///
    /// T3.2/F-CONC-006: each delivery carries the model generation it was
    /// sliced from. If that generation is older than the active model (a model
    /// swap happened while the rebuild was in flight), the stale result is
    /// discarded straight to the GC without touching the active DSP state — a
    /// slimmable rebuild of model A can never overwrite a subsequently loaded
    /// model B.
    fn drain_slimmable_models(&mut self) {
        while let Ok(rebuild) = self.slimmable_rx.pop() {
            if rebuild.generation != self.model_generation {
                self.push_to_gc(GcItem::Model(rebuild.model));
                self.shared
                    .cold
                    .slimmable_stale_discarded_total
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let old = self.model_l.replace(rebuild.model);
            if let Some(old) = old {
                self.push_to_gc(GcItem::Model(old));
            }
        }
    }
}

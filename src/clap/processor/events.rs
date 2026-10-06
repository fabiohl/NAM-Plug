// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Event draining: SPSC (Main Thread → Audio Thread), host events,
//! GUI parameter sync and latency monitoring.
//!
//! This module owns the [`RtSwapHandler`] binding between the engine's
//! structural-swap scheduler and the `ClapParamPayload` command family:
//! [`ClapCommandSwapHandler`], a *split-view* handler over `NamClapProcessor`
//! whose cold `install`/`discard` surface routes every payload kind to its
//! swap handler and retires every replaced or never-applied resource through
//! the GC cascade — zero drop on the audio thread.

use super::{CMD_SWAP_TUNABLES, NamClapProcessor, SLIMMABLE_SWAP_TUNABLES};
use crate::clap::plugin::{ClapParamPayload, NamClapShared, SlimmableRebuild, StructuralKind};
use clack_extensions::tail::HostTail;
use clack_plugin::host::HostAudioProcessorHandle;
use clack_plugin::prelude::OutputEvents;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::common::spsc::{GcItem, GcSink, RtSwapHandler, SwapBudget};
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;
use neural_amp_modeler_rs::math::dsp::gain_lut::GainLUT;
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

        // 1. Structural-swap drains — engine scheduler
        //    (`neural_amp_modeler_rs::common::spsc::swap`). One shared
        //    `SwapBudget` covers both rings (commands first, slimmable
        //    second) so at most one structural apply lands per callback.
        //    The `*_swap_parts!` macros expand to disjoint field borrows:
        //    drain, handler and GC sink must be borrowed simultaneously.
        // Test-only probe window: budget construction + both drains — the
        // canonical per-callback drain path measured by the latency
        // certification gate (`processor_drain_latency_test.rs`). Compiled
        // out of every non-test build; two serialized TSC reads (~15 ns
        // each, calibrated) are included in the reported samples.
        #[cfg(all(test, target_arch = "x86_64"))]
        let drain_probe_t0 = neural_amp_modeler_rs::common::tsc::rdtsc_nanos();
        let mut budget = SwapBudget::new(CMD_SWAP_TUNABLES.swaps_per_callback);
        {
            let (mut handler, mut gc) = command_swap_parts!(self);
            self.cmd_drain.drain(&mut handler, &mut budget, &mut gc);
        }
        {
            let (mut handler, mut gc) = slimmable_swap_parts!(self);
            // `SLIMMABLE_SWAP_TUNABLES` documents this consumer's dedicated-ring
            // tuning; the shared budget carries the per-callback allowance, so
            // assert the drains agree instead of constructing a second budget.
            debug_assert_eq!(
                SLIMMABLE_SWAP_TUNABLES.swaps_per_callback,
                CMD_SWAP_TUNABLES.swaps_per_callback
            );
            self.slimmable_drain
                .drain(&mut handler, &mut budget, &mut gc);
        }
        #[cfg(all(test, target_arch = "x86_64"))]
        super::DRAIN_PROBE_NS.store(
            neural_amp_modeler_rs::common::tsc::rdtsc_nanos().wrapping_sub(drain_probe_t0),
            Ordering::Relaxed,
        );

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
        // (driven autonomously by the 250 ms CLAP watchdog timer via
        // `clap_plugin_timer_support`) polls `current_latency` and emits
        // `HostLatency::changed()` without depending on sporadic GUI events or
        // user interactions. Worst case: one timer period (250 ms) delay.
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
                // Realtime.
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
    }

    /// Checks if the adaptive FSM demands a WaveNet channel count change
    /// and signals the main thread to perform the rebuild off the audio thread.
    ///
    /// The audio thread ONLY sets the atomic flag and target channel count.
    /// All allocation, prewarm, and mmap happen on the main thread.
    ///
    /// The active model generation is recorded in the request payload
    /// (`requested_slimmable_generation`) before the Release flag is set, so
    /// the main thread tags the rebuilt delivery with the exact generation the
    /// audio thread was running when it asked for the rebuild.
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
}

/// Splits a `&mut NamClapProcessor` into the two disjoint borrow domains the
/// engine's structural-swap scheduler consumes: the command-swap handler
/// (every field the cold install/discard paths mutate) and the GC cascade
/// sink (the retirement path).
///
/// This must expand to *field-path* borrows of `$s` — never to a
/// whole-struct reborrow: the [`RtSwapDrain`](neural_amp_modeler_rs::common::spsc::RtSwapDrain)
/// call site needs the handler, the [`GcSink`] and the drain itself
/// (`self.cmd_drain`) borrowed simultaneously, which only disjoint field
/// borrows allow. It is therefore a macro instead of a `&mut self` method (a
/// method's returned borrows would pin the whole processor). The single
/// definition here is the authoritative list of fields the command-swap
/// domain owns.
macro_rules! command_swap_parts {
    ($s:ident) => {{
        (
            $crate::clap::processor::events::ClapCommandSwapHandler {
                model_l: &mut $s.model_l,
                model_generation: &mut $s.model_generation,
                resampler: &mut $s.resampler,
                stream: &mut $s.stream,
                os_l: &mut $s.os_l,
                os_r: &mut $s.os_r,
                cabsim_adapter: &mut $s.cabsim_adapter,
                params: &mut $s.params,
                applied_os_factor: &mut $s.applied_os_factor,
                model_input_mult_adj: &mut $s.model_input_mult_adj,
                model_output_mult_adj: &mut $s.model_output_mult_adj,
                adaptive_compute: &mut $s.adaptive_compute,
                cached_effective_latency: &mut $s.cached_effective_latency,
                dry_delay: &mut $s.dry_delay,
                smoother_in: &mut $s.smoother_in,
                smoother_out: &mut $s.smoother_out,
                mod_input_gain: &mut $s.mod_input_gain,
                mod_output_gain: &mut $s.mod_output_gain,
                gate_dirty: &mut $s.gate_dirty,
                prewarm_pending: &mut $s.prewarm_pending,
                shared: $s.shared,
                rt_status: &$s.rt_status,
                gain_lut: $s.gain_lut,
                host: &mut $s.host,
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

/// Splits a `&mut NamClapProcessor` into the two disjoint borrow domains the
/// engine's structural-swap scheduler consumes for the dedicated slimmable
/// ring: the slimmable-swap handler (every field the cold install/discard
/// paths mutate) and the GC cascade sink (the retirement path).
///
/// Same whole-struct-reborrow rule as [`command_swap_parts!`]: this must
/// expand to *field-path* borrows of `$s` so the handler, the [`GcSink`] and
/// the drain itself (`self.slimmable_drain`) borrow simultaneously.
macro_rules! slimmable_swap_parts {
    ($s:ident) => {{
        (
            $crate::clap::processor::events::SlimmableSwapHandler {
                model_l: &mut $s.model_l,
                model_generation: &$s.model_generation,
                shared: $s.shared,
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

/// [`RtSwapHandler`] binding the engine's structural-swap scheduler to the
/// dedicated slimmable payload family ([`SlimmableRebuild`]).
///
/// Every delivery is structural (a rebuilt model install consumes the
/// callback-shared structural budget). The staleness guard compares each
/// delivery's stamped generation against the processor's active
/// `model_generation` — a stale rebuild (sliced from a model swapped out
/// while the rebuild was in flight) is discarded straight to the GC without
/// touching the active DSP state, so a rebuild of model A can never overwrite
/// a subsequently loaded model B. Latest-wins identity is the stamped
/// generation itself: rebuilds sliced from the same model collapse to the
/// newest, while rebuilds of different models never supersede each other
/// (stale ones fall on the generation guard, FIFO intact).
/// Budget-exhausted heads stay queued with FIFO intact and resolve on a
/// later callback; superseded or stale deliveries retire through the GC
/// cascade with zero heap drop on the audio thread. All heavy methods are
/// `#[cold]`.
pub(crate) struct SlimmableSwapHandler<'p, 'a> {
    /// Active neural model (mono/primary chain), if any.
    pub(super) model_l: &'p mut Option<Box<StaticModel>>,
    /// Generation of the active model (slimmable staleness anchor, read-only:
    /// a rebuild never re-stamps the identity it was sliced from).
    pub(super) model_generation: &'p u64,
    /// Shared main<->RT state (stale-discard telemetry counter).
    pub(super) shared: &'a NamClapShared,
}

impl RtSwapHandler for SlimmableSwapHandler<'_, '_> {
    type Payload = SlimmableRebuild;

    /// Every slimmable delivery is a structural model install.
    #[inline(always)]
    fn is_structural(&self, _payload: &Self::Payload) -> bool {
        true
    }

    /// Latest-wins identity: the model generation the delivery was sliced
    /// from. Rebuilds of the same model collapse to the newest; rebuilds of
    /// different models never supersede each other (stale ones fall on the
    /// generation guard below, FIFO intact).
    #[inline(always)]
    fn coalesce_key(&self, payload: &Self::Payload) -> Option<u64> {
        Some(payload.generation)
    }

    /// Generation the audio thread currently runs (the staleness anchor).
    #[inline(always)]
    fn current_generation(&self) -> Option<u64> {
        Some(*self.model_generation)
    }

    /// Generation the delivery was sliced from (stamped by the main thread).
    #[inline(always)]
    fn generation_of(&self, payload: &Self::Payload) -> Option<u64> {
        Some(payload.generation)
    }

    /// Installs the rebuilt model, retiring the replaced model to the GC.
    /// The active generation anchor is left untouched: the delivery was
    /// verified to match it, and a rebuild never re-stamps the model identity.
    /// Adaptive tracking is also untouched — it was already advanced by
    /// `take_slimmable_rebuild` at request time, and the full-channel
    /// reference must survive the install.
    #[cold]
    fn install(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        let rebuild = *payload;
        if let Some(old) = self.model_l.replace(rebuild.model) {
            gc.retire(GcItem::Model(old));
        }
    }

    /// Discards a stale or superseded delivery straight to the GC without
    /// touching the active DSP state. Only generation-mismatched (stale)
    /// discards bump the monotonic `slimmable_stale_discarded_total`
    /// telemetry counter; same-generation supersession (latest-wins
    /// coalescing) is already covered by the engine's
    /// `RT_STATUS_STRUCTURAL_SUPERSEDED` telemetry.
    #[cold]
    fn discard(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        let rebuild = *payload;
        if rebuild.generation != *self.model_generation {
            self.shared
                .cold
                .slimmable_stale_discarded_total
                .fetch_add(1, Ordering::Relaxed);
        }
        gc.retire(GcItem::Model(rebuild.model));
    }
}

/// [`RtSwapHandler`] binding the engine's structural-swap scheduler to the
/// CLAP command payload family ([`ClapParamPayload`]).
///
/// The mixed command ring carries every command kind, so this handler is the
/// classification *and* install surface for all of them:
/// - `is_structural`: `Params` are light scalars — applied inline during the
///   drain, never parked, never coalesced by the scheduler. Every other kind
///   (`Model`, `CabIr`, `Oversample`, `Restore`) consumes the shared
///   structural budget.
/// - `coalesce_key`: the coalescible kinds (`Model`, `CabIr`, `Oversample`)
///   collapse latest-wins on their kind id — e.g. repeated IR swaps in one
///   callback window reduce to the newest; `RestoreTxn` returns `None`, since
///   a full-preset restore is an ack-gated atomic transaction that is never
///   superseded and never supersedes.
/// - `install`/`discard` decompose payloads into [`GcItem`]s and retire every
///   replaced or never-applied resource through the GC cascade, so no
///   destructor ever runs on the audio thread.
///
/// Borrowing: the handler is a *split view* of [`NamClapProcessor`]. The
/// engine drain needs the handler, the [`GcSink`] and the ring all borrowed
/// at once — impossible with a whole-processor `&mut` — so the handler holds
/// disjoint field borrows built by [`command_swap_parts!`]. The `&mut` fields
/// are exactly the swap domain (model/resampler/stream/engines/params/…
/// latency and gain calibration state); `shared`, `rt_status`, `gain_lut`
/// and the host handle are ambient references. All heavy methods are
/// `#[cold]`.
///
/// Acknowledgment: `resolved` counts every trait-level install/discard of a
/// drain. Each one consumes exactly one ring sequence slot (popped payloads,
/// or a parked payload resolved without a pop), so `after_drain` publishing
/// `last_ack + resolved` reproduces the consumer's `processed_seq` exactly —
/// the ack stays monotonic and gapless across deferrals.
pub(crate) struct ClapCommandSwapHandler<'p, 'a> {
    // ── Swap domain: fields the cold install/discard paths mutate ──
    /// Active neural model (mono/primary chain), if any.
    pub(super) model_l: &'p mut Option<Box<StaticModel>>,
    /// Generation of the active model (slimmable staleness anchor).
    pub(super) model_generation: &'p mut u64,
    /// Polyphase sinc resampler between host and model rate.
    pub(super) resampler: &'p mut Box<NamResampler>,
    /// Streaming resample adapter (owns the authoritative stream latency).
    pub(super) stream: &'p mut Box<StreamingResampleBuffer>,
    /// Oversample engines (L/R) and the factor they were actually built for.
    pub(super) os_l: &'p mut Box<OversampleEngine>,
    pub(super) os_r: &'p mut Box<OversampleEngine>,
    /// Cab-sim convolution adapter (`None` = bypass cabsim).
    pub(super) cabsim_adapter: &'p mut Option<Box<CabSimAdapter>>,
    /// Last full parameter snapshot applied from the ring.
    pub(super) params: &'p mut RtProcessingParams,
    /// Oversampling factor of the installed engines.
    pub(super) applied_os_factor: &'p mut OversampleFactor,
    /// Model input/output calibration multipliers (level/loudness metadata).
    pub(super) model_input_mult_adj: &'p mut f32,
    pub(super) model_output_mult_adj: &'p mut f32,
    /// Adaptive compute FSM (WaveNet channel switching).
    pub(super) adaptive_compute: &'p mut AdaptiveCompute,
    /// Cached effective latency (stream + oversample + cabsim), host samples.
    pub(super) cached_effective_latency: &'p mut u32,
    /// Dry path delay line tracking the effective latency.
    pub(super) dry_delay: &'p mut crate::clap::processor::dsp::dry_delay::DryDelayLine,
    /// Sample-accurate gain smoothers (targets updated per snapshot).
    pub(super) smoother_in: &'p mut ParamSmoother,
    pub(super) smoother_out: &'p mut ParamSmoother,
    /// Last modulation values applied onto the gain targets.
    pub(super) mod_input_gain: &'p mut f32,
    pub(super) mod_output_gain: &'p mut f32,
    /// Gate coefficient invalidation flag (recomputed in the DSP block).
    pub(super) gate_dirty: &'p mut bool,
    /// Two-phase reset latch (`prewarm_pending`): a model install ends the
    /// amortization window — fresh models arrive converged from the off-RT
    /// loader, so a model swapped in mid-window restarts converged (never
    /// half-drained) and the next callback runs the normal wet path.
    pub(super) prewarm_pending: &'p mut bool,

    // ── Ambient references (not owned by the swap domain) ──
    /// Shared main↔RT state (latency publications, ack atomic, restart state).
    pub(super) shared: &'a NamClapShared,
    /// RT status flags (telemetry, requested oversample, adaptive sync).
    pub(super) rt_status: &'p neural_amp_modeler_rs::common::spsc::RtStatusFlags,
    /// Static gain lookup table (dB → linear).
    pub(super) gain_lut: &'static GainLUT,
    /// Audio-thread host handle (`request_restart`, `HostTail::changed`).
    pub(super) host: &'p mut HostAudioProcessorHandle<'a>,

    /// Install/discard count of the current drain, consumed by
    /// [`RtSwapHandler::after_drain`] for the gapless ack publication.
    pub(super) resolved: u64,
}

impl RtSwapHandler for ClapCommandSwapHandler<'_, '_> {
    type Payload = ClapParamPayload;

    /// Light scalars (`Params`) drain freely; every other kind is structural.
    #[inline(always)]
    fn is_structural(&self, payload: &ClapParamPayload) -> bool {
        payload.is_structural()
    }

    /// Latest-wins identity: the structural kind id. `RestoreTxn` has no key
    /// — an atomic, ack-gated transaction preserves strict FIFO order.
    #[inline(always)]
    fn coalesce_key(&self, payload: &ClapParamPayload) -> Option<u64> {
        payload
            .structural_kind()
            .filter(|kind| kind.is_coalescible())
            .map(|kind| kind as u64)
    }

    /// Applies one command. `#[cold]` — structural applies are rare; light
    /// `Params` snapshots arrive here only when the drain found one.
    #[cold]
    fn install(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        self.resolved += 1;
        self.apply_structural(*payload, gc);
    }

    /// Discards the heap resources of a superseded command off-RT.
    /// `#[cold]` — supersession only fires on coalesced bursts.
    #[cold]
    fn discard(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        self.resolved += 1;
        self.discard_structural_payload(*payload, gc);
    }

    /// Publishes the command acknowledgment for this drain.
    ///
    /// Every trait-level install/discard consumed exactly one ring sequence
    /// slot (a popped payload, or a parked payload resolved without a pop),
    /// and a parked candidate had its slot rolled back without consuming the
    /// counter — so `last_ack + resolved` reproduces the consumer's
    /// `processed_seq` exactly. Publishing in `after_drain` keeps the main
    /// thread's `wait_for_ack` contract gapless even when the drain parks
    /// (the ack never covers an unapplied payload) and is a no-op store-free
    /// call for drains that resolved nothing.
    fn after_drain(&mut self, _gc: &mut GcSink<'_>) {
        if self.resolved == 0 {
            return;
        }
        let last_ack = &self.shared.cold.cmd_last_ack;
        last_ack.store(
            last_ack.load(Ordering::Acquire) + self.resolved,
            Ordering::Release,
        );
    }
}

impl ClapCommandSwapHandler<'_, '_> {
    /// Applies one command drained from the ring (or a parked deferred head).
    /// Routes every variant to its cold handler; `Params` is the only light
    /// (non-budgeted) payload and applies via the full parameter snapshot.
    #[cold]
    fn apply_structural(&mut self, payload: ClapParamPayload, gc: &mut GcSink<'_>) {
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
                crate::clap::plugin::shared::LoadModelPayload {
                    generation,
                    model_l,
                    new_resampler,
                    new_stream,
                    input_mult_adj,
                    output_mult_adj,
                },
                gc,
            ),
            ClapParamPayload::LoadCabIr { adapter } => {
                self.cold_load_cabsim(adapter, gc);
            }
            ClapParamPayload::SetOversample { os_l, os_r } => {
                self.cold_load_os(os_l, os_r, gc);
            }
            ClapParamPayload::RestoreTxn(txn) => {
                self.cold_apply_restore_txn(txn, gc);
            }
        }
    }

    /// Discards the heap resources of a superseded structural command off-RT
    /// (command coalescing — latest-wins).
    ///
    /// A superseded command is never applied and never dropped on the audio
    /// thread: its resources are decomposed into [`GcItem`]s and retired
    /// through the GC cascade, so every destructor runs on the main thread.
    ///
    /// Only coalescible kinds (`Model`, `CabIr`, `Oversample`) reach this
    /// helper: the scheduler discards payloads only on supersession, and
    /// `Restore` is never superseded while `Params` is never parked.
    #[cold]
    fn discard_structural_payload(&mut self, payload: ClapParamPayload, gc: &mut GcSink<'_>) {
        match payload {
            ClapParamPayload::LoadModel {
                model_l,
                new_resampler,
                new_stream,
                ..
            } => {
                if let Some(old_l) = model_l {
                    gc.retire(GcItem::Model(old_l));
                }
                gc.retire(GcItem::Resampler(new_resampler));
                gc.retire(GcItem::Streaming(new_stream));
            }
            ClapParamPayload::LoadCabIr { adapter } => {
                if let Some(old) = adapter {
                    gc.retire(GcItem::CabConvAdapter(old));
                }
            }
            ClapParamPayload::SetOversample { os_l, os_r } => {
                gc.retire(GcItem::Oversample(os_l));
                gc.retire(GcItem::Oversample(os_r));
            }
            other => {
                // Unreachable by construction: the scheduler discards only
                // coalescible kinds. Kept as a typed arm so the match is
                // exhaustive; a `RestoreTxn`/`Params` payload here would be a
                // logic bug caught by the heap-audit lane in debug builds.
                debug_assert!(
                    !other.is_structural()
                        || other.structural_kind() == Some(StructuralKind::Restore),
                    "discard_structural_payload called for a non-coalescible command"
                );
            }
        }
    }

    /// Installs a new model (mono/primary chain) with its resampler and
    /// streaming adapter, retiring the replaced resources to the GC.
    #[cold]
    fn cold_load_model(
        &mut self,
        payload: crate::clap::plugin::shared::LoadModelPayload,
        gc: &mut GcSink<'_>,
    ) {
        let crate::clap::plugin::shared::LoadModelPayload {
            generation,
            model_l,
            new_resampler,
            new_stream,
            input_mult_adj,
            output_mult_adj,
        } = payload;
        // Bind the processor's active model generation to the generation
        // carried by the install payload. This is what the slimmable staleness
        // check compares against — it must come from the payload, not a read of
        // the shared atomic (a later load could have already bumped the counter
        // past this model).
        *self.model_generation = generation;
        if let Some(old_l) = std::mem::replace(self.model_l, model_l) {
            gc.retire(GcItem::Model(old_l));
        }
        // Swap clears the two-phase reset latch: a freshly installed model
        // arrives converged from the off-RT loader, so a model swapped in
        // mid-drain restarts converged (never half-drained) and the next
        // callback runs the normal wet path.
        *self.prewarm_pending = false;
        if let Some(model) = self.model_l.as_mut() {
            model.inject_rt_status(std::sync::Arc::clone(&self.shared.cold.rt_status));
            // Buffer sizing is guaranteed on the main thread before SPSC delivery
            // (load.rs when buffer_size > 0, or flush_pending_model() in activate/housekeeping
            // when buffer_size was 0). set_max_buffer_size is NEVER called here anymore.
            // The heap-audit CI lane catches any regression.
        }

        let old_resampler = std::mem::replace(self.resampler, new_resampler);
        gc.retire(GcItem::Resampler(old_resampler));

        // Swap the streaming adapter and hand the old one to GC (off-RT drop).
        let old_stream = std::mem::replace(self.stream, new_stream);
        gc.retire(GcItem::Streaming(old_stream));

        // Publish the stream latency contribution of the *installed* stream.
        // The main thread reads this to decide whether a model swap changes
        // the physical latency (same ⇒ continuous swap, different ⇒ staged +
        // `request_restart()`).
        self.shared
            .cold
            .current_stream_latency
            .store(self.stream.latency_samples(), Ordering::Relaxed);

        *self.model_input_mult_adj = input_mult_adj;
        *self.model_output_mult_adj = output_mult_adj;

        if let Some(model) = self.model_l.as_ref()
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
    /// The dry delay line is re-aligned to the new latency at the exact
    /// instant the wet resources land, so the delayed dry tracks the applied
    /// wet latency continuously.
    #[cold]
    fn recompute_effective_latency(&mut self) {
        // The streaming adapter zero-primes exactly `latency_samples()` host
        // samples, so its value is authoritative.
        let mut effective_latency = self.stream.latency_samples();
        effective_latency += self.os_l.latency_samples() as u32;
        if let Some(adapter) = self.cabsim_adapter.as_ref() {
            effective_latency += adapter.latency_samples() as u32;
        }
        *self.cached_effective_latency = effective_latency;
        self.dry_delay.set_delay(effective_latency as usize);
    }

    /// Installs a new cab-sim convolution adapter (or clears to bypass),
    /// retiring the replaced one to the GC, and notifies the host that the
    /// plugin tail changed.
    #[cold]
    fn cold_load_cabsim(&mut self, adapter: Option<Box<CabSimAdapter>>, gc: &mut GcSink<'_>) {
        // The incoming adapter is already heap-boxed by the main thread;
        // `mem::replace` moves the old `Box` by value into the GC queue with
        // zero allocations on the audio thread. The off-RT GC drops it.
        if let Some(old_adapter) = std::mem::replace(self.cabsim_adapter, adapter) {
            gc.retire(GcItem::CabConvAdapter(old_adapter));
        }
        // Publish the cabsim latency contribution of the *installed* adapter
        // (0 = no IR). The main thread reads this to decide whether an IR
        // load/clear changes the physical latency.
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

        self.recompute_effective_latency();

        // Notify the host of tail changes from the audio thread,
        // which owns the valid HostAudioProcessorHandle. Eliminates the
        // unsafe main-thread → audio-thread pointer cast previously in
        // housekeeping.rs.
        if let Some(tail_ext) = self.host.get_extension::<HostTail>() {
            tail_ext.changed(self.host);
        }
    }

    /// Installs new oversample engines (L/R), retiring the replaced pair to
    /// the GC and recording the factor the engines were actually built for.
    #[cold]
    fn cold_load_os(
        &mut self,
        os_l: Box<OversampleEngine>,
        os_r: Box<OversampleEngine>,
        gc: &mut GcSink<'_>,
    ) {
        // The applied factor is derived from the engine that actually landed
        // (the main thread built it off-RT), never from the requested
        // `params.oversample` — the two may legitimately diverge while a host
        // restart is pending.
        *self.applied_os_factor = os_l.factor();
        let old_l = std::mem::replace(self.os_l, os_l);
        let old_r = std::mem::replace(self.os_r, os_r);
        gc.retire(GcItem::Oversample(old_l));
        gc.retire(GcItem::Oversample(old_r));

        self.recompute_effective_latency();
    }

    /// Applies a complete [`RestoreTxn`](crate::clap::plugin::shared::RestoreTxn)
    /// atomically within the current block.
    ///
    /// The whole package (model, IR, params) is applied in one call so no
    /// observer ever sees a hybrid of two restores. The transaction generation
    /// is published to `ColdShared::last_applied_generation` only after every
    /// component has been installed. `#[cold]` — this is a restore path, never
    /// the per-block hot path.
    #[cold]
    fn cold_apply_restore_txn(
        &mut self,
        txn: crate::clap::plugin::shared::RestoreTxn,
        gc: &mut GcSink<'_>,
    ) {
        if let Some(model) = txn.model {
            self.cold_load_model(model, gc);
        }
        if let Some(ir) = txn.ir {
            self.cold_load_cabsim(ir, gc);
        }
        self.apply_params_from_spsc(txn.params);
        self.shared
            .cold
            .last_applied_generation
            .store(txn.generation, Ordering::Relaxed);
    }
}

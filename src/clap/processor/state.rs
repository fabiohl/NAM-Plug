// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Processor state (struct definition).

use crate::clap::plugin::{ClapParamPayload, CommandConsumer, NamClapShared, SlimmableRebuild};
use crate::clap::processor::dsp::dry_delay::DryDelayLine;
use crate::clap::processor::dsp::orchestrator::ScheduledEvent;
use clack_plugin::host::HostAudioProcessorHandle;
use neural_amp_modeler_rs::common::params::{ActivationPrecision, RtProcessingParams};
use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, RtStatusFlags};
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateParams};
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;
use neural_amp_modeler_rs::math::common::AlignedVec;
use neural_amp_modeler_rs::math::dsp::gain_lut::GainLUT;
use neural_amp_modeler_rs::models::StaticModel;
use rtrb::{Consumer, Producer};
use std::sync::Arc;

pub const BYPASS_XFADE_SAMPLES: usize = 64;
pub(crate) const BYPASS_XFADE_INV: f32 = 1.0 / BYPASS_XFADE_SAMPLES as f32;

/// Command Budgeting: maximum number of structural (heavy swap) commands
/// applied per audio callback.
///
/// Structural applies (model/resampler swap, cab-sim IR swap, oversample engine
/// rebuild, full state restore) recompute latency, feed the GC cascade and may
/// call host extensions (`HostTail::changed`) — their cost is not bounded by
/// command *count*, so a burst of 64 structural payloads must not all execute
/// in one callback. A single deferred structural command is parked per callback
/// and applied at the start of the next one, preserving FIFO ordering and the
/// atomicity of composite transactions (`RestoreTxn`).
pub(crate) const MAX_STRUCTURAL_COMMANDS_PER_CALLBACK: u32 = 1;

/// Sample-accurate bypass crossfade state machine.
///
/// When bypass toggles via CLAP host event, a 64-sample linear crossfade
/// blends between the dry (passthrough) and wet (pipeline) signals to
/// prevent click artifacts and phase discontinuities.
///
/// Direction:
/// - `mix = 0` → fully dry (bypass ON)
/// - `mix = 1` → fully wet (bypass OFF = pipeline running)
/// - On un-bypass (OFF→ON or equiv): `step = +INV`, ramp from current to target
/// - On bypass (ON→OFF or equiv): `step = -INV`, ramp from current to target
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BypassCrossfader {
    /// Target bypass state (false = pipeline, true = bypass).
    pub target: bool,
    /// Whether a crossfade is in progress.
    pub active: bool,
    /// Current mix position [0.0 = dry, 1.0 = wet].
    pub mix: f32,
    /// Per-sample step for the mix ramp.
    pub step: f32,
    /// Samples remaining in the crossfade.
    pub remaining: usize,
}

impl BypassCrossfader {
    pub fn new(initial_bypass: bool) -> Self {
        let mix = if initial_bypass { 0.0 } else { 1.0 };
        Self {
            target: initial_bypass,
            active: false,
            mix,
            step: 0.0,
            remaining: 0,
        }
    }

    /// Trigger a crossfade towards the given bypass state.
    /// If already at or transitioning to `target`, does nothing.
    ///
    /// # Design Rationale — Rapid Automation Toggling
    ///
    /// If bypass toggles rapidly (faster than `BYPASS_XFADE_SAMPLES` = 64 samples),
    /// invoking `trigger()` while a crossfade is still active restarts the 64-sample
    /// ramp from the current intermediate `mix` position `[0.0, 1.0]` towards the new
    /// target state (`remaining` is reset to 64 and `step` reverses direction).
    ///
    /// Preserving the current `mix` value ensures continuous signal transitions without
    /// instantaneous level jumps or clicks. However, rapid direction reversals mid-ramp
    /// under extreme automation rates may produce subtle transition dynamics.
    pub fn trigger(&mut self, target: bool) {
        if self.target == target {
            return;
        }
        self.target = target;
        self.active = true;
        self.remaining = BYPASS_XFADE_SAMPLES;
        if target {
            self.step = -BYPASS_XFADE_INV;
        } else {
            self.step = BYPASS_XFADE_INV;
        }
    }
}

/// RT-safe audio processor. Runs on the host's audio thread.
///
/// Holds pre-allocated buffers and mutable inference state.
/// Created in `activate()` and destroyed in `deactivate()`.
pub struct NamClapProcessor<'a> {
    /// Active model for the left channel (None = bypass).
    pub(crate) model_l: Option<Box<StaticModel>>,
    /// Monotonic generation of the currently active model identity.
    ///
    /// Set to the generation carried by the model-install payload in
    /// `cold_load_model()` — never read back from the shared atomic, which may
    /// have already advanced past this model. `signal_slimmable_rebuild()`
    /// publishes it into the rebuild request and `drain_slimmable_models()`
    /// compares it against each rebuilt delivery to reject stale results.
    pub(crate) model_generation: u64,
    /// Active cab-sim convolution adapter (None = bypass, zero cost).
    /// Held in `Box` end-to-end (main thread → SPSC → RT swap → GC) so
    /// installing/swapping/clearing an IR never allocates on the audio thread.
    pub(crate) cabsim_adapter: Option<Box<CabSimAdapter>>,
    /// Polyphase sinc resampler (bypass when sample_rate == 48000).
    /// Held in Box for RT-safe disposal without allocation.
    pub(crate) resampler: Box<NamResampler>,
    /// Strict-cardinality streaming resample adapter.
    /// Owns the bounded FIFO pipeline that guarantees exactly `frames_count`
    /// host samples are produced per callback. Built off-RT; swapped on the
    /// audio thread via SPSC with the old box disposed by GC.
    pub(crate) stream: Box<neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer>,
    /// Half-band oversampling engine for the left channel.
    pub(crate) os_l: Box<OversampleEngine>,
    /// Half-band oversampling engine for the right channel.
    pub(crate) os_r: Box<OversampleEngine>,
    /// Current parameters on the audio thread (snapshotted from SPSC at each process()).
    pub(crate) params: RtProcessingParams,
    /// Oversampling factor **actually applied** by the active engines
    /// (`os_l`/`os_r`).
    ///
    /// Updated **only** at the instant engines are installed — `activate()`
    /// and `cold_load_os()` (derived from the incoming engine itself). Never
    /// updated eagerly by parameter events: `params.oversample` carries the
    /// user/DAW-requested factor and may diverge from this field while a host
    /// restart is pending. `deactivate()` persists this applied factor into
    /// [`DeactivatedDspState::os_factor`] so a reactivation can verify real
    /// engine compatibility instead of trusting the requested value.
    pub(crate) applied_os_factor: OversampleFactor,

    /// Intermediate buffers pre-allocated in activate() — ZERO alloc in process().
    /// 1. Copy of host input (variable sample_rate)
    pub(crate) buf_host_l: AlignedVec<f32>,
    pub(crate) buf_host_r: AlignedVec<f32>,
    /// 2. Post-resampler input / Pre-model (f32 @ 48kHz)
    pub(crate) buf_mid_l: AlignedVec<f32>,
    pub(crate) buf_mid_r: AlignedVec<f32>,
    /// 3. Post-model / Pre-resampler output (f32 @ 48kHz)
    pub(crate) buf_model_l: AlignedVec<f32>,
    pub(crate) buf_model_r: AlignedVec<f32>,
    /// 4. Post-resampler output / Final (variable sample_rate)
    pub(crate) buf_out_l: AlignedVec<f32>,
    pub(crate) buf_out_r: AlignedVec<f32>,
    /// 5. Oversampled input buffers (pre-model, at 2×/4× rate).
    pub(crate) buf_os_in_l: AlignedVec<f32>,
    pub(crate) buf_os_in_r: AlignedVec<f32>,
    /// 6. Oversampled model output buffers (post-model, at 2×/4× rate).
    pub(crate) buf_os_model_l: AlignedVec<f32>,
    pub(crate) buf_os_model_r: AlignedVec<f32>,

    /// Hysteresis for absolute silence detection.
    pub(crate) silence_hyst: DynamicHysteresis,
    /// Hysteresis for mono signal detection. Persistent field to avoid
    /// re-initialization on every port_pair iteration.
    pub(crate) mono_hyst: DynamicHysteresis,
    /// Flag indicating whether we are processing in mono (for optimization).
    pub(crate) process_mono: bool,
    /// Pre-allocated event buffer for host CLAP parameter events.
    /// Cleared and refilled each process() cycle — zero alloc on RT thread.
    pub(crate) scheduled_events: Vec<ScheduledEvent>,
    /// Bypass crossfade state machine for click-free bypass transitions.
    pub(crate) bypass_xfade: BypassCrossfader,
    /// Pre-allocated circular dry delay line: delays the dry (bypass/crossfade)
    /// signal by exactly the applied wet latency (`cached_effective_latency`)
    /// so dry and wet always represent the same temporal instant, and the
    /// fully-bypassed path keeps the physical latency declared to the host.
    /// Zero allocations on the audio thread.
    pub(crate) dry_delay: DryDelayLine,
    /// Dry input signal storage for bypass crossfade blending.
    /// `dry_delay` writes the latency-compensated dry here each sub-block;
    /// the pipeline modifies `buf_host_l/r` in place, and these preserve the
    /// *delayed* dry signal for the bypass output and the crossfade blend.
    pub(crate) buf_xfade_dry_l: AlignedVec<f32>,
    pub(crate) buf_xfade_dry_r: AlignedVec<f32>,
    /// 7. WaveNet crossfade scratch buffers (motor 0.5.0 `run_inference`):
    ///    second-pass output used when processing is chunked (active
    ///    resampler). Pre-allocated `MAX_RESAMP_BUF` each — zero alloc in
    ///    `process()`.
    pub(crate) buf_xfd_scratch_l: AlignedVec<f32>,
    pub(crate) buf_xfd_scratch_r: AlignedVec<f32>,

    /// Status flags for RT telemetry.
    pub(crate) rt_status: Arc<RtStatusFlags>,
    /// Adaptive compute FSM for soft-degrade under CPU pressure.
    pub(crate) adaptive_compute: AdaptiveCompute,
    /// Reference to shared state (to return channels on deactivate).
    pub(crate) shared: &'a NamClapShared,
    /// Smoothers for input and output gains.
    pub(crate) smoother_in: ParamSmoother,
    /// Smoothers for input and output gains.
    pub(crate) smoother_out: ParamSmoother,
    /// Model input calibration multiplier (from input_level_dbu metadata).
    /// Applied as `input_gain_mult` in the DSP pipeline context, separate from
    /// user-configured input gain applied via `smoother_in`.
    pub(crate) model_input_mult_adj: f32,
    /// Model output calibration multiplier (from loudness metadata).
    /// Applied as `output_gain_mult` in the DSP pipeline context, separate from
    /// user-configured output gain applied via `smoother_out`.
    pub(crate) model_output_mult_adj: f32,
    /// Parking lot for model/resampler disposal if the GC channel is full.
    pub(crate) parking_lot: [Option<GcItem>; 16],
    /// Command consumer with acknowledgment.
    pub(crate) cmd_consumer: CommandConsumer<'a>,
    /// Single-slot deferral for Command Budgeting.
    ///
    /// When the per-callback structural budget is exhausted, the drained
    /// structural command is parked here (its sequence slot is rolled back so
    /// the ack never covers an unapplied command) and the drain loop stops —
    /// everything still in the ring is causally *after* this command, so FIFO
    /// order is preserved. At the start of the next callback the parked command
    /// is applied first (or superseded by a newer same-kind ring head via
    /// command coalescing, in which case its resources are discarded off-RT
    /// through the GC cascade).
    pub(crate) deferred_structural: Option<ClapParamPayload>,
    /// SPSC channel: Main Thread -> Audio Thread (Slimmable model consumer).
    pub(crate) slimmable_rx: Consumer<SlimmableRebuild>,
    /// GC channel: Audio Thread -> Main Thread (Producer).
    pub(crate) gc_tx: Producer<GcItem>,
    /// Fallback buffer for GC overflow (overwrite).
    pub(crate) gc_overflow: Arc<GcOverflowBuffer>,
    /// Modulation offsets (CLAP Parameter Modulation).
    pub(crate) mod_input_gain: f32,
    /// Modulation offsets (CLAP Parameter Modulation).
    pub(crate) mod_output_gain: f32,
    /// Modulation offsets (CLAP Parameter Modulation).
    pub(crate) mod_gate_thresh: f32,
    /// Pre-computed thresholds (linear²) — invalidated only when
    /// gate_threshold_db or mod_gate_thresh changes.
    /// SHARED ALGORITHM: Any change to the cache/invalidation logic
    /// here must be mirrored in src/standalone/pw_host.rs (threshold_open_sq
    /// and threshold_close_sq), and vice-versa. Both pre-calculate thresholds in
    /// linear² via LUT to avoid lookups on the RT hotpath.
    pub(crate) cached_threshold_open_sq: f32,
    pub(crate) cached_threshold_close_sq: f32,
    pub(crate) cached_gate_params: GateParams,
    pub(crate) gate_dirty: bool,
    /// Telemetry decimation: 1-in-16. Cycle counter since last measurement.
    /// SHARED ALGORITHM: Same decimation strategy as `src/standalone/pw_host.rs` (frame_count & 0xF).
    /// Any change to the decimation logic here must be mirrored in pw_host.rs, and vice-versa.
    pub(crate) cycles_since_telemetry: u32,
    /// Per-instance flag for one-time RT priority query on the first block.
    pub(crate) prio_checked: bool,
    /// Monotonic generation counter for GUI param synchronization.
    /// Guard: only load atomics from UiToRt when generation differs.
    pub(crate) last_seen_generation: u32,
    /// Host audio buffer size, used for RT-safety contract validation.
    pub(crate) max_frames_count: usize,
    /// Last seen render mode for transition detection (0 = Realtime, 1 = Offline).
    pub(crate) last_render_mode: u32,
    /// Immutable snapshot of activation precision captured when entering
    /// Offline mode. Restored when returning to Realtime.
    /// Initialized to the same value as `params.activation_precision`
    /// during activate().
    pub(crate) realtime_activation: ActivationPrecision,
    /// Pre-resolved gain LUT reference, hoisted from process_events hot-path.
    pub(crate) gain_lut: &'static GainLUT,
    /// Remaining cab-sim tail samples to drain after the noise gate closes.
    /// Decremented by `process_tail_drain` until zero, at which point the
    /// tail ring-out is complete and true silence can be emitted.
    pub(crate) cabsim_tail_remaining: usize,
    /// Effective latency (resampler + oversample + cab-sim) in host-rate
    /// samples, cached on the audio thread. Recomputed only in the cold
    /// handlers that swap latency-affecting resources (model/resampler,
    /// cab-sim IR, oversample engines) — `process_events` reads this cache
    /// instead of recomputing on every block.
    pub(crate) cached_effective_latency: u32,
    /// Host audio processor handle. Used for `host.request_restart()` when
    /// structural latency changes are pending.
    pub(crate) host: HostAudioProcessorHandle<'a>,
}

#[cfg(test)]
#[path = "state_test.rs"]
mod tests;

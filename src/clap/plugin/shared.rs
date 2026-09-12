// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Lock-free shared state between the audio thread and the main thread.

use crate::clap::processor::DeactivatedDspState;
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::diagnostics::ModelInfo;
use neural_amp_modeler_rs::common::diagnostics::NamErrorCode;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, RtStatusFlags};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::models::StaticModel;
use rtrb::{Consumer, Producer};
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Active plugin instance counter.
/// Incremented in `new_shared()`, decremented in `NamClapShared::drop()`.
/// `set_shutdown_in_progress()` is only called when this reaches zero,
/// so crash-reporting remains active as long as at least one instance exists.
static ACTIVE_INSTANCES: AtomicU32 = AtomicU32::new(0);

/// Monotonic instance identifier generator for per-instance logging and diagnostics isolation.
static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// Increments the active instance counter.
#[inline]
pub(crate) fn bump_active_instances() {
    let prev = ACTIVE_INSTANCES.fetch_add(1, Ordering::Release);
    if prev == 0 {
        neural_amp_modeler_rs::common::panic_hook::clear_shutdown_in_progress();
    }
}

/// Allocates a unique monotonic instance ID.
#[inline]
pub(crate) fn next_instance_id() -> u64 {
    NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Builds the strict-cardinality streaming resample adapter
/// on the main thread, sized for the worst-case host block.
///
/// `max_block` is the host buffer size (clamped to the adapter's supported
/// range); all storage (FIFOs + aligned scratch) is allocated here — the audio
/// thread only swaps the box, so the processing path stays zero-alloc.
#[inline]
pub(crate) fn build_stream_adapter(
    host_rate: u32,
    model_rate: u32,
    max_block: usize,
) -> Result<Box<neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer>, NamErrorCode> {
    neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer::new(
        host_rate,
        model_rate,
        max_block.max(1),
    )
    .map(Box::new)
}

/// Pending host-restart oversampling request.
///
/// Replaces the legacy raw `AtomicU32` whose `0` value collided between
/// `OversampleFactor::Off` and "no restart pending". The new encoding is
/// offset by one so a pending transition **to Off** is representable:
///
/// | raw | meaning |
/// | --- | --- |
/// | `0` | no restart pending |
/// | `1` | restart pending → `Off` |
/// | `2` | restart pending → `X2` |
/// | `3` | restart pending → `X4` |
///
/// Written by the audio thread (Release) whenever a latency-affecting
/// oversampling change is requested while the plugin is active, read/consumed
/// by `activate()` (Acquire). Never heap-allocates — RT-safe by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingRestartOs {
    /// No host restart pending.
    None,
    /// A host restart is pending; `factor` is the requested target.
    Pending(OversampleFactor),
}

impl PendingRestartOs {
    /// Encodes this state into the shared `AtomicU32` representation.
    #[inline]
    pub(crate) fn encode(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Pending(f) => f.to_f32() as u32 + 1,
        }
    }

    /// Decodes a raw `AtomicU32` value back into the typed state.
    #[inline]
    pub(crate) fn decode(raw: u32) -> Self {
        match raw {
            0 => Self::None,
            1 => Self::Pending(OversampleFactor::Off),
            2 => Self::Pending(OversampleFactor::X2),
            3 => Self::Pending(OversampleFactor::X4),
            _ => {
                debug_assert!(false, "invalid PendingRestartOs encoding: {raw}");
                Self::None
            }
        }
    }

    /// Loads and decodes the pending-restart state from `atomic`.
    #[inline]
    pub(crate) fn load(atomic: &AtomicU32, order: Ordering) -> Self {
        Self::decode(atomic.load(order))
    }

    /// Encodes and stores this state into `atomic`.
    #[inline]
    pub(crate) fn store(self, atomic: &AtomicU32, order: Ordering) {
        atomic.store(self.encode(), order);
    }

    /// Atomically consumes the pending-restart state (writes `None`),
    /// returning the previous state. Used by `activate()`.
    #[inline]
    pub(crate) fn take(atomic: &AtomicU32, order: Ordering) -> Self {
        Self::decode(atomic.swap(0, order))
    }
}

/// Main -> RT communication payload for the CLAP plugin.
pub enum ClapParamPayload {
    /// Parameter update (gain, gate, bypass).
    Params(RtProcessingParams),
    /// Loading of a new model pair (transferred/constructed outside RT) and its resampler.
    LoadModel {
        /// Monotonic model generation this load belongs to.
        /// Allocated on the main thread when the model identity is adopted and
        /// used by the audio thread to reject stale slimmable rebuilds.
        generation: u64,
        /// The encapsulated model for neural inference (Left Channel)
        model_l: Option<Box<StaticModel>>,
        /// Polyphase sinc resampler
        new_resampler: Box<NamResampler>,
        /// Streaming resample adapter with strict host cardinality,
        /// built on the main thread to match the model rate.
        new_stream: Box<neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer>,
        /// Model input gain calibration multiplier (from input_level_dbu metadata).
        input_mult_adj: f32,
        /// Model output gain calibration multiplier (from loudness metadata).
        output_mult_adj: f32,
    },
    /// Loading of a new cab-sim convolution adapter via SPSC.
    /// Follows the same pattern as `LoadModel`: the adapter is boxed and
    /// constructed outside the RT thread, then swapped atomically in the
    /// audio thread — the old `Box` moves by value to the GC.
    LoadCabIr {
        /// Pre-built, boxed convolution adapter (None = bypass cabsim).
        adapter: Option<Box<neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter>>,
    },
    /// Oversampling factor change: carries new pre-built engines for the audio thread.
    SetOversample {
        /// Pre-built left-channel oversample engine.
        os_l: Box<neural_amp_modeler_rs::dsp::oversample::OversampleEngine>,
        /// Pre-built right-channel oversample engine.
        os_r: Box<neural_amp_modeler_rs::dsp::oversample::OversampleEngine>,
    },
    /// A complete state restore delivered as ONE command.
    ///
    /// The audio thread applies the whole package (model, IR, params) atomically
    /// within a single block, so no observer ever sees a hybrid of two restores.
    /// UI/paths/hashes are published by the main thread only after the audio
    /// thread acks this transaction's sequence number.
    RestoreTxn(RestoreTxn),
}

/// Classification of a [`ClapParamPayload`] for Command Budgeting.
///
/// The SPSC drain loop distinguishes **light** parameter updates (unlimited per
/// callback, up to the queue-drain cap) from **structural** transactions that
/// swap heavy DSP resources (model, resampler, IR, oversample engines). A
/// structural apply recomputes latency, feeds the GC cascade and may call host
/// extensions (`HostTail::changed`), so at most one structural command is
/// applied per audio callback — the rest are deferred to the next callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructuralKind {
    /// Full model swap (model + resampler + streaming adapter).
    Model,
    /// Cab-sim IR swap (or clear).
    CabIr,
    /// Oversample engine rebuild (L+R).
    Oversample,
    /// Complete atomic state restore (model + IR + params).
    Restore,
}

impl StructuralKind {
    /// Whether a deferred command of this kind may be superseded by a newer
    /// same-kind command already queued in the ring (command coalescing).
    ///
    /// [`StructuralKind::Restore`] is deliberately **not** coalescible: a
    /// [`RestoreTxn`] is ack-gated by the main thread and must always be
    /// applied atomically as the exact package the host committed — skipping one
    /// would desynchronize UI/path/hash publication from the applied DSP state.
    pub const fn is_coalescible(self) -> bool {
        !matches!(self, StructuralKind::Restore)
    }
}

impl ClapParamPayload {
    /// Returns the [`StructuralKind`] for structural commands, or `None` for
    /// light parameter updates (`Params`). Used by the RT drain loop for
    /// Command Budgeting.
    pub const fn structural_kind(&self) -> Option<StructuralKind> {
        match self {
            ClapParamPayload::Params(_) => None,
            ClapParamPayload::LoadModel { .. } => Some(StructuralKind::Model),
            ClapParamPayload::LoadCabIr { .. } => Some(StructuralKind::CabIr),
            ClapParamPayload::SetOversample { .. } => Some(StructuralKind::Oversample),
            ClapParamPayload::RestoreTxn(_) => Some(StructuralKind::Restore),
        }
    }

    /// Whether this payload is a structural (heavy-swap) command as opposed to
    /// a light parameter update. Structural commands are budgeted to at most
    /// one per audio callback; light updates drain freely.
    pub const fn is_structural(&self) -> bool {
        !matches!(self, ClapParamPayload::Params(_))
    }
}

/// Model component of a [`RestoreTxn`] (or a standalone model load).
pub struct LoadModelPayload {
    /// Monotonic model generation this load belongs to.
    /// Allocated on the main thread when the model identity is adopted.
    pub generation: u64,
    /// The encapsulated model for neural inference (Left Channel).
    /// `None` explicitly clears the active model.
    pub model_l: Option<Box<StaticModel>>,
    /// Polyphase sinc resampler matching the model rate.
    pub new_resampler: Box<NamResampler>,
    /// Streaming resample adapter with strict host cardinality,
    /// matching the model rate and the host buffer size.
    pub new_stream: Box<neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer>,
    /// Model input gain calibration multiplier.
    pub input_mult_adj: f32,
    /// Model output gain calibration multiplier.
    pub output_mult_adj: f32,
}

/// A latency-affecting resource swap staged to land only on the next host
/// restart cycle (Strict Restart Policy / TR.1).
///
/// Built entirely on the main thread (off-RT) exactly like the SPSC payloads.
/// When a model/IR swap would change the *physical* latency applied by the DSP,
/// the plugin must not install it mid-stream: it requests `host.request_restart()`
/// and parks the fully-built resources here so `activate()` installs them during
/// the restart cycle (the DSP keeps running with the old, still-reported
/// latency until then). Swaps that keep the exact same latency bypass this slot
/// and apply continuously through the regular SPSC path.
///
/// Coalescing is latest-wins per component: a newer model load replaces
/// `model`, a newer IR load/clear replaces `ir`, and a continuous (same-latency)
/// swap clears its own component — the latest user intent always prevails
/// before the host restarts.
#[derive(Default)]
pub(crate) struct StagedSwap {
    /// Model component (`model_l: None` clears the model).
    pub(crate) model: Option<LoadModelPayload>,
    /// Cab-sim component (`None` clears the IR).
    pub(crate) ir: Option<Option<Box<CabSimAdapter>>>,
}

impl StagedSwap {
    /// Returns `true` when no component is pending.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.model.is_none() && self.ir.is_none()
    }

    /// Clears the model component (a same-latency model swap was applied
    /// continuously, superseding any staged model). Drops the superseded
    /// resources on the main thread.
    pub(crate) fn clear_model(&mut self) {
        self.model = None;
    }

    /// Clears the cab-sim component (a same-latency IR swap was applied
    /// continuously, superseding any staged IR). Drops the superseded
    /// resources on the main thread.
    pub(crate) fn clear_ir(&mut self) {
        self.ir = None;
    }
}

/// A single atomic state-restore transaction.
///
/// Carries the complete validated restore so the audio thread applies it as one
/// package in a single block. The `generation` tags the restore for traceability
/// and is stored in `ColdShared::last_applied_generation` once the package lands.
pub struct RestoreTxn {
    /// Monotonic generation tag identifying this restore.
    pub generation: u64,
    /// Model component: `Some(payload)` loads (or clears when `model_l` is None),
    /// `None` leaves the active model untouched (ForPreset without model).
    pub model: Option<LoadModelPayload>,
    /// IR component: `Some(Some(adapter))` loads, `Some(None)` clears,
    /// `None` leaves the active IR untouched (ForPreset without IR).
    /// The adapter travels `Box`ed end-to-end for RT-safe swaps.
    pub ir: Option<Option<Box<CabSimAdapter>>>,
    /// Full RT parameter snapshot applied atomically with the model/IR.
    pub params: RtProcessingParams,
}

/// Model publication data retained until the restore transaction is acked.
pub struct RestoreModelPublish {
    /// Model metadata for GUI display.
    pub metadata: NamModelMetadata,
    /// Dynamic model info for diagnostics.
    pub info: ModelInfo,
    /// Clone of full WaveNet weights for slimmable rebuild storage.
    pub full_wavenet: Option<Box<StaticModel>>,
    /// Model native sample rate.
    pub model_rate: u32,
}

/// UI/path/hash publication payload for a restore, applied only after the audio
/// thread acks the transaction (or immediately in a pre-activate local commit).
pub struct RestorePublish {
    /// Whether this restore was a `RestoreMode::Full` (drives the clear branches).
    pub mode_full: bool,
    /// Effective `ProcessingParams` that `main_thread.params` must adopt on publish.
    pub params: ProcessingParams,
    /// Model publication (None = no model loaded in this restore).
    pub model: Option<RestoreModelPublish>,
    /// Model path on disk to store in `params.model_path`.
    pub model_path_on_disk: Option<PathBuf>,
    /// Model basename for `ui_model_name` and portable identity.
    pub model_basename: Option<String>,
    /// Model SHA-256 hex digest.
    pub model_hash: Option<String>,
    /// Model search path hint to append to `params.model_search_paths`.
    pub model_search_path_to_add: Option<PathBuf>,
    /// IR path on disk for `ir_path` / state save.
    pub ir_path_on_disk: Option<String>,
    /// IR SHA-256 hex digest (mandatory for any persisted IR reference).
    pub ir_hash: Option<String>,
    /// IR raw samples for `ir_raw_samples` (adapter is rebuilt by `activate()`).
    pub ir_raw_samples: Option<Vec<f32>>,
    /// Sample rate of `ir_raw_samples`.
    pub ir_raw_sample_rate: u32,
}

/// A staged restore awaiting atomic delivery and ack-gated publication.
///
/// Held in `NamClapMainThread::pending_restore` (private main-thread slot).
/// `txn` is `Some` while not yet pushed (SPSC full retry) and `None` once pushed
/// and waiting for the audio-thread ack of `seq`.
pub struct PendingRestore {
    /// Monotonic generation tag identifying this restore.
    pub generation: u64,
    /// `Some` = not yet pushed (retain for retry); `None` = pushed, awaiting ack.
    pub txn: Option<RestoreTxn>,
    /// Publication payload applied only after the ack.
    pub publish: RestorePublish,
    /// Sequence number of the pushed transaction (0 = not yet pushed).
    pub seq: u64,
}

/// A validated restore transaction staged on the main thread to land only on
/// the next host restart cycle (Strict Restart Policy / TR.1 extended to
/// RestoreTxn).
///
/// Holds the entire atomic transaction and its publication payload together so
/// that no component is installed piecemeal before the host restart.
pub struct StagedRestore {
    /// The complete atomic restore transaction.
    pub txn: RestoreTxn,
    /// Publication payload to apply off-RT upon `activate()`.
    pub publish: RestorePublish,
}

/// Model metadata for display in the GUI.
#[derive(Clone, Debug, Default)]
pub struct NamModelMetadata {
    /// Model architecture (e.g. "LSTM", "WaveNet").
    pub architecture: String,
    /// Model topology (e.g. "Standard", "1x64").
    pub topology: String,
    /// Model native sample rate (Hz).
    pub sample_rate: u32,
    /// Author / Modeled by.
    pub modeled_by: Option<String>,
    /// Original equipment manufacturer.
    pub gear_make: Option<String>,
    /// Original equipment model.
    pub gear_model: Option<String>,
    /// Equipment type.
    pub gear_type: Option<String>,
    /// Style/Tone type of the equipment.
    pub tone_type: Option<String>,
    /// Date formatted as YYYY-MM-DD.
    pub date: Option<String>,
}

// ---------------------------------------------------------------------------
// Cache-line-isolated sub-structs grouped by access pattern
// ---------------------------------------------------------------------------

/// Fields written every block by the RT thread, read by the UI thread.
///
/// All fields in this struct are written exclusively by the audio thread
/// and read lock-free by the UI/main thread via `Ordering::Relaxed` (or
/// stronger when the field is part of a synchronization pair).
/// The UI thread must never write to any field in this struct.
#[repr(align(128))]
pub struct RtToUi {
    /// True Peak L level set by the audio thread (f32 bits via f32::to_bits()). Read by the UI thread.
    pub ui_peak_l: AtomicU32,
    /// True Peak R level set by the audio thread (f32 bits via f32::to_bits()). Read by the UI thread.
    pub ui_peak_r: AtomicU32,
    /// Flag indicating whether clipping has occurred since the last UI frame. Read/reset by the UI thread.
    pub ui_clipped: AtomicBool,
    /// Flag indicating whether output/input clipping is active for UI indicator display.
    pub ui_clip_indicator: AtomicBool,
    /// Flag indicating whether the noise gate is currently active (attenuating).
    pub ui_gate_active: AtomicBool,
    /// Current latency reported to the host (in samples).
    pub current_latency: AtomicU32,
    /// CabSim tail length in samples (= num_partitions × partition_size).
    /// Written by the audio thread after each IR swap; read by the main thread
    /// via `clap.tail`. Zero when no IR is loaded (passthrough mode).
    pub cabsim_tail_samples: AtomicU32,
    /// Number of active channels: 1 = mono, 2 = stereo.
    pub active_channel_count: AtomicU32,
}

/// Fields written by the UI/Main thread, read every block by the RT thread.
#[repr(align(128))]
pub struct UiToRt {
    /// Latest Input Gain parameter value (f32 as bits).
    pub param_input_gain: AtomicU32,
    /// Latest Output Gain parameter value (f32 as bits).
    pub param_output_gain: AtomicU32,
    /// Latest Gate Threshold parameter value (f32 as bits).
    pub param_gate_thresh: AtomicU32,
    /// Latest Bypass parameter value (0 = false, 1 = true).
    pub param_bypass: AtomicU32,
    /// Latest Adaptive Compute mode parameter value (0=Off, 1=Conservative, 2=Aggressive).
    pub param_adaptive_compute: AtomicU32,
    /// Latest Slim Override parameter value (0=Auto, 1=ForceFull, 2=ForceLite).
    pub param_slim_override: AtomicU32,
    /// Latest Oversampling factor parameter value (0=Off, 1=X2, 2=X4).
    pub param_oversample: AtomicU32,
    /// Latest Activation Precision parameter value (0=Fast, 1=Standard).
    pub param_activation: AtomicU32,
    /// Gesture and modification flag bitmap per parameter (GUI -> Host/Processor).
    pub gesture_flags: AtomicU32,
    /// Monotonic generation counter bumped (Release) by GUI on any param write.
    pub gui_param_generation: AtomicU32,
    /// True if the host has deactivated the R channel via audio-ports-activation.
    /// Written by Main Thread (or Audio Thread when processing), read every block by RT.
    pub host_r_deactivated: AtomicBool,
}

/// Deferred preset-load metadata for `HostPresetLoad` notification.
/// Stored by `PluginPresetLoadImpl::load_from_location()` and consumed by
/// `housekeeping()` to call `loaded()` or `on_error()` after the async load.
pub struct PendingPresetLoad {
    /// Owned copy of the file-system path from the `Location`.
    pub location_path: CString,
    /// Owned copy of the `load_key` (preset identifier from discovery provider).
    pub load_key: Option<CString>,
}

/// Fields accessed at low frequency by both threads (init, shutdown, rare events).
#[repr(align(128))]
pub struct ColdShared {
    /// Unique instance identifier for multi-instance telemetry and logging isolation.
    pub instance_id: u64,
    /// SPSC channel: Main Thread -> Audio Thread (New parameters/models).
    pub param_tx: Mutex<Option<Producer<ClapParamPayload>>>,
    /// SPSC channel: Main Thread -> Audio Thread (Consumer).
    pub param_rx: Mutex<Option<Consumer<ClapParamPayload>>>,
    /// GC channel: Audio Thread -> Main Thread (Obsolete models for disposal).
    pub gc_tx: Mutex<Option<Producer<GcItem>>>,
    /// GC channel: Audio Thread -> Main Thread (Consumer).
    pub gc_rx: Mutex<Option<Consumer<GcItem>>>,
    /// Fallback buffer for GC overflow (overwrite).
    pub gc_overflow: Arc<GcOverflowBuffer>,
    /// Atomic status flags (RT->Main telemetry).
    pub rt_status: Arc<RtStatusFlags>,
    /// Native sample rate required by the actively loaded model.
    pub model_sample_rate: AtomicU32,
    /// Detected host sample rate.
    pub sample_rate: AtomicU32,
    /// Host buffer size.
    pub buffer_size: AtomicU32,
    /// Latency contribution (host-rate samples) of the streaming resample
    /// adapter **currently installed** on the audio thread.
    ///
    /// Written by the audio thread (Relaxed) at the exact instant a stream is
    /// installed — `activate()` and `cold_load_model()` — and read by the main
    /// thread during `load_model()` to decide whether a model swap changes the
    /// physical latency (Strict Restart Policy: same latency ⇒ continuous swap, different
    /// latency ⇒ staged + `request_restart()`).
    pub current_stream_latency: AtomicU32,
    /// Latency contribution (host-rate samples) of the cab-sim convolution
    /// adapter **currently installed** on the audio thread.
    /// `0` when no IR is loaded.
    ///
    /// Written by the audio thread (Relaxed) at the exact instant an adapter is
    /// installed — `activate()` and `cold_load_cabsim()` — and read by the main
    /// thread during `load_cabsim()`/IR-clear to decide whether the swap
    /// changes the physical latency.
    pub current_cabsim_latency: AtomicU32,
    /// Dynamic accent color based on DAW track color (packed ARGB).
    pub track_accent_color: AtomicU32,
    /// Parameter indication (mapping, automation, and override) for the 9 parameters.
    /// Bit 0: Mapped, Bit 1: Automating, Bit 2: Override.
    pub param_indication: [AtomicU8; 9],
    /// Indicated/mapped parameter colors (packed ARGB).
    pub param_indication_color: [AtomicU32; 9],
    /// Model load counter (incremented on each successful model load).
    pub model_load_counter: AtomicU32,
    /// Monotonic model-generation allocator.
    /// Written only by the main thread via [`ColdShared::allocate_model_generation`]
    /// each time a new model identity is adopted (load, restore-with-model, or
    /// clear). The returned value tags the payload that installs that model so
    /// the audio thread can reject a stale slimmable rebuild. `0` is reserved
    /// for "no model installed yet".
    pub model_generation: AtomicU64,
    /// Loaded model name (path basename). Written by the main thread, read by the UI thread.
    pub ui_model_name: Mutex<String>,
    /// Loaded model metadata for UI display.
    pub ui_model_metadata: Mutex<Option<NamModelMetadata>>,
    /// Pending model path to be loaded by the Main Thread. Written by the UI thread.
    pub ui_pending_model: Mutex<Option<PathBuf>>,
    /// Indicates whether the GUI is in the middle of an asynchronous model load.
    pub ui_loading: AtomicBool,
    /// Flag signaling that a model loading error occurred.
    pub ui_load_error: AtomicBool,
    /// Detailed error message for the GUI.
    pub ui_load_error_msg: Mutex<String>,
    /// Dynamic model info for diagnostics.
    pub ui_model_info: Mutex<Option<neural_amp_modeler_rs::common::diagnostics::ModelInfo>>,
    /// Flag signaling that the model should be cleared (unload model).
    pub ui_clear_model: AtomicBool,
    /// Lifetime fence: true while the plugin exists. Checked by the File Picker thread.
    pub alive_fence: Arc<AtomicBool>,
    /// Backend signal: the user closed the GUI window (WM close button, Alt-F4).
    ///
    /// Set by the Slint window's `on_close_requested` callback on the GUI
    /// thread (Release) and consumed by `housekeeping()` on the main thread
    /// (Acquire) to drive `GuiEvent::UserClosed` into the lifecycle FSM — the
    /// GUI thread cannot mutate `NamClapMainThread::gui_lifecycle` directly.
    pub gui_user_closed: AtomicBool,
    /// Render mode as set by the host via `clap.render`: 0 = Realtime, 1 = Offline.
    /// Written by the Main Thread, read by the RT thread at low frequency (transitions only).
    pub render_mode: AtomicU32,
    /// Current GUI scale factor (f32 bits). Written by `gui.set_scale` on the Main Thread,
    /// read by the window handler for HiDPI rendering before the first Resized event.
    pub gui_scale_factor: AtomicU32,
    /// Active cab-sim IR file path (for state save/load and GUI display).
    pub ir_path: Mutex<Option<String>>,
    /// SHA-256 hex digest of the active cab-sim IR file — kept in lockstep with
    /// `ir_path` so persisted state always carries the digest.
    pub ir_hash: Mutex<Option<String>>,
    /// Pending IR path to be loaded by the Main Thread. Written by the UI thread.
    pub ui_pending_ir: Mutex<Option<PathBuf>>,
    /// Indicates whether the GUI is in the middle of an asynchronous IR load.
    pub ui_ir_loading: AtomicBool,
    /// Flag signaling that an IR loading error occurred.
    pub ui_ir_load_error: AtomicBool,
    /// Detailed error message for the GUI.
    pub ui_ir_load_error_msg: Mutex<String>,
    /// Flag signaling that the IR should be cleared (bypass cabsim).
    pub ui_clear_ir: AtomicBool,
    /// Raw IR samples stored for adaptive partition rebuild without WAV reload.
    pub ir_raw_samples: Mutex<Option<Vec<f32>>>,
    /// Sample rate of the stored `ir_raw_samples` — tracks the host rate
    /// at which the IR was last loaded. When the host rate changes during
    /// re-activation, the samples are resampled to the new rate.
    pub ir_raw_sample_rate: AtomicU32,
    /// SPSC channel: Main Thread -> Audio Thread (Slimmable model producer).
    pub slimmable_tx: Mutex<Option<Producer<SlimmableRebuild>>>,
    /// SPSC channel: Main Thread -> Audio Thread (Slimmable model consumer).
    pub slimmable_rx: Mutex<Option<Consumer<SlimmableRebuild>>>,
    /// Slimmable rebuild request payload: the monotonic model generation the
    /// audio thread was running when it requested the rebuild. Written by the audio thread (Relaxed, ordered by the
    /// `RT_STATUS_NEEDS_SLIMMABLE_REBUILD` Release flag), read by the main
    /// thread (after Acquire on the flag) to tag the rebuilt delivery.
    pub requested_slimmable_generation: AtomicU64,
    /// Telemetry: total slimmable rebuilds discarded by the audio thread
    /// because their generation was stale. Written by the
    /// audio thread (Relaxed); read by tests/housekeeping for observability.
    pub slimmable_stale_discarded_total: AtomicU32,
    /// Full WaveNet model weights stored for main-thread slimmable rebuild.
    /// When a WaveNet model is loaded, a clone is stored here so the main thread
    /// can create slimmed variants without touching the audio thread.
    pub full_wavenet_model: Mutex<Option<Box<StaticModel>>>,
    /// Command scheduler ack atomics.
    /// Monotonic sequence counter incremented by the main thread on each
    /// enqueued command batch.
    pub cmd_next_seq: AtomicU64,
    /// Last sequence fully drained and processed by the audio thread.
    /// Written by the audio thread (Release), read by the main thread (Acquire).
    pub cmd_last_ack: AtomicU64,
    /// Generation of the most recent restore transaction applied atomically by
    /// the audio thread. Written by the audio thread (Relaxed) after applying a
    /// [`RestoreTxn`]; read by tests/main thread to verify which restore is live.
    pub last_applied_generation: AtomicU64,
    /// Pending restart oversampling request.
    /// Set by the audio thread (via `host.request_restart()`) when
    /// oversampling changes during active processing. Consumed by
    /// `activate()` to build engines at the correct factor. Stored with the
    /// [`PendingRestartOs`] encoding — `0` means *no pending restart* and a
    /// pending transition **to Off** is representable (`1`), so the raw value
    /// no longer doubles as the Off factor.
    pub pending_restart_os_factor: AtomicU32,
    /// In-flight parameter snapshot.
    /// When the main-thread `flush()` fails to push params to the SPSC
    /// (channel full), the snapshot is stored here for retry via
    /// `host.request_callback()` → `housekeeping()`.
    pub in_flight_params: Mutex<Option<neural_amp_modeler_rs::common::params::RtProcessingParams>>,
    /// Pending preset-load operations (bounded FIFO queue).
    /// Enqueued by `load_from_location()` with the location and load_key for
    /// deferred host notification. Consumed by `housekeeping()` in FIFO order to call
    /// `HostPresetLoad::loaded()` or `on_error()` after async loads.
    pub pending_preset_load: Mutex<std::collections::VecDeque<PendingPresetLoad>>,
    /// Model loaded before `activate()` (state restore while `buffer_size == 0`),
    /// deferred to avoid heap allocation on the audio thread.
    /// See `flush_pending_model()` in load.rs and housekeeping.rs.
    pub pending_model: Mutex<Option<PendingModel>>,
    /// Heavy DSP resources preserved across deactivate/activate cycles.
    /// When the host deactivates a plugin instance, model weights, resampler,
    /// oversampling engines, and convolution partitions are moved here instead
    /// of being dropped. On the next `activate()`, they are reinstalled
    /// deterministically — avoiding I/O and recompute. See [`DeactivatedDspState`].
    pub(crate) deactivated_dsp: Mutex<Option<DeactivatedDspState>>,
    /// Dialog state for model file-dialog (Arc-backed, UAF-safe). Initialized on main thread.
    pub(crate) dialog_state: Option<Arc<crate::clap::gui::dialog_state::DialogSharedState>>,
    /// Dialog state for IR file-dialog (Arc-backed, UAF-safe). Initialized on main thread.
    pub(crate) ir_dialog_state: Option<Arc<crate::clap::gui::dialog_state::IrDialogSharedState>>,
    /// Sink for model dialog thread handle (written by UI, read by main thread for join).
    pub(crate) dialog_handle_sink: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Sink for IR dialog thread handle (written by UI, read by main thread for join).
    pub(crate) ir_dialog_handle_sink: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Host-log bridge sink for forwarding `log::*` macros to the DAW host.
    /// Each CLAP instance registers a `Weak<HostLogFn>` with the global `NamLogger`.
    /// This `Arc` is stored here (owned by the plugin) so the `Weak` stays alive
    /// for the plugin's lifetime.
    pub(crate) host_log_sink:
        Mutex<Option<Arc<neural_amp_modeler_rs::common::diagnostics::logger::HostLogFn>>>,
}

/// Model payload deferred from the main thread until `buffer_size` is known
/// (state-restore-before-activate scenario). Carries the model and enough
/// metadata for `flush_pending_model()` to construct the resampler with the
/// correct host sample rate and buffer capacity — both unknown during pre-activation
/// state restore.
pub struct PendingModel {
    /// Monotonic model generation this model was adopted with.
    /// Preserved through the deferral so the deferred model keeps its identity.
    pub generation: u64,
    /// The encapsulated model for neural inference (Left Channel).
    pub model: Option<Box<StaticModel>>,
    /// Model native sample rate read from the .nam file metadata.
    /// Used to construct the polyphase resampler at flush time when
    /// `activate()` has set the real host `sample_rate`.
    pub model_rate: u32,
    /// Model input gain calibration multiplier.
    pub input_mult_adj: f32,
    /// Model output gain calibration multiplier.
    pub output_mult_adj: f32,
}

/// A slimmable-rebuilt model delivered from the main thread to the audio
/// thread, tagged with the model generation it was sliced from.
///
/// The audio thread installs the rebuilt model only when `generation` matches
/// the generation of the model it is currently running; a stale rebuild
/// (produced from a model that was swapped out in the meantime) is discarded
/// straight to the GC without touching the active DSP state.
pub struct SlimmableRebuild {
    /// Monotonic generation of the active model at the time the rebuild was
    /// requested (echoed from the audio-thread request payload).
    pub generation: u64,
    /// The rebuilt (sliced + prewarmed + resized) model, boxed for RT-safe
    /// swap and GC disposal.
    pub model: Box<StaticModel>,
}

// ---------------------------------------------------------------------------
// Outer shared struct
// ---------------------------------------------------------------------------

/// Lock-free shared state between the audio thread, main thread, and GUI window.
///
/// Fields are segregated into cache-line-isolated sub-structs grouped by
/// access pattern to eliminate False Sharing.  Each sub-struct has its own
/// `#[repr(align(128))]` so that no two sub-structs share a 128-byte cache
/// line, preventing cache-line bouncing between RT↔UI hotpath writes/reads.
///
/// SPSC channels are wrapped in Mutex<Option<...>> only to allow
/// them to be "extracted" by their respective threads during initialization,
/// satisfying the `Sync` requirement of the `PluginShared` trait.
///
/// - `rt_to_ui`: written every block by RT, read by UI.
/// - `ui_to_rt`: written by UI/Main, read every block by RT.
/// - `cold`: low-frequency access by both threads.
pub struct GuiSharedState {
    /// Cache-line-isolated sub-struct: written every block by RT, read by UI.
    pub rt_to_ui: RtToUi,
    /// Cache-line-isolated sub-struct: written by UI/Main, read every block by RT.
    pub ui_to_rt: UiToRt,
    /// Cache-line-isolated sub-struct: low-frequency access by both threads.
    pub cold: ColdShared,
}

/// Shared state handle for the CLAP plugin lifecycle (`clack_plugin::Plugin::Shared`).
///
/// Owns the underlying [`GuiSharedState`] via an [`Arc`], enabling safe reference-counted
/// ownership across GUI windows and background reaper threads without dangling pointers.
pub struct NamClapShared {
    /// Inner ref-counted GUI and plugin shared state.
    pub gui: Arc<GuiSharedState>,
}

impl<'a> PluginShared<'a> for NamClapShared {}

impl std::ops::Deref for NamClapShared {
    type Target = GuiSharedState;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.gui
    }
}

impl Drop for NamClapShared {
    fn drop(&mut self) {
        log::debug!("NAM-Plug: NamClapShared dropped.");
        self.gui.cold.alive_fence.store(false, Ordering::Release); // pairs with Acquire load
        // Only signal shutdown when the last instance is destroyed.
        let prev = ACTIVE_INSTANCES
            .fetch_update(Ordering::Release, Ordering::Relaxed, |val| {
                Some(val.saturating_sub(1))
            })
            .unwrap_or(0);
        let remaining = prev.saturating_sub(1);
        if prev > 0 && remaining == 0 {
            neural_amp_modeler_rs::common::panic_hook::set_shutdown_in_progress();
        }
    }
}

/// Render mode constants for `ColdShared::render_mode`.
/// Render mode: realtime (normal processing).
pub const RENDER_MODE_REALTIME: u32 = 0;
/// Render mode: offline (export/bounce, max quality, no soft-degrade).
pub const RENDER_MODE_OFFLINE: u32 = 1;

impl ColdShared {
    /// Allocates the next monotonic model generation (1-based) and returns it.
    ///
    /// Called only on the main thread whenever a new model identity is adopted
    /// — a model load, a restore carrying a model, or an explicit clear. The
    /// returned value tags the payload that installs that model so the audio
    /// thread can reject a stale slimmable rebuild.
    /// `0` is reserved for "no model installed yet".
    #[inline]
    pub(crate) fn allocate_model_generation(&self) -> u64 {
        self.model_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
    }
}

/// Bitmask constants for the parameter in the `gesture_flags` field.
pub const GESTURE_CHANGED_SHIFT: u32 = 0;
pub const GESTURE_BEGIN_SHIFT: u32 = 1;
pub const GESTURE_END_SHIFT: u32 = 2;
pub const GESTURE_BITS_PER_PARAM: u32 = 3;

impl GuiSharedState {
    /// Bitmask for the parameter in the `gesture_flags` field.
    /// 3 flags per parameter: Changed, GestureBegin, GestureEnd.
    pub const GESTURE_CHANGED_SHIFT: u32 = GESTURE_CHANGED_SHIFT;
    pub const GESTURE_BEGIN_SHIFT: u32 = GESTURE_BEGIN_SHIFT;
    pub const GESTURE_END_SHIFT: u32 = GESTURE_END_SHIFT;
    pub const GESTURE_BITS_PER_PARAM: u32 = GESTURE_BITS_PER_PARAM;

    /// Maps a CLAP param_id (0..8) to internal index 0..8.
    pub const fn param_index(param_id: u32) -> usize {
        param_id as usize
    }

    /// Sets a begin gesture flag for the given CLAP param_id.
    pub fn begin_gesture(&self, param_id: u32) {
        self.set_gesture(Self::param_index(param_id), Self::GESTURE_BEGIN_SHIFT);
    }

    /// Sets an end gesture flag for the given CLAP param_id.
    pub fn end_gesture(&self, param_id: u32) {
        self.set_gesture(Self::param_index(param_id), Self::GESTURE_END_SHIFT);
    }

    /// Sets a changed flag for the given CLAP param_id.
    pub fn mark_param_changed(&self, param_id: u32) {
        self.set_gesture(Self::param_index(param_id), Self::GESTURE_CHANGED_SHIFT);
    }

    /// Sets a gesture flag for the parameter (store = true).
    pub fn set_gesture(&self, param_index: usize, flag_shift: u32) {
        let bit = 1u32 << (param_index as u32 * Self::GESTURE_BITS_PER_PARAM + flag_shift);
        self.ui_to_rt.gesture_flags.fetch_or(bit, Ordering::Relaxed);
    }

    /// Reads and clears a gesture flag (swap to false), returns the previous value.
    pub fn take_gesture(&self, param_index: usize, flag_shift: u32) -> bool {
        let bit = 1u32 << (param_index as u32 * Self::GESTURE_BITS_PER_PARAM + flag_shift);
        (self
            .ui_to_rt
            .gesture_flags
            .fetch_and(!bit, Ordering::Relaxed)
            & bit)
            != 0
    }

    /// Zeros out all gesture flags.
    pub fn clear_gestures(&self) {
        self.ui_to_rt.gesture_flags.store(0, Ordering::Relaxed);
    }

    /// Bumps the GUI parameter generation counter (Release ordering).
    /// Signals to the RT thread that GUI-initiated parameter values may have changed.
    pub fn bump_generation(&self) {
        self.ui_to_rt
            .gui_param_generation
            .fetch_add(1, Ordering::Release); // pairs with Acquire loads in processor/events.rs, extensions/params/audio.rs
    }

    /// Returns whether the model file dialog is currently active.
    pub fn is_model_dialog_active(&self) -> bool {
        self.cold
            .dialog_state
            .as_ref()
            .map(|d| d.active.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// Sets whether the model file dialog is currently active.
    pub fn set_model_dialog_active(&self, active: bool) {
        if let Some(d) = self.cold.dialog_state.as_ref() {
            d.active.store(active, Ordering::Relaxed);
        }
    }

    /// Test helper creating an `Arc<GuiSharedState>` initialized with valid dummy channels
    /// and dialog states for unit and integration testing.
    pub fn new_test() -> Arc<Self> {
        use crate::clap::gui::dialog_state::{DialogSharedState, IrDialogSharedState};
        use neural_amp_modeler_rs::common::spsc::{GcOverflowBuffer, RtStatusFlags};
        use rtrb::RingBuffer;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64};

        let (param_tx, param_rx) = RingBuffer::new(8);
        let (gc_tx, gc_rx) = RingBuffer::new(32);
        let (slimmable_tx, slimmable_rx) = RingBuffer::new(4);

        Arc::new(Self {
            rt_to_ui: RtToUi {
                ui_peak_l: AtomicU32::new(0.0f32.to_bits()),
                ui_peak_r: AtomicU32::new(0.0f32.to_bits()),
                ui_clipped: AtomicBool::new(false),
                ui_clip_indicator: AtomicBool::new(false),
                ui_gate_active: AtomicBool::new(false),
                current_latency: AtomicU32::new(0),
                cabsim_tail_samples: AtomicU32::new(0),
                active_channel_count: AtomicU32::new(1),
            },
            ui_to_rt: UiToRt {
                param_input_gain: AtomicU32::new(0.0f32.to_bits()),
                param_output_gain: AtomicU32::new(0.0f32.to_bits()),
                param_gate_thresh: AtomicU32::new((-90.0f32).to_bits()),
                param_bypass: AtomicU32::new(0),
                param_adaptive_compute: AtomicU32::new(1),
                param_slim_override: AtomicU32::new(0),
                param_oversample: AtomicU32::new(0),
                param_activation: AtomicU32::new(1), // Standard (exact-grade)
                gesture_flags: AtomicU32::new(0),
                gui_param_generation: AtomicU32::new(0),
                host_r_deactivated: AtomicBool::new(false),
            },
            cold: ColdShared {
                instance_id: 1,
                param_tx: Mutex::new(Some(param_tx)),
                param_rx: Mutex::new(Some(param_rx)),
                gc_tx: Mutex::new(Some(gc_tx)),
                gc_rx: Mutex::new(Some(gc_rx)),
                gc_overflow: Arc::new(GcOverflowBuffer::new(
                    neural_amp_modeler_rs::common::spsc::SPSC_CAPACITY,
                )),
                rt_status: Arc::new(RtStatusFlags::new()),
                model_sample_rate: AtomicU32::new(48000),
                sample_rate: AtomicU32::new(44100),
                buffer_size: AtomicU32::new(0),
                current_stream_latency: AtomicU32::new(0),
                current_cabsim_latency: AtomicU32::new(0),
                track_accent_color: AtomicU32::new(0),
                param_indication: [
                    AtomicU8::new(0),
                    AtomicU8::new(0),
                    AtomicU8::new(0),
                    AtomicU8::new(0),
                    AtomicU8::new(0),
                    AtomicU8::new(0),
                    AtomicU8::new(0),
                    AtomicU8::new(0),
                    AtomicU8::new(0),
                ],
                param_indication_color: [
                    AtomicU32::new(0),
                    AtomicU32::new(0),
                    AtomicU32::new(0),
                    AtomicU32::new(0),
                    AtomicU32::new(0),
                    AtomicU32::new(0),
                    AtomicU32::new(0),
                    AtomicU32::new(0),
                    AtomicU32::new(0),
                ],
                model_load_counter: AtomicU32::new(0),
                model_generation: AtomicU64::new(0),
                ui_model_name: Mutex::new(String::new()),
                ui_model_metadata: Mutex::new(None),
                ui_pending_model: Mutex::new(None),
                ui_loading: AtomicBool::new(false),
                ui_load_error: AtomicBool::new(false),
                ui_load_error_msg: Mutex::new(String::new()),
                ui_model_info: Mutex::new(None),
                alive_fence: Arc::new(AtomicBool::new(true)),
                gui_user_closed: AtomicBool::new(false),
                render_mode: AtomicU32::new(RENDER_MODE_REALTIME),
                gui_scale_factor: AtomicU32::new(0),
                ir_path: Mutex::new(None),
                ir_hash: Mutex::new(None),
                ui_pending_ir: Mutex::new(None),
                ui_ir_loading: AtomicBool::new(false),
                ui_ir_load_error: AtomicBool::new(false),
                ui_ir_load_error_msg: Mutex::new(String::new()),
                ui_clear_ir: AtomicBool::new(false),
                ui_clear_model: AtomicBool::new(false),
                ir_raw_samples: Mutex::new(None),
                ir_raw_sample_rate: AtomicU32::new(0),
                slimmable_tx: Mutex::new(Some(slimmable_tx)),
                slimmable_rx: Mutex::new(Some(slimmable_rx)),
                requested_slimmable_generation: AtomicU64::new(0),
                slimmable_stale_discarded_total: AtomicU32::new(0),
                full_wavenet_model: Mutex::new(None),
                cmd_next_seq: AtomicU64::new(0),
                cmd_last_ack: AtomicU64::new(0),
                last_applied_generation: AtomicU64::new(0),
                pending_restart_os_factor: AtomicU32::new(0),
                in_flight_params: Mutex::new(None),
                pending_preset_load: Mutex::new(std::collections::VecDeque::new()),
                pending_model: Mutex::new(None),
                deactivated_dsp: Mutex::new(None),
                dialog_state: Some(Arc::new(DialogSharedState::new())),
                ir_dialog_state: Some(Arc::new(IrDialogSharedState::new())),
                dialog_handle_sink: Mutex::new(None),
                ir_dialog_handle_sink: Mutex::new(None),
                host_log_sink: Mutex::new(None),
            },
        })
    }

    /// Flushes gestures and parameter updates initiated by the GUI
    /// into the host's output event queue (fail-closed).
    ///
    /// # Fail-closed contract (T6.3)
    ///
    /// If `output.try_push` fails (host output queue full), the corresponding
    /// gesture bit is **retained** for retry on the next `flush`/`process`
    /// instead of being silently consumed. Draining stops at the first failure
    /// so the legal order `begin → value → end` is preserved for every
    /// parameter: an `end` is never emitted before its `begin` (or `value`).
    /// While the queue is full, repeated value writes for the same `param_id`
    /// coalesce into the latest atomic value (read at retry time); `begin`/`end`
    /// never coalesce. Every failure raises
    /// `RT_STATUS_GUI_EVENT_BACKPRESSURE` so the saturation is observable
    /// off-RT (housekeeping logs it) — never an invisible loss.
    pub fn write_gui_events(&self, output: &mut OutputEvents) {
        use crate::clap::extensions::params::{
            PARAM_ACTIVATION, PARAM_ADAPTIVE_COMPUTE, PARAM_BYPASS, PARAM_GATE_THRESH,
            PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN, PARAM_OVERSAMPLE, PARAM_SLIM_OVERRIDE,
        };
        use clack_plugin::events::event_types::{
            ParamGestureBeginEvent, ParamGestureEndEvent, ParamValueEvent,
        };

        let params: [(u32, u32, &AtomicU32); 8] = [
            (
                PARAM_INPUT_GAIN,
                Self::param_index(PARAM_INPUT_GAIN) as u32,
                &self.ui_to_rt.param_input_gain,
            ),
            (
                PARAM_OUTPUT_GAIN,
                Self::param_index(PARAM_OUTPUT_GAIN) as u32,
                &self.ui_to_rt.param_output_gain,
            ),
            (
                PARAM_GATE_THRESH,
                Self::param_index(PARAM_GATE_THRESH) as u32,
                &self.ui_to_rt.param_gate_thresh,
            ),
            (
                PARAM_BYPASS,
                Self::param_index(PARAM_BYPASS) as u32,
                &self.ui_to_rt.param_bypass,
            ),
            (
                PARAM_ADAPTIVE_COMPUTE,
                Self::param_index(PARAM_ADAPTIVE_COMPUTE) as u32,
                &self.ui_to_rt.param_adaptive_compute,
            ),
            (
                PARAM_SLIM_OVERRIDE,
                Self::param_index(PARAM_SLIM_OVERRIDE) as u32,
                &self.ui_to_rt.param_slim_override,
            ),
            (
                PARAM_OVERSAMPLE,
                Self::param_index(PARAM_OVERSAMPLE) as u32,
                &self.ui_to_rt.param_oversample,
            ),
            (
                PARAM_ACTIVATION,
                Self::param_index(PARAM_ACTIVATION) as u32,
                &self.ui_to_rt.param_activation,
            ),
        ];

        for (param_id, param_idx, value_atomic) in &params {
            let pi = *param_idx as usize;

            if self.take_gesture(pi, Self::GESTURE_BEGIN_SHIFT) {
                let ev = ParamGestureBeginEvent::new(0, ClapId::new(*param_id));
                if output.try_push(ev).is_err() {
                    self.set_gesture(pi, Self::GESTURE_BEGIN_SHIFT);
                    self.note_gui_backpressure();
                    return;
                }
            }
            if self.take_gesture(pi, Self::GESTURE_CHANGED_SHIFT) {
                // Value is re-read from the atomic on retry, so the latest GUI
                // value wins while the queue is full (coalescing).
                let val = f32::from_bits(value_atomic.load(Ordering::Relaxed)) as f64;
                let ev = ParamValueEvent::new(
                    0,
                    ClapId::new(*param_id),
                    clack_plugin::events::Pckn::new(0u8, 0u8, 0u8, 0u8),
                    val,
                    clack_plugin::utils::Cookie::empty(),
                );
                if output.try_push(ev).is_err() {
                    self.set_gesture(pi, Self::GESTURE_CHANGED_SHIFT);
                    self.note_gui_backpressure();
                    return;
                }
            }
            if self.take_gesture(pi, Self::GESTURE_END_SHIFT) {
                let ev = ParamGestureEndEvent::new(0, ClapId::new(*param_id));
                if output.try_push(ev).is_err() {
                    self.set_gesture(pi, Self::GESTURE_END_SHIFT);
                    self.note_gui_backpressure();
                    return;
                }
            }
        }
    }

    /// Records GUI→host output backpressure as an RT-safe telemetry flag.
    /// Consumed and logged off-RT by `emit_pending_logs()` (housekeeping).
    #[inline(always)]
    fn note_gui_backpressure(&self) {
        self.cold
            .rt_status
            .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GUI_EVENT_BACKPRESSURE);
    }
}

impl neural_amp_modeler_rs::common::diagnostics::HasRuntimeSnapshot for GuiSharedState {
    fn model_info(&self) -> Option<neural_amp_modeler_rs::common::diagnostics::ModelInfo> {
        if let Ok(info_guard) = self.cold.ui_model_info.lock() {
            info_guard.clone()
        } else {
            None
        }
    }

    fn audio_info(
        &self,
        consumer: &neural_amp_modeler_rs::common::diagnostics::AudioMetadata,
    ) -> neural_amp_modeler_rs::common::diagnostics::AudioInfo {
        let sr = self.cold.sample_rate.load(Ordering::Relaxed);
        let buffer_size = self.cold.buffer_size.load(Ordering::Relaxed) as usize;
        neural_amp_modeler_rs::common::diagnostics::AudioInfo {
            sample_rate: sr,
            buffer_size,
            channel_count: consumer.channel_count,
            host_name: consumer.host_name.clone(),
        }
    }

    fn rt_info(&self) -> neural_amp_modeler_rs::common::diagnostics::RtInfo {
        self.cold.rt_status.rt_info()
    }

    fn telemetry_snapshot(&self) -> neural_amp_modeler_rs::common::diagnostics::TelemetrySnapshot {
        self.cold.rt_status.telemetry_snapshot()
    }

    fn flags_seen(&self) -> u64 {
        self.cold.rt_status.flags_seen()
    }
}

impl neural_amp_modeler_rs::common::diagnostics::HasRuntimeSnapshot for NamClapShared {
    fn model_info(&self) -> Option<neural_amp_modeler_rs::common::diagnostics::ModelInfo> {
        self.gui.model_info()
    }

    fn audio_info(
        &self,
        consumer: &neural_amp_modeler_rs::common::diagnostics::AudioMetadata,
    ) -> neural_amp_modeler_rs::common::diagnostics::AudioInfo {
        self.gui.audio_info(consumer)
    }

    fn rt_info(&self) -> neural_amp_modeler_rs::common::diagnostics::RtInfo {
        self.gui.rt_info()
    }

    fn telemetry_snapshot(&self) -> neural_amp_modeler_rs::common::diagnostics::TelemetrySnapshot {
        self.gui.telemetry_snapshot()
    }

    fn flags_seen(&self) -> u64 {
        self.gui.flags_seen()
    }
}

#[cfg(test)]
#[path = "shared_test.rs"]
mod shared_test;

#[cfg(test)]
pub(crate) use shared_test::make_test_shared;

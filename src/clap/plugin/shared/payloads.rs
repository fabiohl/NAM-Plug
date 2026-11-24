// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Payload types, restore transactions, and request envelopes exchanged between
//! the main thread, GUI thread, and real-time audio thread.

use crate::clap::extensions::cabinet::{CabAssetKind, CabSlotDsp, CabUiMeta};
use crate::clap::extensions::calibration::{CalibrationConfig, ModelCalibrationMeta};
use crate::clap::extensions::gate_mode::GateMode;
use neural_amp_modeler_rs::common::diagnostics::ModelInfo;
use neural_amp_modeler_rs::common::params::{ProcessingParams, RtProcessingParams};
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::models::StaticModel;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

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
    /// Amp model input calibration & output mode update (`Calibrate Input`, dBu level, `Output Mode`).
    Calibration(CalibrationConfig),
    /// Noise gate mode update (`Silence` legacy vs `Musical` expander).
    GateMode(GateMode),
    /// Independent cabinet slot bypass update (`true` = bypassed, `false` = active).
    CabBypass(bool),
    /// Loading of a new model pair (transferred/constructed outside RT) and its resampler.
    LoadModel {
        /// Monotonic model generation this load belongs to.
        /// Allocated on the main thread when the model identity is adopted and
        /// used by the audio thread to reject stale slimmable rebuilds.
        generation: u64,
        /// The encapsulated model for neural inference (Left Channel)
        model_l: Option<Box<StaticModel>>,
        /// The encapsulated model for neural inference (Right Channel in dual-mono mode)
        model_r: Option<Box<StaticModel>>,
        /// Polyphase sinc resampler
        new_resampler: Box<NamResampler>,
        /// Streaming resample adapter with strict host cardinality,
        /// built on the main thread to match the model rate.
        new_stream: Box<neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer>,
        /// Native sample rate of the model in this payload.
        model_rate: u32,
        /// Model input gain calibration multiplier (from input_level_dbu metadata).
        input_mult_adj: f32,
        /// Model output gain calibration multiplier (from loudness metadata).
        output_mult_adj: f32,
        /// Amp model calibration metadata (`input_level_dbu`, `output_level_dbu`, `loudness`).
        cal_meta: ModelCalibrationMeta,
        /// Off-RT measured per-sample inference cost (ns) of `model_l`, seeded
        /// by the loader from the engine's cost calibration (U-02). `None`
        /// selects the conservative per-family prewarm table.
        prewarm_cost_ns: Option<f32>,
    },
    /// Loading of a new cabinet slot asset (WAV IR or 1x1 NAM model) via SPSC.
    /// Constructed off-RT, swapped atomically on the audio thread, and retired
    /// via `GcItem` without dropping heap memory on RT.
    LoadCabIr {
        /// Pre-built cabinet slot asset (`None` = clear cabinet slot).
        pair: Option<CabSlotDsp>,
    },
    /// Off-RT rebuilt and prewarmed neural cabinet slot (`CabNamSlot`),
    /// pushed after RT re-engagement or processor reset without running
    /// `prewarm_reset`/`prewarm_step` on the audio thread.
    RebuildCabNam {
        /// Generation counter requested by the audio thread; mismatched or
        /// superseded deliveries are retired to GC cleanly without swapping.
        generation: u64,
        /// Freshly constructed and converged neural cabinet slot.
        slot: CabSlotDsp,
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
    /// Off-RT prewarmed neural cabinet rebuild (coalescible with itself only,
    /// never discarding an explicit `LoadCabIr` asset swap).
    CabNamRebuild,
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
            ClapParamPayload::Params(_)
            | ClapParamPayload::Calibration(_)
            | ClapParamPayload::GateMode(_)
            | ClapParamPayload::CabBypass(_) => None,
            ClapParamPayload::LoadModel { .. } => Some(StructuralKind::Model),
            ClapParamPayload::LoadCabIr { .. } => Some(StructuralKind::CabIr),
            ClapParamPayload::RebuildCabNam { .. } => Some(StructuralKind::CabNamRebuild),
            ClapParamPayload::SetOversample { .. } => Some(StructuralKind::Oversample),
            ClapParamPayload::RestoreTxn(_) => Some(StructuralKind::Restore),
        }
    }

    /// Whether this payload is a structural (heavy-swap) command as opposed to
    /// a light parameter update. Structural commands are budgeted to at most
    /// one per audio callback; light updates drain freely.
    pub const fn is_structural(&self) -> bool {
        !matches!(
            self,
            ClapParamPayload::Params(_)
                | ClapParamPayload::Calibration(_)
                | ClapParamPayload::GateMode(_)
                | ClapParamPayload::CabBypass(_)
        )
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
    /// The encapsulated model for neural inference (Right Channel in dual-mono mode).
    /// `None` in mono mode or when cleared.
    pub model_r: Option<Box<StaticModel>>,
    /// Polyphase sinc resampler matching the model rate.
    pub new_resampler: Box<NamResampler>,
    /// Streaming resample adapter with strict host cardinality,
    /// matching the model rate and the host buffer size.
    pub new_stream: Box<neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer>,
    /// Native sample rate of the model in this payload.
    pub model_rate: u32,
    /// Model input gain calibration multiplier.
    pub input_mult_adj: f32,
    /// Model output gain calibration multiplier.
    pub output_mult_adj: f32,
    /// Amp model calibration metadata (`input_level_dbu`, `output_level_dbu`, `loudness`).
    pub cal_meta: ModelCalibrationMeta,
    /// Off-RT measured per-sample inference cost (ns) of `model_l`, produced by
    /// the engine's [`measure_cost_ns_per_sample`](neural_amp_modeler_rs::models::NamModel::measure_cost_ns_per_sample).
    /// `None` selects the conservative per-family prewarm table.
    pub prewarm_cost_ns: Option<f32>,
}

/// Pre-injects the canonical `RtStatusFlags` instance into both channel models
/// before the payload leaves the main thread.
///
/// The loaders build models with no status attached, so the audio-thread
/// install would otherwise have to adopt the `Arc` (one atomic increment) and
/// would silently refuse a foreign instance. Pre-injecting the canonical
/// instance off-RT keeps the later RT hand-off a pure `ptr_eq` no-op.
pub(crate) fn preinject_rt_status(
    status: &Arc<RtStatusFlags>,
    model_l: &mut Option<Box<StaticModel>>,
    model_r: &mut Option<Box<StaticModel>>,
) {
    if let Some(model) = model_l.as_mut() {
        model.inject_rt_status(Arc::clone(status));
    }
    if let Some(model) = model_r.as_mut() {
        model.inject_rt_status(Arc::clone(status));
    }
}

/// A latency-affecting resource swap staged to land only on the next host
/// restart cycle (Strict Restart Policy).
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
    /// Cabinet slot component (`None` clears the cabinet slot).
    pub(crate) ir: Option<Option<CabSlotDsp>>,
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
    /// Cabinet slot component: `Some(Some(cab))` loads, `Some(None)` clears,
    /// `None` leaves the active cabinet untouched (ForPreset without cabinet).
    pub ir: Option<Option<CabSlotDsp>>,
    /// Full RT parameter snapshot applied atomically with the model/IR.
    pub params: RtProcessingParams,
    /// Calibration configuration restored atomically with the preset/project state.
    pub calibration: CalibrationConfig,
    /// Gate mode restored atomically with the preset/project state.
    pub gate_mode: GateMode,
    /// Independent cabinet bypass state restored atomically with the preset/project state.
    pub cab_bypass: bool,
}

/// Model publication data retained until the restore transaction is acked.
pub struct RestoreModelPublish {
    /// Model metadata for GUI display.
    pub metadata: NamModelMetadata,
    /// Dynamic model info for diagnostics.
    pub info: ModelInfo,
    /// Dynamic model memory footprint in bytes.
    pub memory_bytes: usize,
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
    /// IR level policy restored with this transaction.
    pub ir_level_policy: crate::clap::extensions::ir_level_policy::IrLevelPolicy,
    /// Input calibration and output mode configuration restored with this transaction.
    pub calibration: crate::clap::extensions::calibration::CalibrationConfig,
    /// Gate mode restored with this transaction.
    pub gate_mode: GateMode,
    /// Cabinet bypass state restored with this transaction.
    pub cab_bypass: bool,
    /// Cabinet asset kind (`None` when cabinet slot is empty).
    pub cab_kind: Option<CabAssetKind>,
    /// Preserved raw cabinet `.nam`/`.namb` bytes for rate renegotiation rebuilds.
    pub cab_nam_raw_bytes: Option<(CabAssetKind, Vec<u8>)>,
    /// Disambiguated cabinet UI metadata.
    pub cab_ui_meta: Option<CabUiMeta>,
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
/// the next host restart cycle (Strict Restart Policy).
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
    /// Capture input calibration reference level (dBu), when present in metadata.
    pub input_level_dbu: Option<f32>,
    /// Capture output reference level (dBu), when present in metadata.
    pub output_level_dbu: Option<f32>,
    /// Measured loudness (dB), when present in metadata.
    pub loudness: Option<f32>,
}

/// Owned representation of a CLAP preset discovery `Location` for async notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetLocationOrigin {
    /// `CLAP_PRESET_DISCOVERY_LOCATION_FILE` with its file-system path.
    File(CString),
    /// `CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN` (internal plugin preset catalog).
    Plugin,
}

impl PresetLocationOrigin {
    /// Converts this owned origin back into a borrowed CLAP `Location<'_>`.
    pub fn as_clap_location(&self) -> clack_extensions::preset_discovery::prelude::Location<'_> {
        match self {
            Self::File(path) => clack_extensions::preset_discovery::prelude::Location::File {
                path: path.as_c_str(),
            },
            Self::Plugin => clack_extensions::preset_discovery::prelude::Location::Plugin,
        }
    }
}

/// Origin of a model load request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRequestOrigin {
    /// Triggered via the graphical interface / file dialog.
    Gui,
    /// Triggered via CLAP preset-load extension.
    Preset {
        /// Owned representation of the CLAP `Location`.
        location: PresetLocationOrigin,
        /// Owned copy of the `load_key` (preset identifier from discovery provider).
        load_key: Option<CString>,
    },
}

/// A pending model load request queued for processing by the main thread.
#[derive(Debug, Clone)]
pub struct PendingModelRequest {
    /// Path to the model file to load.
    pub path: PathBuf,
    /// Origin of this load request (GUI or CLAP preset load).
    pub origin: ModelRequestOrigin,
}

/// Maximum capacity of the pending model load queue (bounded FIFO).
pub const MAX_PENDING_MODEL_REQUESTS: usize = 16;

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
    /// The encapsulated model for neural inference (Right Channel in dual-mono mode).
    pub model_r: Option<Box<StaticModel>>,
    /// Model native sample rate read from the .nam file metadata.
    /// Used to construct the polyphase resampler at flush time when
    /// `activate()` has set the real host `sample_rate`.
    pub model_rate: u32,
    /// Model input gain calibration multiplier.
    pub input_mult_adj: f32,
    /// Model output gain calibration multiplier.
    pub output_mult_adj: f32,
    /// Amp model calibration metadata (`input_level_dbu`, `output_level_dbu`, `loudness`).
    pub cal_meta: ModelCalibrationMeta,
}

/// A slimmable-rebuilt model delivered from the main thread to the audio
/// thread, tagged with the model generation it was sliced from.
///
/// The audio thread installs the rebuilt model only when `generation` matches
/// the generation of the model it is currently running; a stale rebuild
/// (produced from a model that was swapped out in the meantime) is discarded
/// straight to the GC without touching the active DSP state.
pub struct SlimmableRebuild {
    /// Identity of the request whose slice this payload carries. The audio
    /// thread installs it only while it is still the newest published request,
    /// so a result superseded by B can never overwrite B (F-13/T7.2).
    pub request_id: u64,
    /// Monotonic generation of the active model at the time the rebuild was
    /// requested (echoed from the audio-thread request payload).
    pub generation: u64,
    /// The rebuilt (sliced + prewarmed + resized) model for the primary
    /// channel, boxed for RT-safe swap and GC disposal (`Option` lets the RT
    /// handler move it out without unboxing and dropping the
    /// `SlimmableRebuild` envelope on RT).
    pub model: Option<Box<StaticModel>>,
    /// The rebuilt model for the secondary (dual-mono) channel. `None` for a
    /// mono request; the RT handler replaces `model_r` only when present.
    pub model_r: Option<Box<StaticModel>>,
}

/// Commands recovered from the command ring after a failed activation.
pub type RecoveredCommands = std::collections::VecDeque<Box<ClapParamPayload>>;

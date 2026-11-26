// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Cache-line-isolated shared state blocks (`RtToUi`, `UiToRt`, `ColdShared`).

use super::gui_events::UI_NO_INTERACTION;
use super::payloads::{
    ClapParamPayload, NamModelMetadata, PendingModel, PendingModelRequest, RecoveredCommands,
    SlimmableRebuild,
};
use crate::clap::extensions::cabinet::{CabAssetKind, CabUiMeta};
use crate::clap::extensions::calibration::{
    CalibrationConfig, INPUT_CALIBRATION_LEVEL_DEFAULT_DBU, OutputMode,
};
use crate::clap::extensions::gate_mode::GateMode;
use crate::clap::extensions::params::PARAM_COUNT;
use crate::clap::gui::dialog_state::{DialogSharedState, IrDialogSharedState};
use crate::clap::plugin::command_scheduler::CMD_QUEUE_CAPACITY;
use crate::clap::plugin::retired_box::{
    RETIRED_BOX_QUEUE_CAPACITY, RetiredBox, RetiredBoxOverflow,
};
use crate::clap::processor::DeactivatedDspState;
use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, RtStatusFlags};
use neural_amp_modeler_rs::models::StaticModel;
use rtrb::{Consumer, Producer, RingBuffer};
use std::path::PathBuf;
use std::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};
use std::sync::{Arc, Mutex};

/// Render mode constants for `ColdShared::render_mode`.
/// Render mode: realtime (normal processing).
pub const RENDER_MODE_REALTIME: u32 = 0;
/// Render mode: offline (export/bounce, max quality, no soft-degrade).
pub const RENDER_MODE_OFFLINE: u32 = 1;

/// Port configuration constant: Mono (1 input port [1 channel], 1 output port [1 channel]).
pub const CONFIG_ID_MONO: u32 = 0;
/// Port configuration constant: Stereo / Dual-Mono (1 input port [2 channels], 1 output port [2 channels]).
pub const CONFIG_ID_STEREO: u32 = 1;

// ---------------------------------------------------------------------------
// UI Alert bits
// ---------------------------------------------------------------------------

/// Audio processor is poisoned due to a fatal panic in the RT callback.
pub const UI_ALERT_POISONED: u32 = 1 << 0;
/// Audio host provided a buffer violating the FFI contract (e.g. oversize block).
pub const UI_ALERT_HOST_CONTRACT: u32 = 1 << 1;
/// Polyphase sinc resampler rebuild failed.
pub const UI_ALERT_RESAMPLER_FAILED: u32 = 1 << 2;
/// CabSim sub-block violated adapter contract.
pub const UI_ALERT_CABSIM_CONTRACT: u32 = 1 << 3;
/// Soft-degrade: minimal quality mode engaged under CPU pressure.
pub const UI_ALERT_DEGRADE_MINIMAL: u32 = 1 << 4;
/// Soft-degrade: reduced quality mode engaged under CPU pressure.
pub const UI_ALERT_DEGRADE_REDUCED: u32 = 1 << 5;
/// Non-finite input samples (NaN/Inf) detected; muted and reconverging (3s hold).
pub const UI_ALERT_NON_FINITE_INPUT: u32 = 1 << 6;

/// Telemetry fields communicated from the RT audio thread to the UI / main thread.
///
/// # Concurrency Protocol
///
/// Most fields in this struct (`ui_clip_indicator`, `ui_gate_active`, `current_latency`,
/// `cabsim_tail_samples`, `active_channel_count`) are written exclusively by the audio
/// thread and read lock-free by the UI/main thread using `Ordering::Relaxed`.
///
/// Two bidirectional exceptions exist to support loss-free UI metering and acknowledgement:
/// - **Peak Telemetry (`ui_peak_l`, `ui_peak_r`)**: The audio thread atomically accumulates
///   the maximum sample peak across audio blocks using `AtomicU32::fetch_max(peak.to_bits(), Ordering::Relaxed)`.
///   Because non-negative IEEE 754 float bit patterns preserve monotonic order with unsigned
///   integers, `AtomicU32::fetch_max` is strictly ordered and race-free. The UI thread
///   periodically reads and resets the accumulated peak to zero using `swap(0.0f32.to_bits(), Ordering::Relaxed)`.
/// - **Sticky Clip Flag (`ui_clipped`)**: Set to `true` by the audio thread upon detecting
///   an over-scale sample (> 1.0), and cleared to `false` by the UI thread upon acknowledgement.
#[repr(align(128))]
pub struct RtToUi {
    /// Sample Peak L level accumulated by the audio thread (`fetch_max`) and read/reset by UI (`swap(0.0)`).
    pub ui_peak_l: AtomicU32,
    /// Sample Peak R level accumulated by the audio thread (`fetch_max`) and read/reset by UI (`swap(0.0)`).
    pub ui_peak_r: AtomicU32,
    /// Flag indicating whether clipping has occurred since the last UI frame. Set by audio thread; read/reset by UI.
    pub ui_clipped: AtomicBool,
    /// Flag indicating whether output/input clipping is active for UI indicator display.
    pub ui_clip_indicator: AtomicBool,
    /// Input Sample Peak L level accumulated by the audio thread (`fetch_max`) and read/reset by UI (`swap(0.0)`).
    pub ui_in_peak_l: AtomicU32,
    /// Input Sample Peak R level accumulated by the audio thread (`fetch_max`) and read/reset by UI (`swap(0.0)`).
    pub ui_in_peak_r: AtomicU32,
    /// Flag indicating whether input clipping has occurred since the last UI frame. Set by audio thread; read/reset by UI.
    pub ui_in_clipped: AtomicBool,
    /// Flag indicating whether input clipping is active for UI indicator display.
    pub ui_in_clip_indicator: AtomicBool,
    /// Input RMS / energy level (L) computed by audio thread EWMA (tau ≈ 300 ms), stored as f32 bits.
    pub ui_in_rms_l: AtomicU32,
    /// Input RMS / energy level (R) computed by audio thread EWMA (tau ≈ 300 ms), stored as f32 bits.
    pub ui_in_rms_r: AtomicU32,
    /// Output RMS / energy level (L) computed by audio thread EWMA (tau ≈ 300 ms), stored as f32 bits.
    pub ui_out_rms_l: AtomicU32,
    /// Output RMS / energy level (R) computed by audio thread EWMA (tau ≈ 300 ms), stored as f32 bits.
    pub ui_out_rms_r: AtomicU32,
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
    /// DSP load permille (0..=1000) updated per block via EWMA (tau ≈ 250 ms).
    pub dsp_load_permille: AtomicU16,
    /// Peak instantaneous DSP load permille observed on any overloaded block since
    /// the last main-thread log drain (`0` when no overload occurred in window).
    pub dsp_overload_peak_permille: AtomicU16,
    /// Xrun / overload counter epoch (mirrors `dsp_overloads`).
    pub xrun_epoch: AtomicU32,
    /// Non-finite input detection epoch counter incremented by RT audio loop.
    pub non_finite_epoch: AtomicU32,
    /// Prewarm zero-budget starvation transition epoch counter.
    pub prewarm_starvation_epoch: AtomicU32,
    /// Indicates whether the real-time audio thread is actively draining prewarm amortization.
    pub prewarm_pending: AtomicBool,
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
    /// Opt-in input calibration toggle (0=Off, 1=On; new project default 0).
    pub param_calibrate_input: AtomicU32,
    /// Input calibration reference level (f32 bits in `[-60.0, 60.0]` dBu, default 12.0).
    pub param_input_calibration_level: AtomicU32,
    /// Model output mode (0=Raw, 1=Normalized, 2=Calibrated; default 1).
    pub param_output_mode: AtomicU32,
    /// Noise gate operating mode (0=Silence legacy, 1=Musical expander; default 0).
    pub param_gate_mode: AtomicU32,
    /// Independent cabinet slot bypass (0=Active, 1=Bypassed; default 0).
    pub param_cab_bypass: AtomicU32,
    /// Gesture and modification flag bitmap per parameter (GUI -> Host/Processor).
    pub gesture_flags: AtomicU64,
    /// Monotonic generation counter bumped (Release) by GUI on any param write.
    pub gui_param_generation: AtomicU32,
    /// True if the host has deactivated the R channel via audio-ports-activation.
    /// Written by Main Thread (or Audio Thread when processing), read every block by RT.
    pub host_r_deactivated: AtomicBool,
}

impl Default for RtToUi {
    fn default() -> Self {
        Self {
            ui_peak_l: AtomicU32::new(0.0f32.to_bits()),
            ui_peak_r: AtomicU32::new(0.0f32.to_bits()),
            ui_clipped: AtomicBool::new(false),
            ui_clip_indicator: AtomicBool::new(false),
            ui_in_peak_l: AtomicU32::new(0.0f32.to_bits()),
            ui_in_peak_r: AtomicU32::new(0.0f32.to_bits()),
            ui_in_clipped: AtomicBool::new(false),
            ui_in_clip_indicator: AtomicBool::new(false),
            ui_in_rms_l: AtomicU32::new(0.0f32.to_bits()),
            ui_in_rms_r: AtomicU32::new(0.0f32.to_bits()),
            ui_out_rms_l: AtomicU32::new(0.0f32.to_bits()),
            ui_out_rms_r: AtomicU32::new(0.0f32.to_bits()),
            ui_gate_active: AtomicBool::new(false),
            current_latency: AtomicU32::new(0),
            cabsim_tail_samples: AtomicU32::new(0),
            active_channel_count: AtomicU32::new(1),
            dsp_load_permille: AtomicU16::new(0),
            dsp_overload_peak_permille: AtomicU16::new(0),
            xrun_epoch: AtomicU32::new(0),
            non_finite_epoch: AtomicU32::new(0),
            prewarm_starvation_epoch: AtomicU32::new(0),
            prewarm_pending: AtomicBool::new(false),
        }
    }
}

impl Default for UiToRt {
    fn default() -> Self {
        Self {
            param_input_gain: AtomicU32::new(0.0f32.to_bits()),
            param_output_gain: AtomicU32::new(0.0f32.to_bits()),
            // Gate off by default: parked at the range minimum (-90 dB), the most
            // permissive setting available, so the noise gate practically never
            // closes out of the box.
            param_gate_thresh: AtomicU32::new((-90.0f32).to_bits()),
            param_bypass: AtomicU32::new(0),
            param_adaptive_compute: AtomicU32::new(1), // Conservative by default in CLAP plugin
            param_slim_override: AtomicU32::new(0),    // Auto by default
            param_oversample: AtomicU32::new(0),       // Off by default
            param_activation: AtomicU32::new(1),       // Standard (exact-grade) by default
            param_calibrate_input: AtomicU32::new(0),  // Opt-in Off for new instances
            param_input_calibration_level: AtomicU32::new(
                INPUT_CALIBRATION_LEVEL_DEFAULT_DBU.to_bits(),
            ),
            param_output_mode: AtomicU32::new(OutputMode::Normalized.to_u32()),
            param_gate_mode: AtomicU32::new(GateMode::Silence.to_u32()),
            param_cab_bypass: AtomicU32::new(0),
            gesture_flags: AtomicU64::new(0),
            gui_param_generation: AtomicU32::new(0),
            host_r_deactivated: AtomicBool::new(false),
        }
    }
}

/// Fields accessed at low frequency by both threads (init, shutdown, rare events).
#[repr(align(128))]
pub struct ColdShared {
    /// Unique instance identifier for multi-instance telemetry and logging isolation.
    pub instance_id: u64,
    /// SPSC channel: Main Thread -> Audio Thread (New parameters/models).
    /// Payloads travel boxed: boxes are created on the main-thread push path
    /// only, so the audio thread pops with zero allocation.
    pub param_tx: Mutex<Option<Producer<Box<ClapParamPayload>>>>,
    /// SPSC channel: Main Thread -> Audio Thread (Consumer).
    pub param_rx: Mutex<Option<Consumer<Box<ClapParamPayload>>>>,
    /// GC channel: Audio Thread -> Main Thread (Obsolete models for disposal).
    pub gc_tx: Mutex<Option<Producer<GcItem>>>,
    /// GC channel: Audio Thread -> Main Thread (Consumer).
    pub gc_rx: Mutex<Option<Consumer<GcItem>>>,
    /// Fallback buffer for GC overflow (overwrite).
    pub gc_overflow: Arc<GcOverflowBuffer>,
    /// Off-RT disposal channel for consumed command/slimmable `Box` envelopes.
    pub(crate) box_gc_tx: Mutex<Option<Producer<RetiredBox>>>,
    /// Main-thread consumer for retired `Box` envelopes.
    pub(crate) box_gc_rx: Mutex<Option<Consumer<RetiredBox>>>,
    /// Atomic overflow table for retired `Box` envelopes if SPSC + parking lot saturate.
    pub(crate) box_overflow: RetiredBoxOverflow,
    /// Atomic status flags (RT->Main telemetry).
    pub rt_status: Arc<RtStatusFlags>,
    /// Aggregated UI alert bits derived from RT status flags by housekeeping.
    pub ui_alert_bits: AtomicU32,
    /// Native sample rate required by the actively loaded model.
    pub model_sample_rate: AtomicU32,
    /// Detected host sample rate.
    pub sample_rate: AtomicU32,
    /// Host buffer size.
    pub buffer_size: AtomicU32,
    /// Latency contribution (host-rate samples) of the streaming resample
    /// adapter **currently installed** on the audio thread.
    pub current_stream_latency: AtomicU32,
    /// Latency contribution (host-rate samples) of the cab-sim convolution
    /// adapter **currently installed** on the audio thread (`0` when no IR is loaded).
    pub current_cabsim_latency: AtomicU32,
    /// Dynamic accent color based on DAW track color (packed ARGB).
    pub track_accent_color: AtomicU32,
    /// Parameter indication (mapping, automation, and override) for all parameters.
    /// Bit 0: Mapped, Bit 1: Automating, Bit 2: Override.
    pub param_indication: [AtomicU8; PARAM_COUNT],
    /// Indicated/mapped parameter colors (packed ARGB).
    pub param_indication_color: [AtomicU32; PARAM_COUNT],
    /// Model load counter (incremented on each successful model load).
    pub model_load_counter: AtomicU32,
    /// Monotonic model-generation allocator (`0` is reserved for "no model installed yet").
    pub model_generation: AtomicU64,
    /// Milliseconds since `ui_monotonic_ms`'s process epoch of the last user
    /// interaction with the GUI, or [`UI_NO_INTERACTION`] when none has been recorded yet.
    pub ui_last_interaction_ms: AtomicU64,
    /// Loaded model name (path basename). Written by the main thread, read by the UI thread.
    pub ui_model_name: Mutex<String>,
    /// Loaded model metadata for UI display.
    pub ui_model_metadata: Mutex<Option<NamModelMetadata>>,
    /// Pending model load requests to be processed by the Main Thread (bounded FIFO queue).
    pub pending_model_requests: Mutex<std::collections::VecDeque<PendingModelRequest>>,
    /// Whether the model file-picker dialog is currently open.
    pub ui_dialog_open: AtomicBool,
    /// Whether a model load is actually running.
    pub ui_loading: AtomicBool,
    /// Flag signaling that a model loading error occurred.
    pub ui_load_error: AtomicBool,
    /// Detailed error message for the GUI.
    pub ui_load_error_msg: Mutex<String>,
    /// Dynamic model info for diagnostics.
    pub ui_model_info: Mutex<Option<neural_amp_modeler_rs::common::diagnostics::ModelInfo>>,
    /// Dynamic model memory footprint in bytes for diagnostics (0 if no model).
    pub ui_model_memory_bytes: AtomicUsize,
    /// Flag signaling that the model should be cleared (unload model).
    pub ui_clear_model: AtomicBool,
    /// Lifetime fence: true while the plugin exists. Checked by the File Picker thread.
    pub alive_fence: Arc<AtomicBool>,
    /// Backend signal: the user closed the GUI window (WM close button, Alt-F4).
    pub gui_user_closed: AtomicBool,
    /// Render mode as set by the host via `clap.render`: 0 = Realtime, 1 = Offline.
    pub render_mode: AtomicU32,
    /// Current GUI scale factor (f32 bits).
    pub gui_scale_factor: AtomicU32,
    /// Active cab-sim IR file path (for state save/load and GUI display).
    pub ir_path: Mutex<Option<String>>,
    /// SHA-256 hex digest of the active cab-sim IR file.
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
    /// Native sample rate of the stored `ir_raw_samples`.
    pub ir_raw_sample_rate: AtomicU32,
    /// Active cabinet IR level policy (`IrLevelPolicy::to_u8()`).
    pub ir_level_policy: std::sync::atomic::AtomicU8,
    /// Active cabinet slot format (`0` = none, `1` = WAV IR, `2` = NAM, `3` = NAMB).
    pub cab_kind: AtomicU8,
    /// Preserved raw cabinet neural model bytes for reactivation/resampling rebuilds.
    pub cab_nam_raw_bytes: Mutex<Option<(CabAssetKind, Vec<u8>)>>,
    /// Disambiguated cabinet display metadata (format, parent/file label, arch/rate, gear tags).
    pub cab_ui_meta: Mutex<Option<CabUiMeta>>,
    /// In-flight cabinet bypass state update retained when SPSC push saturates.
    pub in_flight_cab_bypass: Mutex<Option<bool>>,
    /// Flag signaling that the IR level policy changed in the UI and the
    /// active `CabSimPair` should be rebuilt from `ir_raw_samples`.
    pub ui_ir_policy_rebuild_pending: AtomicBool,
    /// SPSC channel: Main Thread -> Audio Thread (Slimmable model producer).
    pub slimmable_tx: Mutex<Option<Producer<Box<SlimmableRebuild>>>>,
    /// SPSC channel: Main Thread -> Audio Thread (Slimmable model consumer).
    pub slimmable_rx: Mutex<Option<Consumer<Box<SlimmableRebuild>>>>,
    /// Slimmable rebuild request payload: the monotonic model generation the
    /// audio thread was running when it requested the rebuild.
    pub requested_slimmable_generation: AtomicU64,
    /// Whether the request targets both channels (dual-mono).
    pub requested_slimmable_dual: AtomicBool,
    /// Seqlock counter for the request snapshot.
    pub slimmable_request_seq: AtomicU64,
    /// Monotonic allocator for slimmable request identities (`0` reserved).
    pub slimmable_request_counter: AtomicU64,
    /// Identity of the slimmable request currently owned by the main thread (`0` = none).
    pub slimmable_request_id: AtomicU64,
    /// Newest request identity ever published (`0` = none / cancelled).
    pub latest_slimmable_request_id: AtomicU64,
    /// Telemetry: total slimmable rebuilds discarded by the audio thread for stale generation.
    pub slimmable_stale_discarded_total: AtomicU32,
    /// Packed RT request for off-RT neural cabinet rebuild: `(generation << 1) | (pending_bit)`.
    pub cab_nam_rebuild_req: AtomicU64,
    /// Telemetry counter incremented when RT discards a stale `RebuildCabNam` delivery.
    pub cab_nam_stale_discarded_total: AtomicU64,
    /// Full WaveNet model weights stored for main-thread slimmable rebuild.
    pub full_wavenet_model: Mutex<Option<Box<StaticModel>>>,
    /// Monotonic command sequence counter incremented by the main thread.
    pub cmd_next_seq: AtomicU64,
    /// Last sequence fully drained and processed by the audio thread.
    pub cmd_last_ack: AtomicU64,
    /// Non-model commands recovered from the command ring after failed `activate()`.
    pub recovered_commands: Mutex<RecoveredCommands>,
    /// Generation of the most recent restore transaction applied atomically by RT.
    pub last_applied_generation: AtomicU64,
    /// Pending restart oversampling request (`PendingRestartOs` encoding).
    pub pending_restart_os_factor: AtomicU32,
    /// Set by every writer of `pending_restart_os_factor` that runs while the
    /// plugin is active. The orchestrator consumes it at the end of the
    /// callback (and `flush_inner` at its end) to issue exactly one
    /// `request_restart()` per callback, even when the factor was published
    /// before the callback began (GUI sync / SPSC drain).
    pub restart_wanted: AtomicBool,
    /// Last tail length in samples reported to the host via `HostTail::changed`.
    pub last_reported_tail_samples: AtomicU32,
    /// In-flight parameter snapshot retained when SPSC push saturates.
    pub in_flight_params: Mutex<Option<neural_amp_modeler_rs::common::params::RtProcessingParams>>,
    /// In-flight calibration snapshot retained when SPSC push saturates.
    pub in_flight_calibration: Mutex<Option<CalibrationConfig>>,
    /// In-flight gate mode update retained when SPSC push saturates.
    pub in_flight_gate_mode: Mutex<Option<GateMode>>,
    /// Model loaded before `activate()` (state restore while `buffer_size == 0`).
    pub pending_model: Mutex<Option<PendingModel>>,
    /// Heavy DSP resources preserved across deactivate/activate cycles.
    pub(crate) deactivated_dsp: Mutex<Option<DeactivatedDspState>>,
    /// Dialog state for model file-dialog (Arc-backed, UAF-safe).
    pub(crate) dialog_state: Option<Arc<DialogSharedState>>,
    /// Dialog state for IR file-dialog (Arc-backed, UAF-safe).
    pub(crate) ir_dialog_state: Option<Arc<IrDialogSharedState>>,
    /// Sink for model dialog thread handle.
    pub(crate) dialog_handle_sink: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Sink for IR dialog thread handle.
    pub(crate) ir_dialog_handle_sink: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Host-log bridge sink for forwarding `log::*` macros to the DAW host.
    pub(crate) host_log_sink:
        Mutex<Option<Arc<neural_amp_modeler_rs::common::diagnostics::logger::HostLogFn>>>,
    /// Synchronization fence for host log sink quiescence during plugin teardown.
    pub(crate) host_log_quiescence: Arc<crate::clap::plugin::quiescence::HostLogQuiescence>,
    /// Synchronization fence for host calls from detachable threads.
    pub host_call_quiescence: Arc<crate::clap::plugin::quiescence::HostCallQuiescence>,
    /// Whether the audio processor is currently active in audio processing.
    pub is_processing: AtomicBool,
    /// Total wakeups of the background model loader worker thread.
    pub loader_worker_wakeups: AtomicU64,
    /// Selected audio ports configuration (0 = Mono, 1 = Stereo).
    pub selected_port_config: AtomicU32,
    /// Whether audio port 0 input is active.
    pub port_input_active: AtomicBool,
    /// Whether audio port 0 output is active.
    pub port_output_active: AtomicBool,
    /// Test-only per-instance hook to inject a panic inside `process()`.
    #[cfg(any(test, feature = "testing"))]
    pub test_panic_injection: AtomicBool,
    /// Test-only per-instance hook to inject a panic inside `reset()`.
    #[cfg(any(test, feature = "testing"))]
    pub test_reset_panic_injection: AtomicBool,
    /// Test-only per-instance hook to inject a panic inside the audio-thread `flush()`.
    #[cfg(any(test, feature = "testing"))]
    pub test_flush_panic_injection: AtomicBool,
}

impl ColdShared {
    /// Publishes a new slimmable rebuild request from the audio thread.
    ///
    /// Allocates a fresh identity, then publishes the channel/generation/dual
    /// snapshot inside a seqlock write window. No allocation, no locks, bounded work.
    pub(crate) fn publish_slimmable_request(
        &self,
        target_ch: u32,
        generation: u64,
        dual: bool,
    ) -> u64 {
        let counter = self
            .slimmable_request_counter
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let id = if counter == 0 {
            self.slimmable_request_counter
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1)
        } else {
            counter
        };
        self.slimmable_request_seq.fetch_add(1, Ordering::AcqRel);
        self.rt_status
            .requested_slimmable_ch
            .store(target_ch, Ordering::Relaxed);
        self.requested_slimmable_generation
            .store(generation, Ordering::Relaxed);
        self.requested_slimmable_dual.store(dual, Ordering::Relaxed);
        self.slimmable_request_id.store(id, Ordering::Relaxed);
        self.latest_slimmable_request_id
            .store(id, Ordering::Release);
        self.slimmable_request_seq.fetch_add(1, Ordering::Release);
        self.rt_status.set_flag_release(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
        );
        id
    }

    /// Reads a consistent `(identity, channel, generation, dual)` snapshot of
    /// the pending request, or `None` when no request is outstanding or the
    /// seqlock window could not be validated within the bounded retries.
    pub(crate) fn snapshot_slimmable_request(&self) -> Option<(u64, u32, u64, bool)> {
        self.snapshot_slimmable_request_with(|| {})
    }

    /// Shared seqlock reader; `between_loads` exists only so tests can force a
    /// writer to begin publishing after the reader has sampled the payload.
    fn snapshot_slimmable_request_with<F: FnOnce()>(
        &self,
        between_loads: F,
    ) -> Option<(u64, u32, u64, bool)> {
        let mut seq = self.slimmable_request_seq.load(Ordering::Acquire);
        let mut hook = Some(between_loads);
        for _ in 0..8 {
            if seq & 1 != 0 {
                seq = self.slimmable_request_seq.load(Ordering::Acquire);
                continue;
            }
            let id = self.slimmable_request_id.load(Ordering::Relaxed);
            let ch = self
                .rt_status
                .requested_slimmable_ch
                .load(Ordering::Relaxed);
            let generation = self.requested_slimmable_generation.load(Ordering::Relaxed);
            let dual = self.requested_slimmable_dual.load(Ordering::Relaxed);
            if let Some(hook) = hook.take() {
                hook();
            }
            fence(Ordering::Acquire);
            let after = self.slimmable_request_seq.load(Ordering::Acquire);
            if after == seq {
                if id == 0 {
                    return None;
                }
                return Some((id, ch, generation, dual));
            }
            seq = after;
        }
        None
    }

    #[cfg(test)]
    pub(crate) fn snapshot_slimmable_request_with_test_hook<F: FnOnce()>(
        &self,
        hook: F,
    ) -> Option<(u64, u32, u64, bool)> {
        self.snapshot_slimmable_request_with(hook)
    }

    /// Returns the pending request identity (`0` = none).
    #[inline(always)]
    pub(crate) fn pending_slimmable_request(&self) -> u64 {
        self.slimmable_request_id.load(Ordering::Acquire)
    }

    /// Acknowledges the request `id` after its result was consumed.
    pub(crate) fn complete_slimmable_request(&self, id: u64) -> bool {
        self.rt_status.clear_flag_release(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
        );
        if self
            .slimmable_request_id
            .compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            true
        } else {
            self.rt_status.set_flag_release(
                neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
            );
            false
        }
    }

    /// Returns the newest published request identity (`0` = none/cancelled).
    #[inline(always)]
    pub(crate) fn latest_slimmable_request(&self) -> u64 {
        self.latest_slimmable_request_id.load(Ordering::Acquire)
    }

    /// Cancels any pending slimmable request (model swap / teardown).
    pub(crate) fn cancel_slimmable_request(&self) {
        self.slimmable_request_seq.fetch_add(1, Ordering::AcqRel);
        self.slimmable_request_id.store(0, Ordering::Relaxed);
        self.latest_slimmable_request_id.store(0, Ordering::Release);
        self.slimmable_request_seq.fetch_add(1, Ordering::Release);
        self.rt_status.clear_flag_release(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
        );
    }

    /// Creates a new `ColdShared` instance with default production/test fields and pre-allocated SPSC channels.
    pub fn new(instance_id: u64) -> Self {
        let (param_tx, param_rx) = RingBuffer::new(CMD_QUEUE_CAPACITY);
        let (gc_tx, gc_rx) = RingBuffer::new(32);
        let (box_gc_tx, box_gc_rx) = RingBuffer::new(RETIRED_BOX_QUEUE_CAPACITY);
        let (slimmable_tx, slimmable_rx) = RingBuffer::new(4);
        let alive_fence = Arc::new(AtomicBool::new(true));

        Self {
            instance_id,
            param_tx: Mutex::new(Some(param_tx)),
            param_rx: Mutex::new(Some(param_rx)),
            gc_tx: Mutex::new(Some(gc_tx)),
            gc_rx: Mutex::new(Some(gc_rx)),
            gc_overflow: Arc::new(GcOverflowBuffer::new(
                neural_amp_modeler_rs::common::spsc::SPSC_CAPACITY,
            )),
            box_gc_tx: Mutex::new(Some(box_gc_tx)),
            box_gc_rx: Mutex::new(Some(box_gc_rx)),
            box_overflow: RetiredBoxOverflow::new(),
            rt_status: Arc::new(RtStatusFlags::new()),
            ui_alert_bits: AtomicU32::new(0),
            model_sample_rate: AtomicU32::new(48000),
            sample_rate: AtomicU32::new(0),
            buffer_size: AtomicU32::new(0),
            current_stream_latency: AtomicU32::new(0),
            current_cabsim_latency: AtomicU32::new(0),
            track_accent_color: AtomicU32::new(0),
            param_indication: std::array::from_fn(|_| AtomicU8::new(0)),
            param_indication_color: std::array::from_fn(|_| AtomicU32::new(0)),
            model_load_counter: AtomicU32::new(0),
            model_generation: AtomicU64::new(0),
            ui_last_interaction_ms: AtomicU64::new(UI_NO_INTERACTION),
            ui_model_name: Mutex::new(String::new()),
            ui_model_metadata: Mutex::new(None),
            pending_model_requests: Mutex::new(std::collections::VecDeque::new()),
            ui_dialog_open: AtomicBool::new(false),
            ui_loading: AtomicBool::new(false),
            ui_load_error: AtomicBool::new(false),
            ui_load_error_msg: Mutex::new(String::new()),
            ui_model_info: Mutex::new(None),
            ui_model_memory_bytes: AtomicUsize::new(0),
            ui_clear_model: AtomicBool::new(false),
            alive_fence: Arc::clone(&alive_fence),
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
            ir_raw_samples: Mutex::new(None),
            ir_raw_sample_rate: AtomicU32::new(0),
            ir_level_policy: std::sync::atomic::AtomicU8::new(
                crate::clap::extensions::ir_level_policy::IrLevelPolicy::PeakNormalized.to_u8(),
            ),
            cab_kind: AtomicU8::new(0),
            cab_nam_raw_bytes: Mutex::new(None),
            cab_ui_meta: Mutex::new(None),
            in_flight_cab_bypass: Mutex::new(None),
            ui_ir_policy_rebuild_pending: AtomicBool::new(false),
            slimmable_tx: Mutex::new(Some(slimmable_tx)),
            slimmable_rx: Mutex::new(Some(slimmable_rx)),
            requested_slimmable_generation: AtomicU64::new(0),
            requested_slimmable_dual: AtomicBool::new(false),
            slimmable_request_seq: AtomicU64::new(0),
            slimmable_request_counter: AtomicU64::new(0),
            slimmable_request_id: AtomicU64::new(0),
            latest_slimmable_request_id: AtomicU64::new(0),
            slimmable_stale_discarded_total: AtomicU32::new(0),
            cab_nam_rebuild_req: AtomicU64::new(0),
            cab_nam_stale_discarded_total: AtomicU64::new(0),
            full_wavenet_model: Mutex::new(None),
            cmd_next_seq: AtomicU64::new(0),
            cmd_last_ack: AtomicU64::new(0),
            recovered_commands: Mutex::new(RecoveredCommands::new()),
            last_applied_generation: AtomicU64::new(0),
            pending_restart_os_factor: AtomicU32::new(0),
            restart_wanted: AtomicBool::new(false),
            last_reported_tail_samples: AtomicU32::new(0),
            in_flight_params: Mutex::new(None),
            in_flight_calibration: Mutex::new(None),
            in_flight_gate_mode: Mutex::new(None),
            pending_model: Mutex::new(None),
            deactivated_dsp: Mutex::new(None),
            dialog_state: Some(Arc::new(DialogSharedState::new())),
            ir_dialog_state: Some(Arc::new(IrDialogSharedState::new())),
            dialog_handle_sink: Mutex::new(None),
            ir_dialog_handle_sink: Mutex::new(None),
            host_log_sink: Mutex::new(None),
            host_log_quiescence: Arc::new(crate::clap::plugin::quiescence::HostLogQuiescence::new()),
            host_call_quiescence: Arc::new(
                crate::clap::plugin::quiescence::HostCallQuiescence::new(Arc::clone(&alive_fence)),
            ),
            is_processing: AtomicBool::new(false),
            loader_worker_wakeups: AtomicU64::new(0),
            selected_port_config: AtomicU32::new(CONFIG_ID_MONO),
            port_input_active: AtomicBool::new(true),
            port_output_active: AtomicBool::new(true),
            #[cfg(any(test, feature = "testing"))]
            test_panic_injection: AtomicBool::new(false),
            #[cfg(any(test, feature = "testing"))]
            test_reset_panic_injection: AtomicBool::new(false),
            #[cfg(any(test, feature = "testing"))]
            test_flush_panic_injection: AtomicBool::new(false),
        }
    }

    /// Allocates the next monotonic model generation (1-based) and returns it.
    ///
    /// `0` is reserved for "no model installed yet".
    #[inline]
    pub(crate) fn allocate_model_generation(&self) -> u64 {
        self.model_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
    }

    /// Returns `true` when the audio thread is waiting for an off-RT neural
    /// cabinet rebuild.
    #[inline]
    pub fn is_cab_nam_rebuild_pending(&self) -> bool {
        (self.cab_nam_rebuild_req.load(Ordering::Acquire) & 1) != 0
    }

    /// Returns the pending neural cabinet rebuild generation, if any.
    #[inline]
    pub fn pending_cab_nam_generation(&self) -> Option<u64> {
        let word = self.cab_nam_rebuild_req.load(Ordering::Acquire);
        if (word & 1) != 0 {
            Some(word >> 1)
        } else {
            None
        }
    }

    /// Clears the pending bit only if `cab_nam_rebuild_req` still matches
    /// `(generation << 1) | 1`, preserving any newer RT request cleanly.
    #[inline]
    pub(crate) fn clear_cab_nam_rebuild_if_gen(&self, generation: u64) {
        let expected = (generation << 1) | 1;
        let _ = self.cab_nam_rebuild_req.compare_exchange(
            expected,
            generation << 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

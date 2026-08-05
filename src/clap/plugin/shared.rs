// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Lock-free shared state between the audio thread and the main thread.

use crate::clap::processor::DeactivatedDspState;
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, RtStatusFlags};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::models::StaticModel;
use rtrb::{Consumer, Producer};
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Active plugin instance counter (S5-E5-T03).
/// Incremented in `new_shared()`, decremented in `NamClapShared::drop()`.
/// `set_shutdown_in_progress()` is only called when this reaches zero,
/// so crash-reporting remains active as long as at least one instance exists.
static ACTIVE_INSTANCES: AtomicU32 = AtomicU32::new(0);

/// Increments the active instance counter (S5-E5-T03).
#[inline]
pub(crate) fn bump_active_instances() {
    ACTIVE_INSTANCES.fetch_add(1, Ordering::Release);
}

/// Main -> RT communication payload for the CLAP plugin.
pub enum ClapParamPayload {
    /// Parameter update (gain, gate, bypass).
    Params(RtProcessingParams),
    /// Loading of a new model pair (transferred/constructed outside RT) and its resampler.
    LoadModel {
        /// The encapsulated model for neural inference (Left Channel)
        model_l: Option<Box<StaticModel>>,
        /// Polyphase sinc resampler
        new_resampler: Box<NamResampler>,
        /// Model input gain calibration multiplier (from input_level_dbu metadata).
        input_mult_adj: f32,
        /// Model output gain calibration multiplier (from loudness metadata).
        output_mult_adj: f32,
    },
    /// Loading of a new cab-sim convolution adapter via SPSC.
    /// Follows the same pattern as `LoadModel`: the adapter is constructed
    /// outside the RT thread and swapped atomically in the audio thread.
    #[cfg(test)]
    LoadCabIr {
        /// Pre-built convolution adapter (None = bypass cabsim).
        adapter: Option<neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter>,
    },
    /// Oversampling factor change: carries new pre-built engines for the audio thread.
    SetOversample {
        /// Pre-built left-channel oversample engine.
        os_l: Box<neural_amp_modeler_rs::dsp::oversample::OversampleEngine>,
        /// Pre-built right-channel oversample engine.
        os_r: Box<neural_amp_modeler_rs::dsp::oversample::OversampleEngine>,
    },
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

/// Safe wrapper for a NamClapShared pointer passed to the GUI thread.
///
/// Encapsulates a `NonNull<NamClapShared>` privately so the pointer is never
/// exposed directly. Access is mediated through `as_ptr` and `as_ref`,
/// with the SAFETY contract clearly documented at each call site.
#[derive(Clone, Copy)]
pub struct NamClapSharedRef(std::ptr::NonNull<NamClapShared>);

// SAFETY: the pointer is obtained from a leaked `Arc` — the pointee is
// never deallocated while the process runs. All interior mutation uses
// `Atomic*`/`Mutex`, so concurrent access is sound. `Send` is safe because
// the pointee is Sync+Send; `Sync` is safe because `&NamClapSharedRef` only
// allows reading the pointer itself (not dereferencing the pointee).
unsafe impl Send for NamClapSharedRef {}
unsafe impl Sync for NamClapSharedRef {}

impl NamClapSharedRef {
    /// Creates a new `NamClapSharedRef` from a raw pointer.
    ///
    /// # Safety
    ///
    /// The pointer must be valid, non-null, and obtained from a leaked `Arc<NamClapShared>`.
    /// The pointee must outlive all uses of this wrapper.
    #[inline]
    pub unsafe fn new(ptr: *const NamClapShared) -> Self {
        let nn = std::ptr::NonNull::new(ptr as *mut NamClapShared)
            .expect("NamClapSharedRef::new: pointer must not be null");
        Self(nn)
    }

    /// Returns the raw pointer without dereferencing it.
    ///
    /// Use this only for FFI or type-erased storage; use [`as_ref`](Self::as_ref)
    /// for safe access to the shared state.
    #[inline]
    pub fn as_ptr(&self) -> *const NamClapShared {
        self.0.as_ptr()
    }

    /// Returns a static reference to the shared state.
    ///
    /// # Safety
    ///
    /// The caller must ensure the `alive_fence` is active (see
    /// `NamPluginWindow::safe_shared`) — the returned reference can
    /// outlive the plugin if the fence is not checked.
    #[inline]
    pub unsafe fn as_ref(&self) -> &'static NamClapShared {
        unsafe { self.0.as_ref() }
    }
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

/// Deferred preset-load metadata for `HostPresetLoad` notification (S4-E4-T05).
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
    /// Dynamic accent color based on DAW track color (packed ARGB).
    pub track_accent_color: AtomicU32,
    /// Parameter indication (mapping, automation, and override) for the 9 parameters.
    /// Bit 0: Mapped, Bit 1: Automating, Bit 2: Override.
    pub param_indication: [AtomicU8; 9],
    /// Indicated/mapped parameter colors (packed ARGB).
    pub param_indication_color: [AtomicU32; 9],
    /// Model load counter (incremented on each successful model load).
    pub model_load_counter: AtomicU32,
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
    /// Lifetime fence: true while the plugin exists. Checked by the File Picker thread.
    pub alive_fence: Arc<AtomicBool>,
    /// Render mode as set by the host via `clap.render`: 0 = Realtime, 1 = Offline.
    /// Written by the Main Thread, read by the RT thread at low frequency (transitions only).
    pub render_mode: AtomicU32,
    /// Current GUI scale factor (f32 bits). Written by `gui.set_scale` on the Main Thread,
    /// read by the window handler for HiDPI rendering before the first Resized event.
    pub gui_scale_factor: AtomicU32,
    /// Active cab-sim IR file path (for state save/load and GUI display).
    pub ir_path: Mutex<Option<String>>,
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
    pub slimmable_tx: Mutex<Option<Producer<Option<Box<StaticModel>>>>>,
    /// SPSC channel: Main Thread -> Audio Thread (Slimmable model consumer).
    pub slimmable_rx: Mutex<Option<Consumer<Option<Box<StaticModel>>>>>,
    /// Full WaveNet model weights stored for main-thread slimmable rebuild.
    /// When a WaveNet model is loaded, a clone is stored here so the main thread
    /// can create slimmed variants without touching the audio thread.
    pub full_wavenet_model: Mutex<Option<Box<StaticModel>>>,
    /// Command scheduler ack atomics (S4-E4-T01).
    /// Monotonic sequence counter incremented by the main thread on each
    /// enqueued command batch.
    pub cmd_next_seq: AtomicU64,
    /// Last sequence fully drained and processed by the audio thread.
    /// Written by the audio thread (Release), read by the main thread (Acquire).
    pub cmd_last_ack: AtomicU64,
    /// Pending restart oversampling factor (S4-E4-T02).
    /// Set by the audio thread via `host.request_restart()` when
    /// oversampling changes during active processing. Consumed by
    /// `activate()` to build engines at the correct factor. 0 = no
    /// pending restart, 1 = 2x, 2 = 4x.
    pub pending_restart_os_factor: AtomicU32,
    /// In-flight parameter snapshot (S4-E4-T04).
    /// When the main-thread `flush()` fails to push params to the SPSC
    /// (channel full), the snapshot is stored here for retry via
    /// `host.request_callback()` → `housekeeping()`.
    pub in_flight_params: Mutex<Option<neural_amp_modeler_rs::common::params::RtProcessingParams>>,
    /// Pending preset-load operation (S4-E4-T05).
    /// Set by `load_from_location()` with the location and load_key for
    /// deferred host notification. Consumed by `housekeeping()` to call
    /// `HostPresetLoad::loaded()` or `on_error()` after the async load.
    pub pending_preset_load: Mutex<Option<PendingPresetLoad>>,
    /// Model loaded before `activate()` (state restore while `buffer_size == 0`),
    /// deferred to avoid heap allocation on the audio thread (F3 fix).
    /// See `flush_pending_model()` in load.rs and housekeeping.rs.
    pub pending_model: Mutex<Option<PendingModel>>,
    /// Heavy DSP resources preserved across deactivate/activate cycles.
    /// When the host deactivates a plugin instance, model weights, resampler,
    /// oversampling engines, and convolution partitions are moved here instead
    /// of being dropped. On the next `activate()`, they are reinstalled
    /// deterministically — avoiding I/O and recompute. See [`DeactivatedDspState`].
    pub(crate) deactivated_dsp: Mutex<Option<DeactivatedDspState>>,
    /// Dialog state for model file-dialog (Arc-backed, UAF-safe). Initialized on main thread.
    pub(crate) dialog_state:
        Option<Arc<crate::clap::gui::ui::zones::dialog_state::DialogSharedState>>,
    /// Dialog state for IR file-dialog (Arc-backed, UAF-safe). Initialized on main thread.
    pub(crate) ir_dialog_state:
        Option<Arc<crate::clap::gui::ui::zones::dialog_state::IrDialogSharedState>>,
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
/// state restore. See CLAP-F003, S1-E1-T02.
pub struct PendingModel {
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

// ---------------------------------------------------------------------------
// Outer shared struct
// ---------------------------------------------------------------------------

/// Lock-free shared state between the audio thread and the main thread.
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
pub struct NamClapShared {
    /// Cache-line-isolated sub-struct: written every block by RT, read by UI.
    pub rt_to_ui: RtToUi,
    /// Cache-line-isolated sub-struct: written by UI/Main, read every block by RT.
    pub ui_to_rt: UiToRt,
    /// Cache-line-isolated sub-struct: low-frequency access by both threads.
    pub cold: ColdShared,
}

impl<'a> PluginShared<'a> for NamClapShared {}

impl Drop for NamClapShared {
    fn drop(&mut self) {
        log::debug!("NAM-rs: NamClapShared dropped.");
        self.cold.alive_fence.store(false, Ordering::Release); // pairs with Acquire load em gui/window/state.rs:190
        // S5-E5-T03: only signal shutdown when the last instance is destroyed
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

impl NamClapShared {
    /// Bitmask for the parameter in the `gesture_flags` field.
    /// 3 flags per parameter: Changed, GestureBegin, GestureEnd.
    const GESTURE_CHANGED_SHIFT: u32 = 0;
    const GESTURE_BEGIN_SHIFT: u32 = 1;
    const GESTURE_END_SHIFT: u32 = 2;
    const GESTURE_BITS_PER_PARAM: u32 = 3;

    /// Maps a CLAP param_id (0..8) to internal index 0..8.
    const fn param_index(param_id: u32) -> usize {
        param_id as usize
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
            .fetch_add(1, Ordering::Release); // pairs with Acquire loads em processor/events.rs:94, extensions/params/audio.rs:70
    }

    /// Flushes gestures and parameter updates initiated by the GUI
    /// into the host's output event queue.
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
                let _ = output.try_push(ev);
            }
            if self.take_gesture(pi, Self::GESTURE_CHANGED_SHIFT) {
                let val = f32::from_bits(value_atomic.load(Ordering::Relaxed)) as f64;
                let ev = ParamValueEvent::new(
                    0,
                    ClapId::new(*param_id),
                    clack_plugin::events::Pckn::new(0u8, 0u8, 0u8, 0u8),
                    val,
                    clack_plugin::utils::Cookie::empty(),
                );
                let _ = output.try_push(ev);
            }
            if self.take_gesture(pi, Self::GESTURE_END_SHIFT) {
                let ev = ParamGestureEndEvent::new(0, ClapId::new(*param_id));
                let _ = output.try_push(ev);
            }
        }
    }
}

impl neural_amp_modeler_rs::common::diagnostics::HasRuntimeSnapshot for NamClapShared {
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

#[cfg(test)]
#[path = "shared_test.rs"]
mod shared_test;

#[cfg(test)]
pub(crate) use shared_test::make_test_shared;

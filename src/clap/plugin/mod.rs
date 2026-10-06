// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! NAM-Plug plugin definition and its CLAP lifecycle components.

pub mod command_scheduler;
pub mod errors;
pub mod shared;
pub use command_scheduler::{
    CMD_QUEUE_CAPACITY, CommandConsumer, CommandProducer, CommandScheduler,
    CommandSchedulerChannels,
};
pub(crate) use shared::PendingRestartOs;
pub(crate) use shared::build_stream_adapter;
pub use shared::{
    ClapParamPayload, ColdShared, GuiSharedState, LoadModelPayload, NamClapShared,
    NamModelMetadata, PendingModel, PendingRestore, RENDER_MODE_OFFLINE, RENDER_MODE_REALTIME,
    RestoreModelPublish, RestorePublish, RestoreTxn, RtToUi, SlimmableRebuild, StagedRestore,
    StructuralKind, UiToRt,
};

pub(crate) mod main_thread;
pub use main_thread::{NamClapMainThread, debug_assert_main_thread};

use crate::clap::descriptor::nam_descriptor;
use crate::clap::processor::NamClapProcessor;
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// NAM-Plug plugin: main entry point for the CLAP lifecycle.
pub struct NamClapPlugin;

impl Plugin for NamClapPlugin {
    type AudioProcessor<'a> = NamClapProcessor<'a>;
    type Shared<'a> = NamClapShared;
    type MainThread<'a> = NamClapMainThread<'a>;

    fn declare_extensions(
        builder: &mut PluginExtensions<Self>,
        _shared: Option<&Self::Shared<'_>>,
    ) {
        builder.register::<clack_extensions::audio_ports::PluginAudioPorts>();
        builder.register::<clack_extensions::audio_ports_activation::PluginAudioPortsActivation>();
        builder.register::<clack_extensions::params::PluginParams>();
        builder.register::<clack_extensions::state::PluginState>();
        builder.register::<crate::clap::extensions::latency::NamPluginLatency>();
        builder.register::<crate::clap::extensions::track_info::NamPluginTrackInfo>();
        builder.register::<crate::clap::extensions::remote_controls::NamPluginRemoteControls>();
        builder.register::<crate::clap::extensions::param_indication::NamPluginParamIndication>();
        builder.register::<clack_extensions::preset_discovery::PluginPresetLoad>();
        builder.register::<crate::clap::extensions::render::NamPluginRender>();
        builder.register::<crate::clap::extensions::state_context::NamPluginStateContext>();
        builder.register::<crate::clap::extensions::tail::NamPluginTail>();
        builder.register::<crate::clap::extensions::timer::NamPluginTimer>();

        builder.register::<crate::clap::extensions::gui::NamPluginGui>();
    }
}

impl DefaultPluginFactory for NamClapPlugin {
    fn get_descriptor() -> PluginDescriptor {
        nam_descriptor()
    }

    fn new_shared(_host: HostSharedHandle<'_>) -> Result<Self::Shared<'_>, PluginError> {
        static INIT_PANIC_HOOK: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        INIT_PANIC_HOOK.get_or_init(|| {
            neural_amp_modeler_rs::common::panic_hook::install_panic_hook("clap");
        });

        // Initialize and calibrate hardware TSC on a background thread once per process (T-P4.2.3).
        // Non-blocking for the host: during the 60ms calibration window, any initial audio blocks
        // fall back safely to Instant::now(). Once calibrated, hot-path telemetry uses serialized
        // RDTSC directly (~17 ns vs ~1134 ns fallback).
        static INIT_TSC: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        INIT_TSC.get_or_init(|| {
            #[cfg(target_arch = "x86_64")]
            {
                let _ = std::thread::Builder::new()
                    .name("nam-tsc-calib".into())
                    .spawn(neural_amp_modeler_rs::common::tsc::calibrate_tsc);
            }
        });

        // Track active instances for multi-instance panic isolation.
        crate::clap::plugin::shared::bump_active_instances();

        let gui = Arc::new(GuiSharedState::new(shared::next_instance_id()));

        Ok(NamClapShared { gui })
    }

    fn new_main_thread<'a>(
        mut host: HostMainThreadHandle<'a>,
        shared: &'a Self::Shared<'a>,
    ) -> Result<Self::MainThread<'a>, PluginError> {
        // Initial track color query from the host.
        // `HostTrackInfo::get` still takes `&mut` in clack 0.2.0 (only the
        // plugin-side traits flipped to `&self`); `new_main_thread` owns the
        // handle mutably here, so no interior-mutability workaround is needed.
        if let Some(track_info_ext) =
            host.get_extension::<clack_extensions::track_info::HostTrackInfo>()
        {
            let mut buffer = clack_extensions::track_info::TrackInfoBuffer::new();
            if let Some(color) = track_info_ext
                .get(&mut host, &mut buffer)
                .and_then(|info| info.color())
            {
                let packed = crate::clap::extensions::track_info::pack_argb(
                    color.alpha,
                    color.red,
                    color.green,
                    color.blue,
                );
                shared
                    .cold
                    .track_accent_color
                    .store(packed, Ordering::Relaxed);
            }
        }

        // Extracts the Main Thread's exclusive channels from shared state
        let param_tx = shared
            .cold
            .param_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or(PluginError::Message("param_tx producer already taken"))?;

        let gc_rx = shared
            .cold
            .gc_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or(PluginError::Message("gc_rx consumer already taken"))?;

        let slimmable_tx = shared
            .cold
            .slimmable_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or(PluginError::Message("slimmable_tx producer already taken"))?;

        // Register host-log sink with the global NamLogger so that all
        // log::info! / log::warn! / log::error! macros in the CLAP plugin
        // are forwarded to the DAW host's log console.
        {
            use clack_extensions::log::{HostLog, LogSeverity};
            use neural_amp_modeler_rs::common::diagnostics::logger::{
                HostLogFn, LoggerConfig, NamLogger,
            };

            let _logger = NamLogger::init(LoggerConfig {
                level_filter: log::LevelFilter::Info,
                emit_stderr: false,
            });

            if let Some(host_log) = host.get_extension::<HostLog>() {
                let quiescence = Arc::clone(&shared.cold.host_log_quiescence);
                let host_bridge = crate::clap::gui::GuiHostBridge::new(&host);

                let sink: Arc<HostLogFn> = Arc::new(move |severity_str, msg| {
                    let Ok(guard) = quiescence.try_read() else {
                        // Teardown in progress or lock contended — discard log
                        return;
                    };
                    if !*guard {
                        // Plugin destroyed — never dereference the host handle
                        return;
                    }
                    let severity = match severity_str {
                        "ERROR" => LogSeverity::Error,
                        "WARN" => LogSeverity::Warning,
                        "INFO" => LogSeverity::Info,
                        "DEBUG" => LogSeverity::Debug,
                        _ => LogSeverity::Info,
                    };
                    let cmsg = CString::new(msg).unwrap_or_default();
                    let host_shared = host_bridge.as_static();
                    host_log.log(&host_shared, severity, &cmsg);
                });
                if let Some(nl) = NamLogger::global() {
                    nl.register_instance_sink(shared.cold.instance_id, &sink);
                }
                if let Ok(mut guard) = shared.cold.host_log_sink.lock() {
                    *guard = Some(sink);
                }
            }
        }

        let _scope =
            neural_amp_modeler_rs::common::diagnostics::scope_instance(shared.cold.instance_id);

        let cmd_producer = CommandProducer::new(
            param_tx,
            &shared.cold.cmd_next_seq,
            &shared.cold.cmd_last_ack,
        );

        #[cfg_attr(test, allow(unused_mut, clippy::allow_attributes))]
        let main_thread = NamClapMainThread {
            shared,
            // Gate off by default (product decision): the upstream crate default (-70.0 dB)
            // is overridden here to the range minimum (-90.0 dB), the most permissive
            // setting, so the gate practically never closes unless the user/host
            // explicitly tightens the threshold.
            params: RefCell::new(ProcessingParams::builder().with_gate_threshold_db(-90.0)),
            host,
            sys: SystemSnapshot::capture(),
            cmd_producer: RefCell::new(cmd_producer),
            gc_rx: RefCell::new(gc_rx),
            slimmable_tx: RefCell::new(slimmable_tx),
            last_reported_latency: Cell::new(0),
            last_reported_cabsim_tail: Cell::new(0),
            last_seen_slimmable_stale: Cell::new(0),
            slint_window: RefCell::new(None),
            gui_worker: RefCell::new(None),
            dialog_handle: RefCell::new(None),
            dialog_state: shared.cold.dialog_state.clone(),
            ir_dialog_handle: RefCell::new(None),
            ir_dialog_state: shared.cold.ir_dialog_state.clone(),
            gui_lifecycle: Cell::new(crate::clap::gui::lifecycle::GuiLifecycle::Hidden),
            hugepage_synced: Cell::new(false),
            pending_restore: RefCell::new(None),
            staged_restore: RefCell::new(None),
            staged_swap: RefCell::new(None),
            poison_restart_requested: Cell::new(false),
            watchdog_timer: Cell::new(None),
        };

        // Register periodic main-thread watchdog timer (F-NP-R1).
        // A 250ms period guarantees RT poison recovery <= 250ms, latency/PDC sync,
        // and Tier 1 GC ring drain (32 slots) 4x/second without RT thread involvement.
        if let Some(timer_ext) = main_thread
            .host
            .get_extension::<clack_extensions::timer::HostTimer>()
        {
            match timer_ext.register_timer(&main_thread.host, 250) {
                Ok(id) => {
                    main_thread.watchdog_timer.set(Some(id));
                    log::info!(
                        "NAM-Plug: Registered main-thread watchdog timer (id {id}, period 250 ms)"
                    );
                }
                Err(err) => {
                    log::warn!(
                        "NAM-Plug: Failed to register main-thread watchdog timer ({err}); continuing in degraded mode"
                    );
                }
            }
        }

        let host_name = main_thread
            .host
            .name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("Unknown"));
        let negotiated_clap = main_thread.host.clap_version();
        log::info!(
            "NAM-Plug plugin instance created in host \"{host_name}\" (negotiated CLAP API {negotiated_clap})",
        );

        Ok(main_thread)
    }
}

#[cfg(test)]
pub(crate) use shared::make_test_shared;

#[cfg(test)]
#[path = "plugin_mod_test.rs"]
mod plugin_test;

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the `clap_plugin_gui` extension for NAM-Plug.

use crate::clap::gui::GuiHostBridge;
use crate::clap::gui::lifecycle::{GuiEvent, GuiLifecycle};
use crate::clap::gui::{GUI_HEIGHT, GUI_WIDTH};
use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::debug_assert_main_thread;
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, PluginGui, PluginGuiImpl, Window,
};
use clack_plugin::plugin::PluginError;
use slint::ComponentHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Maximum time the main thread waits for a floating window or dialog thread
/// to exit during teardown before handing the handle to a reaper thread.
const TEARDOWN_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Tries to join `handle` until `deadline`. Returns the handle back when the
/// thread is still running after the deadline — the caller must then hand it
/// to a reaper thread (only after all raw pointers have been invalidated).
fn try_join_until(
    handle: std::thread::JoinHandle<()>,
    deadline: std::time::Instant,
) -> Option<std::thread::JoinHandle<()>> {
    while std::time::Instant::now() < deadline {
        if handle.is_finished() {
            let _ = handle.join();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Some(handle)
}

/// Spawns a lightweight "reaper" thread whose sole responsibility is to join
/// `handle` when the target thread finishes, reclaiming its OS resources.
///
/// # Last resort thread resource cleanup
///
/// The reaper must only be spawned after every raw pointer held by the target
/// thread has been invalidated (`alive_fence` lowered): from that point on the
/// window/dialog event loops are guaranteed no-ops, so the detached interval
/// can never dereference freed memory.
fn spawn_reaper(name: &'static str, handle: std::thread::JoinHandle<()>) {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let _ = handle.join();
        })
        .ok(); // Spawn failure — the handle is dropped and the OS reclaims.
}

impl<'a> NamClapMainThread<'a> {
    /// Closes all active GUI windows (embedded and floating) and joins the
    /// associated threads with a bounded timeout.
    ///
    /// Idempotent — safe to call even when no windows are open.
    ///
    /// # GUI teardown lifecycle protocol
    ///
    /// During plugin destruction (`NamClapMainThread::drop`) the caller lowers
    /// `alive_fence` **before** invoking this method, so any GUI thread that
    /// outlives the bounded join can no longer dereference `NamClapShared` or
    /// the host handle (its event loops become no-ops). The reaper thread is
    /// used only as a last resort — after the fence is down it merely reclaims
    /// the OS thread resources.
    pub(crate) fn teardown_gui_resources(&mut self) {
        // 1. Signal the floating window to exit its event loop.
        if let Some(signal) = self.floating_close_signal.take() {
            signal.store(true, Ordering::Release);
        }

        // 2. Quit Slint event loop and clear the window reference.
        let _ = slint::quit_event_loop();
        self.slint_window = None;

        // 3. Join the GUI window thread (bounded). On timeout the handle
        //    goes to a reaper — with the fence down the window event loop is
        //    a no-op from this point on, so no UAF window exists.
        if let Some(handle) = self.floating_thread_handle.take() {
            let deadline = std::time::Instant::now() + TEARDOWN_JOIN_TIMEOUT;
            if let Some(still_running) = try_join_until(handle, deadline) {
                log::warn!(
                    "NAM-Plug: GUI window thread did not exit within {:?} — \
                     handing it to the reaper (thread holds no valid raw pointers \
                     once the fence is lowered)",
                    TEARDOWN_JOIN_TIMEOUT
                );
                spawn_reaper("nam-gui-reaper", still_running);
            }
        }

        // 4. Clear dialog active flags so the UI doesn't show stale Loading
        //    state after the plugin is destroyed.
        if let Some(dialog_state) = &self.shared.cold.dialog_state {
            dialog_state.active.store(false, Ordering::Release);
        }
        if let Some(ir_dialog_state) = &self.shared.cold.ir_dialog_state {
            ir_dialog_state.active.store(false, Ordering::Release);
        }

        // 5. Join dialog threads (model + IR file pickers) with the same
        //    bounded protocol. Dialog threads never dereference
        //    `NamClapShared` (they only touch their Arc-backed state and the
        //    host handle, which the CLAP spec keeps alive for the plugin's
        //    lifetime), so the reaper fallback is purely resource reclamation.
        for sink in [
            &self.shared.cold.dialog_handle_sink,
            &self.shared.cold.ir_dialog_handle_sink,
        ] {
            if let Ok(mut guard) = sink.lock()
                && let Some(h) = guard.take()
            {
                let deadline = std::time::Instant::now() + TEARDOWN_JOIN_TIMEOUT;
                if let Some(still_running) = try_join_until(h, deadline) {
                    spawn_reaper("nam-dialog-reaper", still_running);
                }
            }
        }
    }

    /// Returns the static host handle and shared Arc needed by window callbacks.
    fn host_static_and_shared(
        &self,
    ) -> (
        clack_plugin::host::HostSharedHandle<'static>,
        Arc<crate::clap::plugin::GuiSharedState>,
    ) {
        let bridge = GuiHostBridge::new(&self.host.shared());
        let host_static = bridge.as_static();
        let shared_arc = Arc::clone(&self.shared.gui);
        (host_static, shared_arc)
    }

    /// Spawns the dedicated Slint GUI thread and waits for the window to be created.
    fn spawn_gui(&mut self, window_info: Option<Window>) -> Result<(), PluginError> {
        self.teardown_gui_resources();

        let (host_static, shared_arc) = self.host_static_and_shared();
        let close_signal = Arc::new(AtomicBool::new(false));
        let cs = Arc::clone(&close_signal);

        let (tx, rx) = std::sync::mpsc::sync_channel(1);

        if let Some(ref w) = window_info {
            log::info!("NAM-Plug: spawning Slint GUI (API={:?})", w.api_type().0);
        } else {
            log::info!("NAM-Plug: spawning Slint GUI (floating mode)");
        }

        let thread_builder = std::thread::Builder::new().name("nam-slint-gui".to_string());
        let handle = thread_builder
            .spawn(move || {
                let window = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::clap::gui::MainWindow::new()
                })) {
                    Ok(Ok(w)) => w,
                    Ok(Err(err)) => {
                        log::error!("NAM-Plug: failed to create Slint MainWindow: {err}");
                        let _ = tx.send(Err("Failed to create Slint MainWindow"));
                        return;
                    }
                    Err(_) => {
                        log::error!("NAM-Plug: panic while creating Slint MainWindow");
                        let _ = tx.send(Err("Panic while creating Slint MainWindow"));
                        return;
                    }
                };

                let weak = window.as_weak();
                if tx.send(Ok(weak)).is_err() {
                    log::warn!("NAM-Plug: GUI receiver dropped before window ready");
                    return;
                }

                let host_close_notify = host_static;
                window.window().on_close_requested(move || {
                    if let Some(gui_host) = host_close_notify.get_extension::<HostGui>() {
                        gui_host.closed(&host_close_notify, false);
                    }
                    slint::CloseRequestResponse::HideWindow
                });

                let host_bridge = crate::clap::gui::GuiHostBridge::new(&host_static);
                let _view_model = crate::clap::gui::SlintViewModel::new(
                    window.clone_strong(),
                    shared_arc,
                    Some(host_bridge),
                );

                if let Err(e) = window.show() {
                    log::error!("NAM-Plug: failed to show Slint window: {e}");
                }

                // Run Slint event loop until quit_event_loop() or close_signal
                log::debug!("NAM-Plug: Slint event loop running");
                let _ = slint::run_event_loop();
                cs.store(true, Ordering::Release);
                log::debug!("NAM-Plug: Slint event loop exited");
            })
            .map_err(|e| {
                log::error!("NAM-Plug: failed to spawn nam-slint-gui thread: {e}");
                PluginError::Message("Failed to spawn GUI thread")
            })?;

        // Wait for window creation outcome with bounded timeout
        match rx.recv_timeout(TEARDOWN_JOIN_TIMEOUT) {
            Ok(Ok(weak)) => {
                self.slint_window = Some(weak);
                self.floating_thread_handle = Some(handle);
                self.floating_close_signal = Some(close_signal);
                Ok(())
            }
            Ok(Err(msg)) => {
                close_signal.store(true, Ordering::Release);
                let _ = slint::quit_event_loop();
                self.floating_thread_handle = Some(handle);
                self.floating_close_signal = Some(close_signal);
                Err(PluginError::Message(msg))
            }
            Err(_) => {
                log::error!(
                    "NAM-Plug: GUI initialization timed out after {:?}",
                    TEARDOWN_JOIN_TIMEOUT
                );
                close_signal.store(true, Ordering::Release);
                let _ = slint::quit_event_loop();
                self.floating_thread_handle = Some(handle);
                self.floating_close_signal = Some(close_signal);
                Err(PluginError::Message("GUI initialization timed out"))
            }
        }
    }
}

impl<'a> PluginGuiImpl for NamClapMainThread<'a> {
    /// Indicates whether the given graphics API configuration and floating mode is supported.
    ///
    /// Accepted configurations (per CLAP GUI extension v1.2+):
    /// - **X11 embedded** (preferred on X11 sessions): the host reparents the plugin window.
    /// - **X11 floating** (fallback): the plugin manages its own top-level X11 window.
    /// - **Wayland floating**: the only Wayland mode allowed by the CLAP spec, because the
    ///   Wayland protocol has no generic cross-client window-embedding primitive.
    ///   Wayland embedded is therefore explicitly rejected.
    fn is_api_supported(&mut self, configuration: GuiConfiguration) -> bool {
        match configuration.api_type {
            // X11: both embedded and floating are supported.
            t if t == GuiApiType::X11 => true,
            // Wayland: only floating is supported (no XEmbed equivalent on Wayland).
            t if t == GuiApiType::WAYLAND => configuration.is_floating,
            // All other APIs (WIN32, COCOA, custom) are unsupported on Linux.
            _ => false,
        }
    }

    /// Returns the preferred graphics configuration for the plugin.
    ///
    /// Priority:
    /// 1. **Wayland floating** — when `WAYLAND_DISPLAY` is set (native Wayland session).
    /// 2. **X11 embedded** — default for X11 sessions or XWayland fallback.
    ///
    /// The detection reads an environment variable once on the main thread; it is
    /// strictly off-RT and safe to call here.
    fn get_preferred_api(&mut self) -> Option<GuiConfiguration<'_>> {
        // Detect Wayland session: WAYLAND_DISPLAY being set is the canonical indicator.
        // This is off-RT (main thread only) so std::env::var is acceptable.
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            log::debug!("GUI API negotiation: Wayland session detected via WAYLAND_DISPLAY");
            return Some(GuiConfiguration {
                api_type: GuiApiType::WAYLAND,
                is_floating: true, // Wayland embedding is not supported by the CLAP spec.
            });
        }
        log::debug!("GUI API negotiation: no WAYLAND_DISPLAY, preferring X11 embedded");
        Some(GuiConfiguration {
            api_type: GuiApiType::X11,
            is_floating: false,
        })
    }

    /// Creates and allocates resources for the graphical interface.
    fn create(&mut self, configuration: GuiConfiguration) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        if !self.is_api_supported(configuration) {
            return Err(PluginError::Message("GUI configuration not supported"));
        }
        let api = if configuration.api_type == GuiApiType::WAYLAND {
            "wayland"
        } else {
            "x11"
        };
        let mode = if configuration.is_floating {
            "floating"
        } else {
            "embedded"
        };
        log::info!("GUI create: api={api} mode={mode}");
        {
            self.gui_lifecycle = GuiLifecycle::Hidden;
        }
        Ok(())
    }

    /// Frees the resources allocated for the graphical interface.
    fn destroy(&mut self) {
        debug_assert_main_thread(&self.host);
        {
            // Notify host that the GUI was destroyed by the plugin
            if let Some(gui_host) = self.host.get_extension::<HostGui>() {
                gui_host.closed(&self.host.shared(), true);
            }
            self.teardown_gui_resources();
            let _ = self.gui_lifecycle.transition(GuiEvent::Destroy);
        }
    }

    /// Sets the absolute scale factor for the GUI.
    fn set_scale(&mut self, scale: f64) -> Result<(), PluginError> {
        self.shared
            .cold
            .gui_scale_factor
            .store((scale as f32).to_bits(), Ordering::Relaxed);
        Ok(())
    }

    /// Returns the fixed GUI size (GUI_WIDTH x GUI_HEIGHT pixels).
    fn get_size(&mut self) -> Option<GuiSize> {
        Some(GuiSize {
            width: GUI_WIDTH,
            height: GUI_HEIGHT,
        })
    }

    /// Sets the GUI size. Only the fixed size is accepted.
    fn set_size(&mut self, size: GuiSize) -> Result<(), PluginError> {
        if size.width == GUI_WIDTH && size.height == GUI_HEIGHT {
            Ok(())
        } else {
            Err(PluginError::Message(
                "GUI resizing is not supported in this version",
            ))
        }
    }

    /// Sets the parent window (host) where the GUI should be embedded.
    fn set_parent(&mut self, window: Window) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        log::info!(
            "NAM-Plug: set_parent requested with API {:?}",
            window.api_type().0
        );
        self.spawn_gui(Some(window))
    }

    /// Configures the window to float above the host window (floating fallback mode).
    fn set_transient(&mut self, window: Window) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        log::info!(
            "NAM-Plug: set_transient requested with API {:?}",
            window.api_type().0
        );
        self.spawn_gui(Some(window))
    }

    /// Makes the GUI window visible.
    ///
    /// Transitions the lifecycle state from `Hidden` to `ShowRequested`.
    /// The window show is dispatched to the Slint event loop.
    fn show(&mut self) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        self.gui_lifecycle.transition(GuiEvent::Show)?;
        if let Some(weak) = &self.slint_window {
            let _ = weak.upgrade_in_event_loop(|w| {
                let _ = w.show();
            });
        }
        Ok(())
    }

    /// Hides the GUI window.
    ///
    /// Transitions the lifecycle state from `Active` to `HideRequested`.
    /// The window hide is dispatched to the Slint event loop.
    /// Resources are preserved so `show()` can re-display the window.
    fn hide(&mut self) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        self.gui_lifecycle.transition(GuiEvent::Hide)?;
        if let Some(weak) = &self.slint_window {
            let _ = weak.upgrade_in_event_loop(|w| {
                let _ = w.hide();
            });
        }
        Ok(())
    }

    /// Reports whether the window size can be changed (fixed size).
    fn can_resize(&mut self) -> bool {
        false
    }
}

/// Marker type for extension registration.
pub type NamPluginGui = PluginGui;

#[cfg(test)]
#[path = "gui_test.rs"]
mod gui_test;

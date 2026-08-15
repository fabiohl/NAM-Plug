// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the `clap_plugin_gui` extension for NAM-rs.

use crate::clap::gui::GuiHostBridge;
use crate::clap::gui::lifecycle::{GuiEvent, GuiLifecycle};
use crate::clap::gui::{GUI_HEIGHT, GUI_WIDTH};
use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::debug_assert_main_thread;
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, PluginGui, PluginGuiImpl, Window,
};
use clack_plugin::plugin::PluginError;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Maximum time the main thread waits for a floating window or dialog thread
/// to exit during teardown before handing the handle to a reaper thread.
const TEARDOWN_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Converts a `PluginError` into a `'static` message suitable for storing in
/// cross-thread slots (`PluginError` itself is not `Send` because it can hold
/// a `Box<dyn Error>`).
fn plugin_error_message(err: &PluginError) -> &'static str {
    match err {
        PluginError::Message(msg) => msg,
        PluginError::Error(boxed) => Box::leak(boxed.to_string().into_boxed_str()),
    }
}

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
/// # Last resort only (R-09)
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
    /// # R-09 teardown protocol
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

        // 2. Close the embedded window synchronously (baseview close()).
        if let Some(mut window_handle) = self.window_handle.take() {
            window_handle.close();
        }

        // 3. Join the floating window thread (bounded). On timeout the handle
        //    goes to a reaper — with the fence down the window event loop is
        //    a no-op from this point on, so no UAF window exists.
        if let Some(handle) = self.floating_thread_handle.take() {
            let deadline = std::time::Instant::now() + TEARDOWN_JOIN_TIMEOUT;
            if let Some(still_running) = try_join_until(handle, deadline) {
                log::warn!(
                    "NAM-rs: floating window thread did not exit within {:?} — \
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

    /// Returns the static host handle and shared pointer needed by window callbacks.
    ///
    /// # Safety
    ///
    /// The returned `HostSharedHandle<'static>` and `NamClapSharedRef` must
    /// only be dereferenced while `alive_fence` is up — the window handler
    /// enforces this via `NamPluginWindow::safe_shared` and fence-gated host
    /// calls. `NamClapMainThread::drop` lowers the fence and bounded-joins
    /// every GUI thread before `NamClapShared` is dropped, so no thread can
    /// dereference these pointers after teardown (R-09).
    fn host_static_and_shared(
        &self,
    ) -> (
        clack_plugin::host::HostSharedHandle<'static>,
        crate::clap::plugin::NamClapSharedRef,
    ) {
        let bridge = GuiHostBridge::new(&self.host.shared());
        let host_static = bridge.as_static();
        // SAFETY: self.shared is a valid reference to the plugin shared state.
        // All dereferences from the GUI thread are fenced by `alive_fence`,
        // which is lowered before the shared state is dropped (see above).
        let shared_ptr = unsafe { crate::clap::plugin::NamClapSharedRef::new(self.shared) };
        (host_static, shared_ptr)
    }

    /// Builds the common `baseview::WindowOpenOptions` for both embedded and floating windows.
    ///
    /// CLAP X11 sizes are physical pixels, but baseview interprets `Size` as logical
    /// pixels and applies the scale policy. To create a window with the correct
    /// physical size, we divide the design size by the scale factor and use
    /// `WindowScalePolicy::with_scale_factor` — this ensures the physical window
    /// matches `GUI_WIDTH × GUI_HEIGHT` while egui renders at the correct DPI.
    fn window_options(title: &str, scale_factor: f32) -> baseview::WindowOpenOptions {
        let logical_w = GUI_WIDTH as f64 / scale_factor as f64;
        let logical_h = GUI_HEIGHT as f64 / scale_factor as f64;
        baseview::WindowOpenOptions {
            title: title.to_string(),
            size: baseview::Size::new(logical_w, logical_h),
            scale: baseview::WindowScalePolicy::ScaleFactor(scale_factor as f64),
            gl_config: Some(baseview::gl::GlConfig::default()),
        }
    }
}

impl<'a> PluginGuiImpl for NamClapMainThread<'a> {
    /// Indicates whether the given graphics API configuration and floating mode is supported.
    ///
    /// Accepts X11 both embedded (preferred) and floating (fallback) modes,
    /// so hosts that only offer floating windows are still usable.
    fn is_api_supported(&mut self, configuration: GuiConfiguration) -> bool {
        configuration.api_type == GuiApiType::X11
    }

    /// Returns the preferred graphics configuration for the plugin (embedded X11).
    /// Falls back to floating only when the host does not offer embedded mode.
    fn get_preferred_api(&mut self) -> Option<GuiConfiguration<'_>> {
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
        let mode = if configuration.is_floating {
            "floating"
        } else {
            "embedded"
        };
        log::info!("GUI mode selected = {mode}");
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
        use std::sync::atomic::Ordering;
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
    fn set_parent(&mut self, _window: Window) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        {
            use crate::clap::gui::window::NamPluginWindow;

            if let Some(mut old_handle) = self.window_handle.take() {
                old_handle.close();
            }

            let scale_factor = {
                let stored = self.shared.cold.gui_scale_factor.load(Ordering::Relaxed);
                if stored == 0 {
                    1.0f32
                } else {
                    f32::from_bits(stored)
                }
            };

            let options = Self::window_options("", scale_factor);
            let (host_static, shared_ptr) = self.host_static_and_shared();

            let close_signal = Arc::new(AtomicBool::new(false));
            let cs = Arc::clone(&close_signal);

            let alive_fence = self.shared.cold.alive_fence.clone();

            // R-11: `NamPluginWindow::new` returns a structured error (never
            // panics) and the baseview build callback is additionally wrapped
            // in `catch_unwind`, so a panic can never cross the CLAP FFI
            // boundary into the C++ host. On failure the callback returns a
            // degraded stub window (which closes on its first frame) and the
            // error message is recorded here for a friendly `Err` return.
            // (A `&'static str` is used — `PluginError` itself is not `Send`.)
            let init_outcome = Arc::new(std::sync::Mutex::new(None::<&'static str>));
            let outcome_cb = Arc::clone(&init_outcome);
            let outcome = Arc::clone(&init_outcome);

            // Clones for the `new` call — the originals are moved into
            // `degraded` on the failure arms (match arms are exclusive).
            let cs_for_new = Arc::clone(&close_signal);
            let fence_for_new = Arc::clone(&alive_fence);

            let window_handle = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                baseview::Window::open_parented(&_window, options, move |win| {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        NamPluginWindow::new(
                            win,
                            shared_ptr,
                            host_static,
                            cs_for_new,
                            fence_for_new,
                            scale_factor,
                        )
                    })) {
                        Ok(Ok(window)) => window,
                        Ok(Err(err)) => {
                            log::error!("NAM-rs: GUI initialization failed: {err}");
                            if let Ok(mut guard) = outcome_cb.lock() {
                                *guard = Some(plugin_error_message(&err));
                            }
                            NamPluginWindow::degraded(
                                shared_ptr,
                                host_static,
                                cs,
                                alive_fence,
                                scale_factor,
                            )
                        }
                        Err(_) => {
                            log::error!(
                                "NAM-rs: GUI initialization panicked (caught at FFI boundary)"
                            );
                            if let Ok(mut guard) = outcome_cb.lock() {
                                *guard = Some("GUI initialization failed unexpectedly");
                            }
                            NamPluginWindow::degraded(
                                shared_ptr,
                                host_static,
                                cs,
                                alive_fence,
                                scale_factor,
                            )
                        }
                    }
                })
            }));

            match window_handle {
                Ok(mut window_handle) => {
                    let init_error = outcome.lock().map(|mut guard| guard.take()).unwrap_or(None);
                    if let Some(msg) = init_error {
                        window_handle.close();
                        return Err(PluginError::Message(msg));
                    }
                    self.window_handle = Some(window_handle);
                }
                Err(_) => {
                    return Err(PluginError::Message(
                        "GUI initialization failed (window backend panicked)",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Configures the window to float above the host window (floating fallback mode).
    ///
    /// NOTE: The `_window` parameter provides the host window for a transient-for
    /// stacking relationship (WM_TRANSIENT_FOR). baseview 0.1.1's `open_blocking` API
    /// does not expose transient window support, so the floating window opens as an
    /// independent top-level window. Tracked for future improvement when baseview adds
    /// transient window capabilities.
    fn set_transient(&mut self, _window: Window) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        {
            use crate::clap::gui::window::NamPluginWindow;

            self.teardown_gui_resources();

            let scale_factor = {
                let stored = self.shared.cold.gui_scale_factor.load(Ordering::Relaxed);
                if stored == 0 {
                    1.0f32
                } else {
                    f32::from_bits(stored)
                }
            };

            let options = Self::window_options("NAM-rs", scale_factor);
            let (host_static, shared_ptr) = self.host_static_and_shared();

            let close_signal = Arc::new(AtomicBool::new(false));
            let cs = Arc::clone(&close_signal);
            let window_ready = Arc::new(AtomicBool::new(false));
            let ready = Arc::clone(&window_ready);

            let alive_fence = self.shared.cold.alive_fence.clone();

            // R-11: same fail-closed protocol as `set_parent` — the build
            // callback never panics (Result + catch_unwind) and records the
            // error message so `set_transient` can report a friendly `Err`
            // while the window thread degrades to a stub and exits.
            let init_outcome = Arc::new(std::sync::Mutex::new(None::<&'static str>));
            let outcome_cb = Arc::clone(&init_outcome);
            let outcome = Arc::clone(&init_outcome);

            // Clones for the `new` call — the originals are moved into
            // `degraded` on the failure arms (match arms are exclusive).
            let cs_for_new = Arc::clone(&close_signal);
            let fence_for_new = Arc::clone(&alive_fence);

            let handle = std::thread::spawn(move || {
                baseview::Window::open_blocking(options, move |win| {
                    let window =
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            NamPluginWindow::new(
                                win,
                                shared_ptr,
                                host_static,
                                cs_for_new,
                                fence_for_new,
                                scale_factor,
                            )
                        })) {
                            Ok(Ok(window)) => window,
                            Ok(Err(err)) => {
                                log::error!("NAM-rs: floating GUI initialization failed: {err}");
                                if let Ok(mut guard) = outcome_cb.lock() {
                                    *guard = Some(plugin_error_message(&err));
                                }
                                NamPluginWindow::degraded(
                                    shared_ptr,
                                    host_static,
                                    cs,
                                    alive_fence,
                                    scale_factor,
                                )
                            }
                            Err(_) => {
                                log::error!(
                                    "NAM-rs: floating GUI initialization panicked \
                                 (caught at FFI boundary)"
                                );
                                if let Ok(mut guard) = outcome_cb.lock() {
                                    *guard = Some("GUI initialization failed unexpectedly");
                                }
                                NamPluginWindow::degraded(
                                    shared_ptr,
                                    host_static,
                                    cs,
                                    alive_fence,
                                    scale_factor,
                                )
                            }
                        };
                    ready.store(true, Ordering::Relaxed);
                    window
                });
            });

            // Wait for the window thread to confirm initialization (up to 2 seconds).
            // If NamPluginWindow::new fails or the X11 connection fails,
            // `ready` will never be set and we report the error to the host.
            let start = std::time::Instant::now();
            while !window_ready.load(Ordering::Relaxed) {
                if start.elapsed() > std::time::Duration::from_secs(2) {
                    close_signal.store(true, Ordering::Relaxed);
                    self.floating_thread_handle = Some(handle);
                    self.floating_close_signal = Some(close_signal);
                    return Err(PluginError::Message(
                        "Floating window creation failed: initialization timed out",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            let init_error = outcome.lock().map(|mut guard| guard.take()).unwrap_or(None);
            if let Some(msg) = init_error {
                // The window degraded to a stub — signal it to exit and
                // report the friendly error to the host. The thread is kept
                // for bounded join on teardown.
                close_signal.store(true, Ordering::Relaxed);
                self.floating_thread_handle = Some(handle);
                self.floating_close_signal = Some(close_signal);
                return Err(PluginError::Message(msg));
            }

            self.floating_thread_handle = Some(handle);
            self.floating_close_signal = Some(close_signal);
        }
        Ok(())
    }

    /// Makes the GUI window visible.
    ///
    /// Transitions the lifecycle state from `Hidden` to `ShowRequested`.
    /// The actual window mapping happens on the GUI thread (baseview callback),
    /// which transitions to `Active` once the window is ready.
    fn show(&mut self) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        {
            self.gui_lifecycle.transition(GuiEvent::Show)?;
        }
        Ok(())
    }

    /// Hides the GUI window.
    ///
    /// Transitions the lifecycle state from `Active` to `HideRequested`.
    /// The actual window unmapping happens on the GUI thread.
    /// Resources are preserved so `show()` can re-display the window.
    fn hide(&mut self) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        {
            self.gui_lifecycle.transition(GuiEvent::Hide)?;
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

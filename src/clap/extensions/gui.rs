// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the `clap_plugin_gui` extension for NAM-Plug.

use crate::clap::gui::GuiHostBridge;
use crate::clap::gui::lifecycle::{GuiEvent, GuiLifecycle};
use crate::clap::gui::x11_embed;
use crate::clap::gui::{GUI_HEIGHT, GUI_WIDTH};
use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::debug_assert_main_thread;
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, PluginGui, PluginGuiImpl, Window,
};
use clack_plugin::plugin::PluginError;
use slint::ComponentHandle;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// How the next window reference is actually shown to the user.
///
/// CLAP distinguishes the two by the negotiation path the host took:
/// `set_parent` means *embedded* (the plugin renders inside the host `Window`),
/// while `set_transient` means *floating* (the plugin manages its own
/// top-level window). The windowing implementation — and therefore the honest
/// contract for the current generation — is decided here, never inferred from
/// the window handle alone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GuiWindowMode {
    /// Plugin-owned top-level window (`set_transient`, and the fallback when
    /// embedding cannot be guaranteed).
    Floating,
    /// Native X11 XEmbed child of the host `Window` (`set_parent`, X11 only).
    EmbeddedX11,
}

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

/// Extracts the host X11 `Window` id (`u32` XID) from a CLAP window handle.
///
/// Primary path: `Window::as_x11_handle()` (the `x11` member of the CLAP
/// window union). Fallback: the rwh-0.6 `HasRawWindowHandle` impl behind
/// clack's `raw-window-handle_06` feature, used when the host reports a
/// custom API whose payload is still an X11 id.
fn extract_parent_x11_id(window: &Window<'_>) -> Option<u32> {
    if let Some(handle) = window.as_x11_handle() {
        return u32::try_from(handle).ok();
    }
    if window.to_standard_api_type().is_some() {
        // clack-extensions 0.1.1 implements the rwh-0.6 `HasRawWindowHandle`
        // trait (deprecated upstream in favour of `HasWindowHandle`, but it is
        // the only rwh impl provided by the CLAP crate version we pin).
        #[expect(
            deprecated,
            reason = "clack 0.1.1 only provides the deprecated rwh-0.6 trait"
        )]
        {
            use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
            if let Ok(RawWindowHandle::Xlib(xlib)) = window.raw_window_handle() {
                // rwh 0.6 `XlibWindowHandle.window` is a `c_ulong`; XIDs are u32.
                return u32::try_from(xlib.window).ok();
            }
        }
    }
    None
}

impl<'a> NamClapMainThread<'a> {
    /// Closes the currently open GUI window (embedded or floating) without
    /// shutting down the persistent GUI worker thread.
    ///
    /// Idempotent — safe to call when no window is open. The worker keeps its
    /// Slint platform alive so the next `spawn_gui` reuses it (Slint 1.17
    /// pins the platform to the thread that initialized it).
    ///
    /// The `quit_event_loop` is guarded by `gui_worker.is_some()`: Slint has a
    /// single process-wide event-loop proxy, so an unconditional quit from an
    /// instance that never owned a loop would tear down a *different*
    /// instance's running loop (observable in parallel tests).
    pub(crate) fn close_current_window(&mut self) {
        // 1. Quit the Slint event loop and clear the window reference.
        if self.gui_worker.is_some() {
            let _ = slint::quit_event_loop();
        }
        self.slint_window = None;

        // 2. Clear dialog active flags so the UI doesn't show stale Loading
        //    state after the window is closed.
        if let Some(dialog_state) = &self.shared.cold.dialog_state {
            dialog_state.active.store(false, Ordering::Release);
        }
        if let Some(ir_dialog_state) = &self.shared.cold.ir_dialog_state {
            ir_dialog_state.active.store(false, Ordering::Release);
        }
    }

    /// Tears down every GUI resource of the instance: closes the window,
    /// joins dialog threads, and shuts down the persistent GUI worker with a
    /// bounded join.
    ///
    /// Idempotent — safe to call even when no windows are open.
    ///
    /// # GUI teardown lifecycle protocol
    ///
    /// During plugin destruction (`NamClapMainThread::drop`) the caller lowers
    /// `alive_fence` **before** invoking this method, so any thread that
    /// outlives the bounded join can no longer dereference `NamClapShared` or
    /// the host handle (its event loops become no-ops). The reaper thread is
    /// used only as a last resort — after the fence is down it merely reclaims
    /// the OS thread resources.
    pub(crate) fn teardown_gui_resources(&mut self) {
        self.close_current_window();

        // 1. Shut down the persistent GUI worker (channel close → worker
        //    exits after its current work ends) and bounded-join it. On
        //    timeout the handle goes to a reaper — with the fence down the
        //    worker's event loops are no-ops from this point on, so no UAF
        //    window exists.
        if let Some(worker) = self.gui_worker.take()
            && let Some(handle) = worker.shutdown()
            && let Some(still_running) =
                try_join_until(handle, std::time::Instant::now() + TEARDOWN_JOIN_TIMEOUT)
        {
            log::warn!(
                "NAM-Plug: GUI worker thread did not exit within {:?} — \
                 handing it to the reaper (thread holds no valid raw pointers \
                 once the fence is lowered)",
                TEARDOWN_JOIN_TIMEOUT
            );
            spawn_reaper("nam-gui-reaper", still_running);
        }

        // 2. Join dialog threads (model + IR file pickers) with the same
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
    ///
    /// `mode` decides the windowing contract for this generation:
    /// [`GuiWindowMode::EmbeddedX11`] creates the Slint window as a native
    /// X11 child of the host `Window` (embed-at-creation, `x11_embed`); any
    /// failure to extract the parent id or to pin an X11 event loop returns an
    /// error **before** any window is created, so the host falls back to a
    /// floating negotiation — never a silently-floating embedded contract and
    /// never two windows. Floating generations are prepared via
    /// `x11_embed::prepare_floating` (the dynamic hook becomes a no-op).
    ///
    /// All window generations run on the instance's persistent GUI worker
    /// thread (`gui::worker::GuiWorker`), which owns the Slint platform and
    /// event loop for the instance lifetime — required by Slint 1.17 (the
    /// platform is pinned to the initializing thread) and by the XEmbed hook,
    /// which is installed once on that thread and applied to every generation.
    fn spawn_gui(
        &mut self,
        window_info: Option<Window<'_>>,
        mode: GuiWindowMode,
    ) -> Result<(), PluginError> {
        // Resolve the windowing mode *before* touching the worker: embedded
        // decisions (extraction + process pin check) happen on the host main
        // thread and any failure returns cleanly without a window.
        let (embed_parent, force_x11, backend_text) = match (&window_info, mode) {
            (Some(window), GuiWindowMode::EmbeddedX11) => {
                let api = window.api_type();
                if api != GuiApiType::X11 {
                    return Err(PluginError::Message(concat!(
                        "embedded GUI is supported only on X11 — ",
                        "Wayland has no generic cross-client embedding primitive"
                    )));
                }
                match extract_parent_x11_id(window) {
                    Some(parent_id) => match x11_embed::prepare_embedded_x11(parent_id) {
                        Ok(()) => {
                            log::info!(
                                "NAM-Plug: embedding X11 GUI as child of host window {parent_id:#x}"
                            );
                            (parent_id, true, "X11 (Embedded)".to_string())
                        }
                        Err(reason) => {
                            log::warn!(
                                "NAM-Plug: X11 embedding unavailable, failing set_parent: {reason}"
                            );
                            // clack 0.1.1 hides this error from the host (the
                            // FFI maps set_parent through is_some()), so notify
                            // the host that no GUI exists instead of leaving an
                            // invisible window behind.
                            if let Some(gui_host) = self.host.get_extension::<HostGui>() {
                                gui_host.closed(&self.host.shared(), true);
                            }
                            return Err(PluginError::Message(reason));
                        }
                    },
                    None => {
                        log::warn!("NAM-Plug: host X11 Window id could not be extracted");
                        if let Some(gui_host) = self.host.get_extension::<HostGui>() {
                            gui_host.closed(&self.host.shared(), true);
                        }
                        return Err(PluginError::Message(
                            "host X11 Window id could not be extracted from set_parent",
                        ));
                    }
                }
            }
            (Some(window), GuiWindowMode::Floating) => {
                let api = window.api_type();
                x11_embed::prepare_floating();
                let backend_text = if api == GuiApiType::X11 {
                    "X11 (Floating)".to_string()
                } else if api == GuiApiType::WAYLAND {
                    "Wayland (Floating)".to_string()
                } else {
                    "Floating".to_string()
                };
                (0, api == GuiApiType::X11, backend_text)
            }
            (None, _) => {
                x11_embed::prepare_floating();
                (0, false, "Floating".to_string())
            }
        };

        log::info!(
            "NAM-Plug: spawning Slint GUI (mode={})",
            if embed_parent != 0 {
                "embedded"
            } else {
                "floating"
            }
        );

        // Close any window from a previous generation (the worker persists).
        self.close_current_window();

        // Ensure the persistent per-instance GUI worker exists.
        if self.gui_worker.is_none() {
            let (host_static, shared_arc) = self.host_static_and_shared();
            self.gui_worker = Some(crate::clap::gui::worker::GuiWorker::spawn(
                shared_arc,
                host_static,
            ));
        }

        // Open the window on the worker (bounded wait for creation feedback).
        let worker = self.gui_worker.as_ref().expect("worker just ensured");
        match worker.open_window(crate::clap::gui::worker::GuiWindowJob {
            force_x11,
            backend_text,
        }) {
            Ok(weak) => {
                self.slint_window = Some(weak);
                // Backend feedback: the worker confirmed creation. When the
                // host already requested visibility (ShowRequested) this
                // advances the FSM to Active; otherwise the window is ready
                // but stays hidden until the host's show() promotes it.
                self.promote_window_ready();
                Ok(())
            }
            Err(msg) => Err(PluginError::Message(msg)),
        }
    }
}

impl<'a> PluginGuiImpl for NamClapMainThread<'a> {
    /// Indicates whether the given graphics API configuration and floating mode is supported.
    ///
    /// Accepted configurations (per CLAP GUI extension v1.2+):
    /// - **X11 floating**: the plugin manages its own top-level X11 window.
    /// - **X11 embedded** (E4/Sprint 8): native XEmbed reparenting via
    ///   embed-at-creation, when the process can still honor it (the winit
    ///   event loop is X11 or undecided). The definitive guarantee happens in
    ///   `set_parent`, which returns an error — and no window — when
    ///   embedding cannot be delivered; the host then falls back to floating.
    /// - **Wayland floating**: the only Wayland mode allowed by the CLAP spec,
    ///   because the Wayland protocol has no generic cross-client
    ///   window-embedding primitive.
    ///
    /// Rejected configurations:
    /// - **Wayland embedded**: explicitly rejected by the CLAP spec.
    fn is_api_supported(&mut self, configuration: GuiConfiguration) -> bool {
        match configuration.api_type {
            // X11: floating always; embedded when the embed-at-creation engine
            // is not already pinned to a non-X11 process event loop.
            t if t == GuiApiType::X11 => {
                configuration.is_floating || x11_embed::is_embedded_supported()
            }
            // Wayland: only floating is supported (no XEmbed equivalent on Wayland).
            t if t == GuiApiType::WAYLAND => configuration.is_floating,
            // All other APIs (WIN32, COCOA, custom) are unsupported on Linux.
            _ => false,
        }
    }

    /// Returns the preferred graphics configuration for the plugin.
    ///
    /// Priority:
    /// 1. **Wayland floating** — when `WAYLAND_DISPLAY` is set (native Wayland
    ///    session; the protocol has no generic embedding primitive).
    /// 2. **X11 embedded** — default for X11 sessions and XWayland fallback:
    ///    native XEmbed into the host panel, preferred by Bitwig/REAPER on
    ///    X11. Any embedding failure degrades to the honest floating fallback
    ///    via the returned error path (never a silent downgrade).
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
            is_floating: false, // Native X11 XEmbed into the host panel (E4/Sprint 8).
        })
    }

    /// Creates and allocates resources for the graphical interface.
    ///
    /// Embedded X11 negotiations are validated **here**, because clack 0.1.1's
    /// FFI wrapper never propagates a `set_parent` error to the host (it maps
    /// the plugin result through `is_some()`). Rejecting an unviable embedded
    /// configuration at `create` — which *does* propagate — is therefore the
    /// only honest way to make a conforming host fall back to a floating
    /// negotiation instead of ending up with a silently missing window.
    fn create(&mut self, configuration: GuiConfiguration) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        if !self.is_api_supported(configuration) {
            return Err(PluginError::Message("GUI configuration not supported"));
        }
        // Embedded viability gate (E4/Sprint 8): beyond the backend pin check
        // (is_api_supported), an X11 embedded negotiation also requires a
        // reachable X11 display (pure-Wayland sessions without XWayland have
        // none). The definitive embedding happens at `set_parent`; failing the
        // negotiation here keeps the host honest.
        if !configuration.is_floating
            && configuration.api_type == GuiApiType::X11
            && std::env::var_os("DISPLAY").is_none()
        {
            log::warn!(
                "GUI create: X11 embedded rejected — no X11 display server \
                 (DISPLAY is unset); host should fall back to floating"
            );
            return Err(PluginError::Message(
                "X11 embedded requires a reachable X11 display server (DISPLAY unset)",
            ));
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
            // A new GUI generation must not inherit a close signal from a
            // previous window; the flag belongs to the current window only.
            self.discard_pending_gui_close();
            self.gui_lifecycle = GuiLifecycle::Hidden;
        }
        Ok(())
    }

    /// Frees the resources allocated for the graphical interface.
    ///
    /// Closes the current window but keeps the persistent GUI worker alive:
    /// the host may call `create`/`set_parent` again (Slint 1.17 requires the
    /// platform to live on the thread that initialized it, so re-creation
    /// must reuse this instance's worker).
    fn destroy(&mut self) {
        debug_assert_main_thread(&self.host);
        {
            // Notify host that the GUI was destroyed by the plugin
            if let Some(gui_host) = self.host.get_extension::<HostGui>() {
                gui_host.closed(&self.host.shared(), true);
            }
            self.close_current_window();
            self.discard_pending_gui_close();
            let _ = self.gui_lifecycle.transition(GuiEvent::Destroy);
        }
    }

    /// Sets the absolute scale factor for the GUI.
    ///
    /// The value is stored in `ColdShared::gui_scale_factor` for diagnostics and
    /// future fractional-HiDPI handling. Rendering resolution is *not*
    /// re-applied manually here: in embedded mode the winit backend derives the
    /// child window scale from the X11 screen/`Xft` DPI and applies it
    /// automatically (fractional scaling beyond that is out of E4 scope, see
    /// `functional-tests.md` 3.4.3). Applying the CLAP hint on top of winit's
    /// factor would double-scale the fixed 600×275 logical layout.
    ///
    /// The stored value is off-RT (host main thread) and safe to read from the
    /// GUI thread via `Relaxed` ordering — writing it here uses `Relaxed` too
    /// because the GUI thread only ever *reads* it (plain non-atomic is not
    /// shared; the atomic matches `ColdShared` conventions).
    fn set_scale(&mut self, scale: f64) -> Result<(), PluginError> {
        if scale <= 0.0 || !scale.is_finite() {
            log::warn!("NAM-Plug: set_scale rejected non-positive/non-finite scale {scale}");
            return Err(PluginError::Message(
                "scale must be a positive finite value",
            ));
        }
        log::info!("NAM-Plug: set_scale({scale:.3})");
        self.shared
            .cold
            .gui_scale_factor
            .store((scale as f32).to_bits(), Ordering::Relaxed);
        Ok(())
    }

    /// Returns the fixed GUI size (GUI_WIDTH x GUI_HEIGHT pixels).
    ///
    /// The same logical size is claimed in embedded mode: the X11 child is
    /// created with the `.slint` preferred size, and the host honors the fixed
    /// geomeography via `can_resize() == false`. Host-side panel resizes reach
    /// the child through X11 `ConfigureNotify`, so the viewport scales without
    /// breaking the CLAP size contract.
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
    ///
    /// CLAP semantics: `set_parent` is the *embedded* negotiation. X11 embeds
    /// the Slint window as a native child of the host `Window` (XEmbed,
    /// embed-at-creation, see `x11_embed`).
    ///
    /// # Error propagation caveat
    ///
    /// clack 0.1.1's FFI wrapper maps the plugin's `set_parent` result through
    /// `is_some()`, so a `Err` here is **not** visible to the host. The honest
    /// fallback therefore happens at `create()` (the *embedded viability
    /// gate*), which *does* propagate. The error path in this method remains
    /// as defense-in-depth: it never creates a window, logs the reason loudly,
    /// and notifies the host through `gui_host.closed()`. Wayland is rejected
    /// outright — the protocol has no generic cross-client embedding and
    /// `is_api_supported` already refuses it.
    fn set_parent(&mut self, window: Window) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        log::info!(
            "NAM-Plug: set_parent requested with API {:?}",
            window.api_type().0
        );
        if window.api_type() != GuiApiType::X11 {
            let msg = "embedded GUI is supported only on X11 — Wayland has no \
                       generic cross-client embedding primitive";
            log::error!("NAM-Plug: set_parent rejected: {msg}");
            if let Some(gui_host) = self.host.get_extension::<HostGui>() {
                gui_host.closed(&self.host.shared(), true);
            }
            return Err(PluginError::Message(msg));
        }
        self.spawn_gui(Some(window), GuiWindowMode::EmbeddedX11)
    }

    /// Configures the window to float above the host window (floating mode).
    fn set_transient(&mut self, window: Window) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        log::info!(
            "NAM-Plug: set_transient requested with API {:?}",
            window.api_type().0
        );
        self.spawn_gui(Some(window), GuiWindowMode::Floating)
    }

    /// Makes the GUI window visible.
    ///
    /// A pending backend "user closed" signal is reconciled first, so a close
    /// that raced the host's re-open cannot reject it. The lifecycle then moves
    /// `Hidden → ShowRequested` and dispatches the window show to the Slint
    /// event loop. When the backend window already exists (created by
    /// `set_parent`/`set_transient`), the FSM is promoted to `Active` — the
    /// window was mapped by the backend thread.
    ///
    /// A dispatch failure (no Slint platform, or an event loop already torn
    /// down by a concurrent teardown) leaves the window visibility unchanged,
    /// yet CLAP offers no error better than `Ok(())` for this case. The failure
    /// is therefore logged at `warn` level — both the dispatch result and the
    /// window operation executed on the GUI thread — so the false positive is
    /// observable through the host log console and the diagnostic bundle.
    fn show(&mut self) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        self.reconcile_pending_gui_close();
        self.gui_lifecycle.transition(GuiEvent::Show)?;
        if let Some(weak) = &self.slint_window {
            let lifecycle = self.gui_lifecycle;
            if let Err(e) = weak.upgrade_in_event_loop(move |w| {
                if let Err(e) = w.show() {
                    log::warn!(
                        "NAM-Plug: Slint window show failed (gui_lifecycle={lifecycle:?}): {e}"
                    );
                }
            }) {
                log::warn!(
                    "NAM-Plug: show dispatch failed \
                     (gui_lifecycle={lifecycle:?}, event loop unavailable): {e:?}"
                );
            }
            self.promote_window_ready();
        }
        Ok(())
    }

    /// Hides the GUI window.
    ///
    /// Accepts `Hide` from `Active` and from `ShowRequested` (a host may hide
    /// before the backend confirms `WindowReady`). The window hide is dispatched
    /// to the Slint event loop and, because Slint exposes no asynchronous
    /// "unmapped" signal, the FSM is driven `HideRequested → Hidden`
    /// immediately after the dispatch. Resources are preserved so `show()` can
    /// re-display the window. Any pending close signal is consumed here because
    /// the hide request already realizes the hidden state.
    ///
    /// As in `show()`, a dispatch failure is logged at `warn` level rather than
    /// returned to the host: the FSM still reaches `Hidden` (the host already
    /// considers the window hidden), but the dropped error stays observable.
    fn hide(&mut self) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        self.discard_pending_gui_close();
        self.gui_lifecycle.transition(GuiEvent::Hide)?;
        if let Some(weak) = &self.slint_window {
            let lifecycle = self.gui_lifecycle;
            if let Err(e) = weak.upgrade_in_event_loop(move |w| {
                if let Err(e) = w.hide() {
                    log::warn!(
                        "NAM-Plug: Slint window hide failed (gui_lifecycle={lifecycle:?}): {e}"
                    );
                }
            }) {
                log::warn!(
                    "NAM-Plug: hide dispatch failed \
                     (gui_lifecycle={lifecycle:?}, event loop unavailable): {e:?}"
                );
            }
        }
        let _ = self.gui_lifecycle.transition(GuiEvent::WindowHidden);
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

#[cfg(test)]
#[path = "gui_lifecycle_integration_test.rs"]
mod gui_lifecycle_integration_test;

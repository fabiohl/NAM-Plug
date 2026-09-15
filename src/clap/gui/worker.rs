// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Persistent per-instance GUI worker thread.
//!
//! Slint 1.17 pins the platform — and with it the winit event loop and the
//! X11 XEmbed hook — to the **thread that first initializes it**. A
//! per-generation "spawn a thread per window" model therefore breaks the very
//! first `destroy()` + re-open cycle of a real window: the new thread has no
//! platform and the process-global event-loop proxy forbids a second one
//! ("The Slint platform was initialized in another thread").
//!
//! This module fixes that by hosting **one** GUI thread per plugin instance
//! that outlives `gui.destroy()`: the platform is initialized on it exactly
//! once, every window generation reuses the same event loop, and the embedded
//! X11 hook (process-global, `x11_embed`) is applied on every window creation
//! with the parent id negotiated per generation. The thread is joined only
//! when the plugin instance itself is destroyed.
//!
//! # Lifetime protocol
//!
//! - [`GuiWorker::spawn`] starts the thread lazily at the first GUI open.
//! - [`GuiWorker::open_window`] queues a window generation and waits (bounded)
//!   for the backend to confirm creation — same protocol as the old
//!   per-generation thread, now routed through the persistent worker.
//! - [`GuiWorker::close_window`] stops the current event loop via the Slint
//!   proxy; the worker keeps running, ready for the next `open_window`.
//! - [`GuiWorker::shutdown`] drops the job channel (the worker exits as soon
//!   as its current work ends) and returns the join handle so the caller can
//!   bounded-join it and fall back to the `nam-gui-reaper` thread.
//!
//! Multi-instance note: Slint 1.17 allows a single platform per process, so a
//! second instance opening a GUI while a first instance's worker is alive
//! fails that instance's `set_parent`/`set_transient` cleanly (honest error,
//! host falls back / reports). This is a documented Slint constraint, not a
//! teardown bug; see `docs/architecture.md` §7.5.

use crate::clap::gui::MainWindow;
use crate::clap::gui::x11_embed;
use crate::clap::plugin::GuiSharedState;
use clack_plugin::host::HostSharedHandle;
use slint::ComponentHandle;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

/// Bounded wait for the backend to confirm window creation.
const OPEN_WINDOW_TIMEOUT: Duration = Duration::from_secs(2);

/// Job sent from the host main thread to the persistent GUI worker.
pub(crate) struct GuiWindowJob {
    /// Whether the winit event loop must be forced to X11 (embedded always;
    /// X11 floating when the host negotiated X11; Wayland floating leaves the
    /// native auto-selection). The embed parent id itself is already recorded
    /// in the process-global `x11_embed` target by the main thread before this
    /// job is queued.
    pub(crate) force_x11: bool,
    /// Status-bar backend label for this generation (Finding F6).
    pub(crate) backend_text: String,
}

/// Job sent from the host main thread to the persistent GUI worker.
pub(crate) enum GuiWorkerJob {
    OpenWindow {
        job: GuiWindowJob,
        reply: mpsc::Sender<Result<slint::Weak<MainWindow>, &'static str>>,
    },
}

/// Persistent GUI worker for one plugin instance.
pub(crate) struct GuiWorker {
    tx: mpsc::Sender<GuiWorkerJob>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl GuiWorker {
    /// Spawns the persistent `nam-slint-gui` thread.
    ///
    /// The thread owns the Slint platform for the instance and keeps
    /// `shared_arc` and `host_static` alive for the window callbacks
    /// (fence-gated no-ops once the instance is destroyed).
    pub(crate) fn spawn(
        shared_arc: Arc<GuiSharedState>,
        host_static: HostSharedHandle<'static>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<GuiWorkerJob>();
        let handle = std::thread::Builder::new()
            .name("nam-slint-gui".to_string())
            .spawn(move || worker_main(rx, shared_arc, host_static))
            .expect("failed to spawn the persistent GUI worker thread");
        Self {
            tx,
            handle: Some(handle),
        }
    }

    /// Queues a window generation and waits (bounded) for the backend to
    /// confirm creation. The window is created on this instance's persistent
    /// worker thread, which owns the Slint platform and event loop.
    pub(crate) fn open_window(
        &self,
        job: GuiWindowJob,
    ) -> Result<slint::Weak<MainWindow>, &'static str> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(GuiWorkerJob::OpenWindow { job, reply })
            .map_err(|_| "the GUI worker is shutting down")?;
        rx.recv_timeout(OPEN_WINDOW_TIMEOUT)
            .map_err(|_| "GUI window creation timed out")?
    }

    /// Stops the worker: the channel is closed so the thread exits as soon as
    /// its current work ends. Returns the join handle for a bounded join
    /// (the caller hands it to the reaper on timeout).
    pub(crate) fn shutdown(mut self) -> Option<std::thread::JoinHandle<()>> {
        drop(self.tx);
        // `self.tx` is dropped; the receiver errors and the worker exits.
        self.handle.take()
    }

    /// Test-only constructor: injects a fake worker thread.
    #[cfg(test)]
    pub(crate) fn from_parts_for_test(
        tx: mpsc::Sender<GuiWorkerJob>,
        handle: std::thread::JoinHandle<()>,
    ) -> Self {
        Self {
            tx,
            handle: Some(handle),
        }
    }
}

/// The worker loop: owns the Slint platform for the instance lifetime.
fn worker_main(
    rx: mpsc::Receiver<GuiWorkerJob>,
    shared_arc: Arc<GuiSharedState>,
    host_static: HostSharedHandle<'static>,
) {
    while let Ok(job) = rx.recv() {
        match job {
            GuiWorkerJob::OpenWindow { job, reply } => {
                run_window_generation(job, reply, Arc::clone(&shared_arc), host_static);
            }
        }
    }
    log::debug!("NAM-Plug: persistent GUI worker exiting");
}

/// Creates, shows, and runs one window generation on the worker thread.
fn run_window_generation(
    job: GuiWindowJob,
    reply: mpsc::Sender<Result<slint::Weak<MainWindow>, &'static str>>,
    shared_arc: Arc<GuiSharedState>,
    host_static: HostSharedHandle<'static>,
) {
    // The Slint platform is pinned to this thread: the embedded path selects
    // the winit backend here (with the X11 event loop + dynamic embed hook),
    // before the first window of the process is created. Floating generations
    // pin the backend too (force_x11 per the negotiated API) so the process
    // event-loop kind is known deterministically.
    if let Err(msg) = x11_embed::ensure_backend_on_gui_thread(job.force_x11) {
        log::warn!("NAM-Plug: GUI backend unavailable: {msg}");
        let _ = reply.send(Err(msg));
        return;
    }

    let window = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(MainWindow::new)) {
        Ok(Ok(w)) => w,
        Ok(Err(err)) => {
            log::error!("NAM-Plug: failed to create Slint MainWindow: {err}");
            let _ = reply.send(Err("Failed to create Slint MainWindow"));
            return;
        }
        Err(_) => {
            log::error!("NAM-Plug: panic while creating Slint MainWindow");
            let _ = reply.send(Err("Panic while creating Slint MainWindow"));
            return;
        }
    };

    let weak = window.as_weak();
    if reply.send(Ok(weak)).is_err() {
        // The host main thread gave up waiting (bounded open): drop the
        // window without running the loop; the next generation reuses this
        // thread's platform.
        log::warn!("NAM-Plug: GUI open request dropped before window ready");
        return;
    }

    let host_close_notify = host_static;
    let shared_close_notify = Arc::clone(&shared_arc);
    window.window().on_close_requested(move || {
        // Publish the backend "user closed" signal for the main thread. The
        // host-facing notification is gated by the teardown fence: after it
        // drops the plugin instance is being (or has been) destroyed, and
        // `request_callback` would dispatch into a freed instance.
        shared_close_notify
            .cold
            .gui_user_closed
            .store(true, Ordering::Release);
        log::debug!("NAM-Plug: GUI user closed");
        if shared_close_notify.cold.alive_fence.load(Ordering::Acquire) {
            if let Some(gui_host) =
                host_close_notify.get_extension::<clack_extensions::gui::HostGui>()
            {
                gui_host.closed(&host_close_notify, false);
            }
            host_close_notify.request_callback();
        }
        slint::CloseRequestResponse::HideWindow
    });

    let host_bridge = crate::clap::gui::GuiHostBridge::new(&host_static);
    let _view_model = crate::clap::gui::SlintViewModel::new(
        window.clone_strong(),
        shared_arc,
        Some(host_bridge),
        &job.backend_text,
    );

    if let Err(e) = window.show() {
        log::error!("NAM-Plug: failed to show Slint window: {e}");
    }

    // Run the event loop until `quit_event_loop()` (host hide/destroy) — the
    // platform persists on this thread for the next generation.
    let _ = slint::run_event_loop();

    // Release the window in the right order so the X11 child is really
    // destroyed before the next generation:
    //  1. `_view_model` holds a strong window clone;
    //  2. `window.window().hide()` drops the additional strong component
    //     reference that `show()` maintains ("... while the window is visible");
    //  3. winit only *queues* `XDestroyWindow` on window drop, and the
    //     persistent worker keeps the connection alive — so one drained loop
    //     iteration flushes the destroy to the server.
    drop(_view_model);
    let _ = window.window().hide();
    drop(window);
    if slint::invoke_from_event_loop(|| {
        let _ = slint::quit_event_loop();
    })
    .is_ok()
    {
        let _ = slint::run_event_loop();
    }
}

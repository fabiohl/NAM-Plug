// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the GUI teardown lifecycle protocol.
//!
//! Covers the bounded-join teardown helpers and the `NamClapMainThread::drop`
//! contract: the alive fence is lowered before teardown, and any fake
//! "floating window" thread is synchronously closed and joined — never left
//! dereferencing `NamClapShared` after the plugin instance is destroyed.

use super::{spawn_reaper, try_join_until};
use crate::clap::gui::worker::{GuiWindowJob, GuiWorker, GuiWorkerJob};
use crate::clap::plugin::NamClapMainThread;
use crate::clap::test_util::{self, make_test_plugin};
use clack_extensions::gui::{GuiApiType, GuiConfiguration, PluginGuiImpl};
use clack_host::plugin::PluginInstance;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Spawns a thread that mimics the persistent GUI worker: it blocks until its
/// job channel is closed (the `GuiWorker::shutdown` contract) and reports its
/// exit through `exited_tx`.
fn fake_gui_worker_thread(
    exited_tx: mpsc::Sender<()>,
) -> (mpsc::Sender<GuiWorkerJob>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<GuiWorkerJob>();
    let handle = std::thread::spawn(move || {
        let _ = rx.recv(); // exits when the worker's sender is dropped
        let _ = exited_tx.send(());
    });
    (tx, handle)
}

/// Returns a shared reference to the main thread struct of a test plugin.
///
/// The reference is only valid while `instance` is alive; callers must not
/// use it after the instance is dropped.
///
/// Shared (not mutable): clack 0.2.0 exposes the main thread through `&self`
/// ("Embrace Reentrancy"); interior mutability (`Cell`/`RefCell`) keeps the
/// staged slots reachable without `&mut`.
fn main_thread_mut(instance: &mut PluginInstance<test_util::TestHost>) -> &NamClapMainThread<'_> {
    let raw_ptr = instance.plugin_handle().as_raw_ptr();
    let ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::plugin::NamClapPlugin>::handle(
            raw_ptr,
            |wrapper| Ok(wrapper.main_thread() as *const NamClapMainThread<'_>),
        )
    }
    .expect("Failed to get plugin wrapper");
    // SAFETY: the plugin instance outlives the returned reference, and the
    // wrapper guarantees main-thread exclusivity while it is alive.
    unsafe { &*ptr }
}

// ---------------------------------------------------------------------------
// Bounded-join helpers
// ---------------------------------------------------------------------------

#[test]
fn test_try_join_until_joins_finished_thread() {
    let handle = std::thread::spawn(|| {});
    let deadline = Instant::now() + Duration::from_secs(2);
    let still_running = try_join_until(handle, deadline);
    assert!(still_running.is_none(), "finished thread must be joined");
}

#[test]
fn test_try_join_until_times_out_and_returns_handle_for_reaper() {
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let _ = rx.recv(); // runs until released by the test
    });
    let deadline = Instant::now() + Duration::from_millis(50);
    let still_running = try_join_until(handle, deadline)
        .expect("a still-running thread must be returned to the caller");
    // Last resort: the reaper reclaims the thread once it exits.
    spawn_reaper("test-reaper", still_running);
    let _ = tx.send(());
    std::thread::sleep(Duration::from_millis(50));
}

// ---------------------------------------------------------------------------
// Drop teardown contract
// ---------------------------------------------------------------------------

/// Destroying the plugin without ever calling `gui.destroy()` must lower the
/// alive fence and synchronously close + join the persistent GUI worker
/// thread — before `NamClapShared` is dropped.
#[test]
fn test_drop_teardown_lowers_fence_and_joins_worker() {
    let (_entry, _host_info, mut plugin_instance) = make_test_plugin();

    let shared_ptr = test_util::extract_shared(&mut plugin_instance);
    // SAFETY: the plugin instance outlives this reference (dropped below).
    let shared = unsafe { &*shared_ptr };
    let fence = std::sync::Arc::clone(&shared.cold.alive_fence);
    assert!(
        fence.load(std::sync::atomic::Ordering::Relaxed),
        "fence must start raised"
    );

    let (exited_tx, exited) = mpsc::channel();
    let (tx, handle) = fake_gui_worker_thread(exited_tx);

    let mt = main_thread_mut(&mut plugin_instance);
    mt.gui_worker
        .replace(Some(GuiWorker::from_parts_for_test(tx, handle)));

    // Host destroys the plugin without gui.destroy().
    drop(plugin_instance);

    assert!(
        !fence.load(std::sync::atomic::Ordering::Relaxed),
        "fence must be lowered before shared state is released"
    );
    exited
        .recv_timeout(Duration::from_secs(1))
        .expect("GUI worker must have been closed and joined during drop");
}

/// Repeatedly opening and destroying the plugin with an active GUI worker
/// must never panic, hang, or leave the fence raised (acceptance: stress with
/// rapid GUI open/close).
#[test]
fn test_stress_rapid_teardown_cycles() {
    for cycle in 0..20 {
        let (_entry, _host_info, mut plugin_instance) = make_test_plugin();

        let shared_ptr = test_util::extract_shared(&mut plugin_instance);
        // SAFETY: the plugin instance outlives this reference (dropped below).
        let shared = unsafe { &*shared_ptr };
        let fence = std::sync::Arc::clone(&shared.cold.alive_fence);

        let (exited_tx, _exited) = mpsc::channel();
        let (tx, handle) = fake_gui_worker_thread(exited_tx);

        let mt = main_thread_mut(&mut plugin_instance);
        mt.gui_worker
            .replace(Some(GuiWorker::from_parts_for_test(tx, handle)));

        drop(plugin_instance);

        assert!(
            !fence.load(std::sync::atomic::Ordering::Relaxed),
            "cycle {cycle}: fence must be lowered after teardown"
        );
    }
}

// ---------------------------------------------------------------------------
// Dispatch observability
// ---------------------------------------------------------------------------

/// A visibility dispatch that reaches a dead event loop must be reported,
/// never silently dropped.
///
/// The test installs a window handle whose Slint event loop is unavailable —
/// the headless test process never starts a Slint platform — and lowers the
/// alive fence first, mirroring a teardown running concurrently with the
/// host's `show()`/`hide()`. Both calls must stay best-effort (`Ok`) for the
/// host while emitting a `warn` record that reaches the registered log sink.
#[test]
fn show_after_teardown_logs_warn() {
    let (_entry, _host_info, mut plugin_instance) = make_test_plugin();
    let (captured, _sink) = test_util::register_test_sink();

    let shared_ptr = test_util::extract_shared(&mut plugin_instance);
    // SAFETY: the plugin instance outlives this reference for the whole test.
    let shared = unsafe { &*shared_ptr };
    shared.cold.alive_fence.store(false, Ordering::Release);

    let mt = main_thread_mut(&mut plugin_instance);
    // A handle whose event loop is gone: the dispatch must fail observably.
    mt.slint_window
        .replace(Some(slint::Weak::<crate::clap::gui::MainWindow>::default()));

    let show = mt.show();
    assert!(
        show.is_ok(),
        "show() must remain best-effort once the event loop is gone, got {show:?}"
    );
    let hide = mt.hide();
    assert!(
        hide.is_ok(),
        "hide() must remain best-effort once the event loop is gone, got {hide:?}"
    );

    let messages = captured.lock().unwrap();
    assert!(
        messages
            .iter()
            .any(|(severity, msg)| *severity == "WARN" && msg.contains("show dispatch failed")),
        "a failed show dispatch must be logged at warn level.\nCaptured: {messages:#?}"
    );
    assert!(
        messages
            .iter()
            .any(|(severity, msg)| *severity == "WARN" && msg.contains("hide dispatch failed")),
        "a failed hide dispatch must be logged at warn level.\nCaptured: {messages:#?}"
    );
}

// ---------------------------------------------------------------------------
// Pre-warm (F-UX-05 / T5.1) and open_window timing (T5.2)
// ---------------------------------------------------------------------------

/// X11 floating configuration used by the pre-warm tests.
fn x11_floating_cfg() -> GuiConfiguration<'static> {
    GuiConfiguration {
        api_type: GuiApiType::X11,
        is_floating: true,
    }
}

/// `create()` must eagerly spawn the persistent GUI worker so the Slint
/// platform is pre-warmed before the first window (F-UX-05 / T5.1). The
/// headless test process never shows a window; only the eager spawn is
/// observable here.
#[test]
fn test_create_spawns_gui_worker_for_prewarm() {
    let (_entry, _host_info, mut plugin_instance) = make_test_plugin();
    let mt = main_thread_mut(&mut plugin_instance);
    mt.create(x11_floating_cfg())
        .expect("create(X11 floating) must succeed");
    assert!(
        mt.gui_worker.borrow().is_some(),
        "create() must spawn the GUI worker eagerly so the platform can be pre-warmed"
    );
}

/// `warm_up` must dispatch a `WarmUp` job (fire-and-forget) carrying the
/// negotiated backend hint; the worker processes it before any later
/// `open_window`.
#[test]
fn test_warm_up_dispatches_warmup_job() {
    let (tx, rx) = mpsc::channel::<GuiWorkerJob>();
    let handle = std::thread::spawn(|| {});
    let worker = GuiWorker::from_parts_for_test(tx, handle);

    worker.warm_up(true);

    let job = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("warm_up must dispatch a job");
    assert!(
        matches!(job, GuiWorkerJob::WarmUp { force_x11: true }),
        "warm_up(true) must dispatch WarmUp{{force_x11: true}}"
    );
}

/// `open_window` must measure and log the host-main-thread block time at info
/// level (the F-UX-05/T5.2 p95 < 50 ms budget). A fake worker replies
/// immediately so the assertion is on the log record, not on real timing.
#[test]
fn test_open_window_logs_main_thread_block_time() {
    let (_entry, _host_info, _plugin_instance) = make_test_plugin();
    let (captured, _sink) = test_util::register_test_sink();

    let (tx, rx) = mpsc::channel::<GuiWorkerJob>();
    let handle = std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            if let GuiWorkerJob::OpenWindow { reply, .. } = job {
                let _ = reply.send(Ok(slint::Weak::<crate::clap::gui::MainWindow>::default()));
            }
        }
    });
    let worker = GuiWorker::from_parts_for_test(tx, handle);

    let result = worker.open_window(GuiWindowJob {
        force_x11: false,
        backend_text: "Test".to_string(),
    });
    assert!(
        result.is_ok(),
        "the fake worker replies Ok, got err={:?}",
        result.err()
    );

    let messages = captured.lock().unwrap();
    assert!(
        messages.iter().any(|(severity, msg)| {
            severity == "INFO" && msg.contains("GUI open_window completed in")
        }),
        "open_window must log its main-thread block time at info level.\nCaptured: {messages:#?}"
    );
}

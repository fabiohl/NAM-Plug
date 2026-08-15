// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the R-09 GUI teardown protocol (Sprint 6 · S6.T1).
//!
//! Covers the bounded-join teardown helpers and the `NamClapMainThread::drop`
//! contract: the alive fence is lowered before teardown, and any fake
//! "floating window" thread is synchronously closed and joined — never left
//! dereferencing `NamClapShared` after the plugin instance is destroyed.

use super::{spawn_reaper, try_join_until};
use crate::clap::plugin::NamClapMainThread;
use crate::clap::test_util::{self, make_test_plugin};
use clack_host::plugin::PluginInstance;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Spawns a thread that mimics the floating window event loop: it polls the
/// close signal every ~1 ms and exits when the window is closed, reporting
/// its exit through the returned receiver.
fn fake_floating_window_thread(
    close_signal: &Arc<AtomicBool>,
) -> (std::thread::JoinHandle<()>, mpsc::Receiver<()>) {
    let (tx, rx) = mpsc::channel();
    let cs = Arc::clone(close_signal);
    let handle = std::thread::spawn(move || {
        while !cs.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = tx.send(());
    });
    (handle, rx)
}

/// Returns a mutable reference to the main thread struct of a test plugin.
///
/// The reference is only valid while `instance` is alive; callers must not
/// use it after the instance is dropped.
fn main_thread_mut(
    instance: &mut PluginInstance<test_util::TestHost>,
) -> &mut NamClapMainThread<'_> {
    let raw_ptr = instance.plugin_handle().as_raw_ptr();
    let mut nn = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::plugin::NamClapPlugin>::handle(
            raw_ptr,
            |wrapper| Ok(wrapper.main_thread()),
        )
    }
    .expect("Failed to get plugin wrapper");
    // SAFETY: the plugin instance is uniquely borrowed for the duration of
    // the caller's use; the wrapper guarantees main-thread exclusivity and
    // nothing else aliases this struct while the caller holds the reference.
    unsafe { nn.as_mut() }
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
// Drop teardown contract (R-09)
// ---------------------------------------------------------------------------

/// Destroying the plugin without ever calling `gui.destroy()` must lower the
/// alive fence and synchronously close + join any floating window thread —
/// before `NamClapShared` is dropped.
#[test]
fn test_drop_teardown_lowers_fence_and_joins_floating_thread() {
    let (_entry, _host_info, mut plugin_instance) = make_test_plugin();

    let shared_ptr = test_util::extract_shared(&mut plugin_instance);
    // SAFETY: the plugin instance outlives this reference (dropped below).
    let shared = unsafe { &*shared_ptr };
    let fence = Arc::clone(&shared.cold.alive_fence);
    assert!(fence.load(Ordering::Relaxed), "fence must start raised");

    let close_signal = Arc::new(AtomicBool::new(false));
    let (handle, exited) = fake_floating_window_thread(&close_signal);

    let mt = main_thread_mut(&mut plugin_instance);
    mt.floating_thread_handle = Some(handle);
    mt.floating_close_signal = Some(close_signal);

    // The R-09 scenario: host destroys the plugin without gui.destroy().
    drop(plugin_instance);

    assert!(
        !fence.load(Ordering::Relaxed),
        "fence must be lowered before shared state is released"
    );
    exited
        .recv_timeout(Duration::from_secs(1))
        .expect("floating thread must have been closed and joined during drop");
}

/// Repeatedly opening and destroying the plugin with an active floating
/// window must never panic, hang, or leave the fence raised (R-09 acceptance:
/// stress with rapid floating window open/close).
#[test]
fn test_stress_rapid_teardown_cycles() {
    for cycle in 0..20 {
        let (_entry, _host_info, mut plugin_instance) = make_test_plugin();

        let shared_ptr = test_util::extract_shared(&mut plugin_instance);
        // SAFETY: the plugin instance outlives this reference (dropped below).
        let shared = unsafe { &*shared_ptr };
        let fence = Arc::clone(&shared.cold.alive_fence);

        let close_signal = Arc::new(AtomicBool::new(false));
        let (handle, _exited) = fake_floating_window_thread(&close_signal);

        let mt = main_thread_mut(&mut plugin_instance);
        mt.floating_thread_handle = Some(handle);
        mt.floating_close_signal = Some(close_signal);

        drop(plugin_instance);

        assert!(
            !fence.load(Ordering::Relaxed),
            "cycle {cycle}: fence must be lowered after teardown"
        );
    }
}

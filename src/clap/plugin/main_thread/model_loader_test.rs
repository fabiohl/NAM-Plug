// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Tests for the asynchronous per-instance model loader (Épico B / F-UX-01).

use crate::clap::host_harness::{
    extract_plugin_main_thread, extract_plugin_shared, make_harness_audio_processor,
    make_test_plugin_with_harness,
};
use crate::clap::plugin::main_thread::load::{set_test_mid_load_hook, test_mid_load_hook_lock};
use crate::clap::plugin::shared::{
    MAX_PENDING_MODEL_REQUESTS, ModelRequestOrigin, PendingModelRequest, RENDER_MODE_OFFLINE,
    RENDER_MODE_REALTIME,
};
use crate::clap::test_util;
use clack_common::process::PluginAudioConfiguration;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Copies a fixture model to a uniquely named temporary file so the shared
/// `TEST_MID_LOAD_HOOK` (a process-global) can be targeted at one load without
/// interfering with other tests loading the same fixture.
fn temp_model(name: &str) -> PathBuf {
    let src = test_util::model_path(name);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dst = std::env::temp_dir().join(format!(
        "nam_loader_test_{}_{}_{}",
        std::process::id(),
        nanos,
        name
    ));
    std::fs::copy(&src, &dst).expect("copy model fixture");
    dst
}

fn push_gui(shared: &crate::clap::plugin::NamClapShared, path: &Path) {
    shared
        .cold
        .pending_model_requests
        .lock()
        .unwrap()
        .push_back(PendingModelRequest {
            path: path.to_path_buf(),
            origin: ModelRequestOrigin::Gui,
        });
}

fn clear_hook() {
    set_test_mid_load_hook::<fn(&Path)>(None);
}

/// Basename of a path as the loader publishes it in `ui_model_name`.
fn basename(path: &Path) -> String {
    path.file_name().unwrap().to_str().unwrap().to_string()
}

/// T4.2: two rapid GUI loads A→B install only B. While A is in flight, accepting
/// B invalidates A so its result is discarded, and B becomes the active model.
#[test]
fn test_rapid_gui_loads_last_wins_only_b_installed() {
    let _hook_guard = test_mid_load_hook_lock();
    let (_entry, _host_info, mut instance, _state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let a = temp_model("lstm.nam");
    let b = temp_model("a2_example.nam");
    let b_name = basename(&b);

    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let a_for_hook = a.clone();
    set_test_mid_load_hook(Some(move |path: &Path| {
        if path == a_for_hook.as_path() {
            let _ = started_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
        }
    }));

    // Accept A and dispatch it; the worker stalls inside `prepare_model`.
    push_gui(shared, &a);
    instance.call_on_main_thread_callback();
    started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("A's prepare_model did not start");

    // Accept B while A is in flight: the newer GUI request supersedes A.
    push_gui(shared, &b);
    instance.call_on_main_thread_callback();

    // Let A finish; its superseded result must be discarded.
    release_tx.send(()).expect("release A");
    test_util::drain_async_model_loading(&mut instance, shared);
    clear_hook();

    assert_eq!(
        *shared.cold.ui_model_name.lock().unwrap(),
        b_name,
        "only the last requested model (B) must be active"
    );
    assert_eq!(
        shared.cold.model_load_counter.load(Ordering::Relaxed),
        1,
        "the superseded load A must never be committed"
    );

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

/// T4.2: the secondary `pending_async_loads` queue stays bounded while the
/// worker is busy, and an evicted preset is reported to the host.
#[test]
fn test_secondary_queue_stays_bounded() {
    let _hook_guard = test_mid_load_hook_lock();
    let (_entry, _host_info, mut instance, _state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };
    let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };

    // Stall the first (heavy) load so the worker stays busy while presets pile up.
    let stall = temp_model("a2_example.nam");
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let stall_for_hook = stall.clone();
    set_test_mid_load_hook(Some(move |path: &Path| {
        if path == stall_for_hook.as_path() {
            let _ = started_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
        }
    }));

    push_gui(shared, &stall);
    instance.call_on_main_thread_callback();
    started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("stalled prepare_model did not start");

    let model = test_util::model_path("lstm.nam");
    let location = CString::new(model.to_str().unwrap()).unwrap();
    for _ in 0..(MAX_PENDING_MODEL_REQUESTS * 2) {
        shared
            .cold
            .pending_model_requests
            .lock()
            .unwrap()
            .push_back(PendingModelRequest {
                path: model.clone(),
                origin: ModelRequestOrigin::Preset {
                    location: location.clone(),
                    load_key: None,
                },
            });
        instance.call_on_main_thread_callback();
    }

    assert!(
        mt.pending_async_loads.borrow().len() <= MAX_PENDING_MODEL_REQUESTS,
        "the secondary queue must not exceed MAX_PENDING_MODEL_REQUESTS, got {}",
        mt.pending_async_loads.borrow().len()
    );

    release_tx.send(()).expect("release stalled load");
    test_util::drain_async_model_loading(&mut instance, shared);
    clear_hook();
    let _ = std::fs::remove_file(&stall);
}

/// T4.2: an error in A must not overwrite the success of a later B.
#[test]
fn test_error_then_success_keeps_success() {
    use crate::clap::test_util::write_invalid_model_fixture;

    let (_entry, _host_info, mut instance, _state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let bad = std::env::temp_dir().join(format!(
        "nam_loader_bad_{}_{}.nam",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_invalid_model_fixture(&bad);
    let good = temp_model("lstm.nam");
    let good_name = basename(&good);

    // A fails.
    push_gui(shared, &bad);
    test_util::drain_async_model_loading(&mut instance, shared);
    assert!(
        shared.cold.ui_load_error.load(Ordering::Relaxed),
        "A must surface an error"
    );

    // B succeeds and clears the stale error.
    push_gui(shared, &good);
    test_util::drain_async_model_loading(&mut instance, shared);

    assert!(
        !shared.cold.ui_load_error.load(Ordering::Relaxed),
        "a successful load must clear the previous error"
    );
    assert_eq!(
        *shared.cold.ui_model_name.lock().unwrap(),
        good_name,
        "B must be the active model"
    );

    let _ = std::fs::remove_file(&bad);
    let _ = std::fs::remove_file(&good);
}

/// T4.4: dialog_open → loading → idle transitions for a successful load, and
/// dialog_open → loading → error for a failed load.
#[test]
fn test_loading_state_transitions() {
    use crate::clap::test_util::write_invalid_model_fixture;

    let (_entry, _host_info, mut instance, _state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let good = temp_model("lstm.nam");
    shared.cold.ui_dialog_open.store(true, Ordering::Relaxed);
    push_gui(shared, &good);

    // One tick: the dialog closes and the spinner turns on (loading).
    instance.call_on_main_thread_callback();
    assert!(
        !shared.cold.ui_dialog_open.load(Ordering::Relaxed),
        "the dialog must close when the picker result is consumed"
    );
    assert!(
        shared.cold.ui_loading.load(Ordering::Relaxed),
        "ui_loading must be raised while the load is in flight"
    );

    // Completion: idle, no error.
    test_util::drain_async_model_loading(&mut instance, shared);
    assert!(!shared.cold.ui_loading.load(Ordering::Relaxed));
    assert!(!shared.cold.ui_load_error.load(Ordering::Relaxed));

    // Failure path: dialog_open → loading → error.
    let bad = std::env::temp_dir().join(format!(
        "nam_loader_bad2_{}_{}.nam",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_invalid_model_fixture(&bad);
    shared.cold.ui_dialog_open.store(true, Ordering::Relaxed);
    push_gui(shared, &bad);

    instance.call_on_main_thread_callback();
    assert!(shared.cold.ui_loading.load(Ordering::Relaxed));
    test_util::drain_async_model_loading(&mut instance, shared);
    assert!(!shared.cold.ui_loading.load(Ordering::Relaxed));
    assert!(
        shared.cold.ui_load_error.load(Ordering::Relaxed),
        "the failed load must surface an error"
    );

    let _ = std::fs::remove_file(&good);
    let _ = std::fs::remove_file(&bad);
}

/// T4.1: destroying an instance while the loader is mid-`prepare_model` must not
/// hang or corrupt memory (the worker holds an `Arc<GuiSharedState>` and is
/// fence-gated; teardown uses a bounded join).
#[test]
fn test_destroy_during_in_flight_prepare_is_safe() {
    let _hook_guard = test_mid_load_hook_lock();
    let (_entry, _host_info, mut instance, _state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };
    let fence: Arc<AtomicBool> = Arc::clone(&shared.gui.cold.alive_fence);

    let a = temp_model("lstm.nam");
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let a_for_hook = a.clone();
    set_test_mid_load_hook(Some(move |path: &Path| {
        if path == a_for_hook.as_path() {
            let _ = started_tx.send(());
            // Hold the worker inside `prepare_model` until the instance starts
            // destroying itself (fence lowered).
            while fence.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }));

    push_gui(shared, &a);
    instance.call_on_main_thread_callback();
    started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("prepare_model did not start");

    // Destroy while the worker is in flight. `drop` must return promptly and
    // without panicking; the fence aborting the worker keeps the join bounded.
    drop(instance);

    clear_hook();
    let _ = std::fs::remove_file(&a);
}

/// S6-T2: while a structural rebuild flag is pending the host watchdog is
/// re-armed at the fast cadence, and restored to nominal once the flag clears.
#[test]
fn test_watchdog_fast_cadence_while_rebuild_pending() {
    use crate::clap::plugin::main_thread::{WATCHDOG_FAST_PERIOD_MS, WATCHDOG_PERIOD_MS};
    use neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_OS_REBUILD;

    let (_entry, _host_info, mut instance, _state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };
    let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };

    // At rest the nominal cadence is retained (no re-arm).
    mt.sync_watchdog_cadence();
    assert_eq!(mt.watchdog_period_ms.get(), WATCHDOG_PERIOD_MS);

    // A pending rebuild flag accelerates the retry cadence.
    shared.cold.rt_status.set_flag(RT_STATUS_NEEDS_OS_REBUILD);
    mt.sync_watchdog_cadence();
    assert_eq!(
        mt.watchdog_period_ms.get(),
        WATCHDOG_FAST_PERIOD_MS,
        "a pending rebuild flag must accelerate the watchdog cadence"
    );

    // Clearing the flag restores the nominal cadence (no idle-CPU regression).
    shared.cold.rt_status.clear_flag(RT_STATUS_NEEDS_OS_REBUILD);
    mt.sync_watchdog_cadence();
    assert_eq!(mt.watchdog_period_ms.get(), WATCHDOG_PERIOD_MS);
}

fn audio_config() -> PluginAudioConfiguration {
    PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 256,
        max_frames_count: 256,
    }
}

/// S6-T1: the loader worker services `RT_STATUS_NEEDS_SLIMMABLE_REBUILD` off the
/// main thread. With no WaveNet installed the build degenerates to `Skipped`
/// (clear the flag, no slice), which exercises the whole worker→slot→main-tick
/// handshake without needing a heavy fixture.
#[test]
fn test_adaptive_rebuild_worker_services_slimmable_flag() {
    use neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD;

    let (_entry, _host_info, mut instance, _state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };
    let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };
    assert!(
        mt.async_adaptive_rebuild,
        "adaptive rebuild offload must be enabled by default"
    );

    shared
        .cold
        .rt_status
        .requested_slimmable_ch
        .store(8, Ordering::Relaxed);
    shared
        .cold
        .requested_slimmable_generation
        .store(0, Ordering::Relaxed);
    shared
        .cold
        .rt_status
        .set_flag_release(RT_STATUS_NEEDS_SLIMMABLE_REBUILD);

    let deadline = Instant::now() + Duration::from_secs(2);
    while shared
        .cold
        .rt_status
        .check_flag(RT_STATUS_NEEDS_SLIMMABLE_REBUILD)
        && Instant::now() < deadline
    {
        instance.call_on_main_thread_callback();
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(
        !shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_NEEDS_SLIMMABLE_REBUILD),
        "the worker-driven path must clear the slimmable flag"
    );
    assert!(
        mt.model_loader.borrow().is_some(),
        "the loader worker must have been spawned to service the rebuild"
    );
    assert!(
        !shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED),
        "no slice was attempted, so the slice-failed flag must stay clear"
    );
}

/// S6-T1: the loader worker services `RT_STATUS_NEEDS_OS_REBUILD`, and the main
/// thread pushes the built `SetOversample` command and clears the flag.
#[test]
fn test_adaptive_rebuild_worker_services_oversample_flag() {
    use neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_OS_REBUILD;

    let (_entry, _host_info, mut instance, _state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    // X2 factor encoding (`OversampleFactor::X2.to_f32() as u32 == 2`).
    shared
        .cold
        .rt_status
        .requested_os_factor
        .store(2, Ordering::Relaxed);
    shared
        .cold
        .rt_status
        .set_flag_release(RT_STATUS_NEEDS_OS_REBUILD);

    let deadline = Instant::now() + Duration::from_secs(2);
    while shared.cold.rt_status.check_flag(RT_STATUS_NEEDS_OS_REBUILD) && Instant::now() < deadline
    {
        instance.call_on_main_thread_callback();
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(
        !shared.cold.rt_status.check_flag(RT_STATUS_NEEDS_OS_REBUILD),
        "the worker-driven path must clear the oversample-rebuild flag"
    );
}

/// T4.3: the async-restore gate is enabled only for an interactive (non-offline)
/// host that is already active, and only when the opt-in switch is set.
#[test]
fn test_async_restore_gating() {
    let (_entry, _host_info, mut instance, _state) = make_test_plugin_with_harness();
    let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };
    let shared = &mt.shared;

    shared
        .cold
        .render_mode
        .store(RENDER_MODE_REALTIME, Ordering::Relaxed);
    shared.cold.buffer_size.store(0, Ordering::Relaxed);
    mt.async_restore_enabled.set(true);
    assert!(
        !mt.should_use_async_restore(),
        "pre-activate restores must stay synchronous"
    );

    shared.cold.buffer_size.store(128, Ordering::Relaxed);
    assert!(
        mt.should_use_async_restore(),
        "an active interactive host with the opt-in set must use the worker"
    );

    shared
        .cold
        .render_mode
        .store(RENDER_MODE_OFFLINE, Ordering::Relaxed);
    assert!(
        !mt.should_use_async_restore(),
        "offline/render restores must stay synchronous"
    );

    shared
        .cold
        .render_mode
        .store(RENDER_MODE_REALTIME, Ordering::Relaxed);
    mt.async_restore_enabled.set(false);
    assert!(
        !mt.should_use_async_restore(),
        "the asynchronous restore is opt-in"
    );
}

/// T4.3 gate: with several live instances each holding a heavy pending load, a
/// single `housekeeping()` tick stays far below the 2 ms budget, proving the
/// prepare phase never runs on the main thread.
#[test]
fn test_multi_instance_housekeeping_tick_stays_fast() {
    const N: usize = 4;

    // Keep every instance (and its host/entry) alive simultaneously, mirroring
    // a project with N instances, and enqueue a heavy (a2/WaveNet) load on each.
    let mut live = Vec::new();
    for _ in 0..N {
        let (entry, host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };
        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate failed");
        let started = stopped.start_processing().expect("start_processing failed");
        push_gui(shared, &test_util::model_path("a2_example.nam"));
        live.push((entry, host_info, instance, started));
    }

    for (_entry, _host_info, instance, _started) in live.iter_mut() {
        let start = std::time::Instant::now();
        instance.call_on_main_thread_callback();
        let ms = start.elapsed().as_secs_f64() * 1e3;
        assert!(
            ms < 2.0,
            "housekeeping() blocked for {ms:.3} ms with a pending heavy load; \
             the prepare phase must run on the nam-model-loader worker"
        );
    }

    // Drain every instance so the loaders finish before teardown.
    for (_entry, _host_info, instance, _started) in live.iter_mut() {
        let shared = unsafe { &*extract_plugin_shared(instance) };
        test_util::drain_async_model_loading(instance, shared);
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::host_harness::{HostEvent, extract_plugin_shared, make_test_plugin_with_harness};
use crate::clap::plugin::shared::{
    MAX_PENDING_MODEL_REQUESTS, ModelRequestOrigin, PendingModelRequest,
};
use crate::clap::test_util;
use clack_extensions::preset_discovery::prelude::*;
use std::ffi::CString;
use std::sync::atomic::Ordering;

#[test]
fn test_preset_load_integration() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    let preset_load_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginPresetLoad>()
        .expect("PluginPresetLoad extension not found");

    let model_path = crate::clap::test_util::model_path("lstm.nam");
    let path_str = model_path.to_str().expect("Invalid model path");
    let path_cstr = CString::new(path_str).expect("Invalid CString");

    let counter_before = shared.cold.model_load_counter.load(Ordering::Relaxed);
    assert_eq!(counter_before, 0, "model_load_counter should start at 0");

    let handle = plugin_instance.plugin_handle();
    preset_load_ext
        .load_from_location(&handle, Location::File { path: &path_cstr }, None)
        .expect("load_from_location should succeed");

    plugin_instance.call_on_main_thread_callback();

    let counter_after = shared.cold.model_load_counter.load(Ordering::Relaxed);
    assert!(
        counter_after > counter_before,
        "model_load_counter should increment after preset load (was {}, now {})",
        counter_before,
        counter_after
    );

    let model_name = shared.cold.ui_model_name.lock().unwrap();
    assert!(
        !model_name.is_empty(),
        "ui_model_name should be set after preset load"
    );
}

#[test]
fn test_consecutive_preset_loads_fifo_and_notification() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let preset_load_ext = instance
        .plugin_handle()
        .get_extension::<PluginPresetLoad>()
        .expect("PluginPresetLoad extension not found");

    let m1 = test_util::model_path("lstm.nam");
    let m2 = test_util::model_path("a2_example.nam");
    let m3 = test_util::model_path("wavenet_a1_standard.nam");

    let c1 = CString::new(m1.to_str().unwrap()).unwrap();
    let c2 = CString::new(m2.to_str().unwrap()).unwrap();
    let c3 = CString::new(m3.to_str().unwrap()).unwrap();

    let k1 = CString::new("key-lstm").unwrap();
    let k2 = CString::new("key-a2").unwrap();

    let handle = instance.plugin_handle();
    preset_load_ext
        .load_from_location(&handle, Location::File { path: &c1 }, Some(&k1))
        .expect("load 1 failed");
    preset_load_ext
        .load_from_location(&handle, Location::File { path: &c2 }, Some(&k2))
        .expect("load 2 failed");
    preset_load_ext
        .load_from_location(&handle, Location::File { path: &c3 }, None)
        .expect("load 3 failed");

    assert_eq!(
        shared.cold.pending_model_requests.lock().unwrap().len(),
        3,
        "All 3 requests should be enqueued"
    );
    assert!(
        shared.cold.ui_loading.load(Ordering::Relaxed),
        "ui_loading should be true"
    );

    // Drain #1
    instance.call_on_main_thread_callback();
    assert_eq!(
        shared.cold.pending_model_requests.lock().unwrap().len(),
        2,
        "1 item should be drained, 2 remaining"
    );
    assert!(
        state.callback_requested.load(Ordering::SeqCst),
        "Host callback should be requested while requests remain"
    );
    state.callback_requested.store(false, Ordering::SeqCst);

    let events = state.snapshot();
    let preset_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, HostEvent::PresetLoaded { .. }))
        .collect();
    assert_eq!(preset_events.len(), 1);
    assert_eq!(
        preset_events[0],
        &HostEvent::PresetLoaded {
            location: c1.to_str().unwrap().to_string(),
            load_key: Some("key-lstm".to_string()),
        }
    );

    // Drain #2
    instance.call_on_main_thread_callback();
    assert_eq!(shared.cold.pending_model_requests.lock().unwrap().len(), 1);
    assert!(state.callback_requested.load(Ordering::SeqCst));
    state.callback_requested.store(false, Ordering::SeqCst);

    let events = state.snapshot();
    let preset_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, HostEvent::PresetLoaded { .. }))
        .collect();
    assert_eq!(preset_events.len(), 2);
    assert_eq!(
        preset_events[1],
        &HostEvent::PresetLoaded {
            location: c2.to_str().unwrap().to_string(),
            load_key: Some("key-a2".to_string()),
        }
    );

    // Drain #3
    instance.call_on_main_thread_callback();
    assert_eq!(
        shared.cold.pending_model_requests.lock().unwrap().len(),
        0,
        "Queue should be empty after 3 drains"
    );
    assert!(
        !shared.cold.ui_loading.load(Ordering::Relaxed),
        "ui_loading should be false when queue is empty"
    );

    let events = state.snapshot();
    let preset_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, HostEvent::PresetLoaded { .. }))
        .collect();
    assert_eq!(preset_events.len(), 3);
    assert_eq!(
        preset_events[2],
        &HostEvent::PresetLoaded {
            location: c3.to_str().unwrap().to_string(),
            load_key: None,
        }
    );
}

#[test]
fn test_interleaved_gui_and_preset_loads() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let preset_load_ext = instance
        .plugin_handle()
        .get_extension::<PluginPresetLoad>()
        .expect("PluginPresetLoad extension not found");

    let m_gui = test_util::model_path("lstm.nam");
    let m_preset = test_util::model_path("a2_example.nam");
    let c_preset = CString::new(m_preset.to_str().unwrap()).unwrap();
    let k_preset = CString::new("preset-interleave-key").unwrap();

    // 1. Enqueue GUI load directly into queue
    shared
        .cold
        .pending_model_requests
        .lock()
        .unwrap()
        .push_back(PendingModelRequest {
            path: m_gui,
            origin: ModelRequestOrigin::Gui,
        });
    shared.cold.ui_loading.store(true, Ordering::Relaxed);

    // 2. Enqueue Preset load via extension
    let handle = instance.plugin_handle();
    preset_load_ext
        .load_from_location(&handle, Location::File { path: &c_preset }, Some(&k_preset))
        .expect("Preset load enqueue failed");

    assert_eq!(shared.cold.pending_model_requests.lock().unwrap().len(), 2);

    // Drain GUI load
    instance.call_on_main_thread_callback();
    assert_eq!(shared.cold.pending_model_requests.lock().unwrap().len(), 1);
    assert_eq!(
        *shared.cold.ui_model_name.lock().unwrap(),
        "lstm.nam",
        "GUI load should have set ui_model_name"
    );

    // Verify host received ZERO preset notifications for the GUI load
    let events = state.snapshot();
    let preset_events: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                HostEvent::PresetLoaded { .. } | HostEvent::PresetLoadError { .. }
            )
        })
        .collect();
    assert_eq!(
        preset_events.len(),
        0,
        "GUI load must not notify HostPresetLoad"
    );

    // Drain Preset load
    instance.call_on_main_thread_callback();
    assert_eq!(shared.cold.pending_model_requests.lock().unwrap().len(), 0);
    assert_eq!(
        *shared.cold.ui_model_name.lock().unwrap(),
        "a2_example.nam",
        "Preset load should have set ui_model_name"
    );

    // Verify host received the preset notification for the preset load
    let events = state.snapshot();
    let preset_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, HostEvent::PresetLoaded { .. }))
        .collect();
    assert_eq!(
        preset_events.len(),
        1,
        "HostPresetLoad must receive exactly one notification for preset load"
    );
    assert_eq!(
        preset_events[0],
        &HostEvent::PresetLoaded {
            location: c_preset.to_str().unwrap().to_string(),
            load_key: Some("preset-interleave-key".to_string()),
        }
    );
}

#[test]
fn test_preset_load_queue_capacity_bound_16() {
    let (_entry, _host_info, mut instance, _state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let preset_load_ext = instance
        .plugin_handle()
        .get_extension::<PluginPresetLoad>()
        .expect("PluginPresetLoad extension not found");

    let m = test_util::model_path("lstm.nam");
    let c = CString::new(m.to_str().unwrap()).unwrap();

    let handle = instance.plugin_handle();
    for i in 0..MAX_PENDING_MODEL_REQUESTS {
        preset_load_ext
            .load_from_location(&handle, Location::File { path: &c }, None)
            .unwrap_or_else(|_| panic!("Load {i} should succeed within capacity"));
    }

    assert_eq!(
        shared.cold.pending_model_requests.lock().unwrap().len(),
        MAX_PENDING_MODEL_REQUESTS
    );

    // 17th request must be rejected
    let res = preset_load_ext.load_from_location(&handle, Location::File { path: &c }, None);
    assert!(res.is_err(), "17th request must be rejected");

    assert_eq!(
        shared.cold.pending_model_requests.lock().unwrap().len(),
        MAX_PENDING_MODEL_REQUESTS,
        "Queue length must remain bounded at MAX_PENDING_MODEL_REQUESTS"
    );
}

#[test]
fn test_pre_enqueuing_validation() {
    let (_entry, _host_info, mut instance, _state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let preset_load_ext = instance
        .plugin_handle()
        .get_extension::<PluginPresetLoad>()
        .expect("PluginPresetLoad extension not found");

    let handle = instance.plugin_handle();

    // 1. Non-existent file path
    let non_existent = CString::new("/non/existent/model/path/foo.nam").unwrap();
    let res1 = preset_load_ext.load_from_location(
        &handle,
        Location::File {
            path: &non_existent,
        },
        None,
    );
    assert!(res1.is_err(), "Non-existent path must be rejected");
    assert_eq!(shared.cold.pending_model_requests.lock().unwrap().len(), 0);
    assert!(!shared.cold.ui_loading.load(Ordering::Relaxed));

    // 2. Directory path (not a regular file)
    let dir_cstr = CString::new("/tmp").unwrap();
    let res2 =
        preset_load_ext.load_from_location(&handle, Location::File { path: &dir_cstr }, None);
    assert!(res2.is_err(), "Directory path must be rejected");
    assert_eq!(shared.cold.pending_model_requests.lock().unwrap().len(), 0);
    assert!(!shared.cold.ui_loading.load(Ordering::Relaxed));

    // 3. Plugin container
    let res3 = preset_load_ext.load_from_location(&handle, Location::Plugin, None);
    assert!(res3.is_err(), "Plugin container location must be rejected");
    assert_eq!(shared.cold.pending_model_requests.lock().unwrap().len(), 0);
    assert!(!shared.cold.ui_loading.load(Ordering::Relaxed));
}

#[test]
fn test_toctou_rejection_on_file_modification_during_load() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let preset_load_ext = instance
        .plugin_handle()
        .get_extension::<PluginPresetLoad>()
        .expect("PluginPresetLoad extension not found");

    // Copy lstm.nam to a temporary file
    let src_model = test_util::model_path("lstm.nam");
    let temp_dir = std::env::temp_dir();
    let temp_file = temp_dir.join(format!(
        "toctou_test_{}_{}.nam",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::copy(&src_model, &temp_file).expect("copy test model");

    // Hook: append extra bytes to the file right after load_and_build_model, before hash calculation
    let target_path = temp_file.clone();
    crate::clap::plugin::main_thread::load::set_test_mid_load_hook(Some(
        move |path: &std::path::Path| {
            if path == target_path {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .expect("open temp file in hook");
                f.write_all(b"\n// modified during load")
                    .expect("write in hook");
                f.sync_all().expect("sync hook");
            }
        },
    ));

    let temp_c = CString::new(temp_file.to_str().unwrap()).unwrap();
    let k = CString::new("toctou-key").unwrap();

    let handle = instance.plugin_handle();
    preset_load_ext
        .load_from_location(&handle, Location::File { path: &temp_c }, Some(&k))
        .expect("load_from_location should enqueue successfully");

    // Run housekeeping
    instance.call_on_main_thread_callback();

    // Reset hook and clean up temp file
    crate::clap::plugin::main_thread::load::set_test_mid_load_hook::<fn(&std::path::Path)>(None);
    let _ = std::fs::remove_file(&temp_file);

    // Verify error state
    assert!(
        shared.cold.ui_load_error.load(Ordering::Relaxed),
        "ui_load_error should be flagged on TOCTOU failure"
    );
    let err_msg = shared.cold.ui_load_error_msg.lock().unwrap();
    assert!(
        err_msg.contains("arquivo modificado durante a carga"),
        "ui_load_error_msg should indicate file modified during load, was: {err_msg}"
    );

    // Verify host received PresetLoadError
    let events = state.snapshot();
    let error_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, HostEvent::PresetLoadError { .. }))
        .collect();
    assert_eq!(
        error_events.len(),
        1,
        "Host should have received 1 PresetLoadError event"
    );
    match error_events[0] {
        HostEvent::PresetLoadError {
            location,
            load_key,
            message,
            ..
        } => {
            assert_eq!(location, temp_c.to_str().unwrap());
            assert_eq!(load_key, &Some("toctou-key".to_string()));
            assert!(
                message.contains("arquivo modificado durante a carga"),
                "Host error message was: {message}"
            );
        }
        _ => unreachable!(),
    }
}

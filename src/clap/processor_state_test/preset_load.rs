// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

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

    let mut handle = plugin_instance.plugin_handle();
    preset_load_ext
        .load_from_location(&mut handle, Location::File { path: &path_cstr }, None)
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

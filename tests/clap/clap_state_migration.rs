// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use clack_extensions::state::PluginState;
use clack_host::prelude::*;
use nam_plug::clap::plugin::NamClapPlugin;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

struct TestHostShared {
    restart_was_called: Arc<AtomicBool>,
}
impl<'a> SharedHandler<'a> for TestHostShared {
    fn request_restart(&self) {
        self.restart_was_called
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

struct TestHost;
impl HostHandlers for TestHost {
    type Shared<'a> = TestHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

/// Creates a plugin instance by loading the freshly built `.so`.
/// Uses `artifact_validator` to resolve the artifact and record its SHA256.
fn create_plugin_instance() -> PluginInstance<TestHost> {
    let artifact = super::artifact_validator::TestedArtifact::resolve_and_hash();

    // SAFETY: Dynamic plugin loading from the build artifact.
    let entry = unsafe {
        PluginEntry::load(&artifact.path).expect("Failed to load plugin entry from build artifact")
    };

    let host_info = HostInfo::new(
        "NAM-rs-Test",
        "NAM",
        "https://github.com/fabiohl/nam-rs",
        "0.1.0",
    )
    .expect("Failed to create HostInfo");

    PluginInstance::<TestHost>::new(
        |_| TestHostShared {
            restart_was_called: Arc::new(AtomicBool::new(false)),
        },
        |_| (),
        &entry,
        c"br.eti.fabiolima.nam-plug",
        &host_info,
    )
    .expect("Failed to instantiate plugin")
}

#[test]
fn test_integration_v0_legacy_load() {
    let mut instance = create_plugin_instance();
    let state_ext = instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("PluginState extension not found");

    // Payload v0 without envelope (raw JSON of ProcessingParams)
    let v0_json = r#"{"input_gain_db": 3.0,"output_gain_db": -6.0,"gate_threshold_db": -50.0,"model_path": null,"bypass": true}"#;

    {
        let mut handle = instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut v0_json.as_bytes())
            .expect("Failed to load v0 state");
    }

    // Verify that internal state on the main thread was properly updated and migrated
    let raw_plugin_ptr = instance.plugin_handle().as_raw_ptr();
    let shared_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<NamClapPlugin>::handle(
            raw_plugin_ptr,
            |wrapper| Ok(wrapper.shared() as *const nam_plug::clap::plugin::NamClapShared),
        )
        .expect("Failed to obtain plugin wrapper")
    };
    let shared = unsafe { &*shared_ptr };

    assert_eq!(
        f32::from_bits(
            shared
                .ui_to_rt
                .param_input_gain
                .load(std::sync::atomic::Ordering::Relaxed)
        ),
        3.0
    );
    assert_eq!(
        f32::from_bits(
            shared
                .ui_to_rt
                .param_output_gain
                .load(std::sync::atomic::Ordering::Relaxed)
        ),
        -6.0
    );
    assert_eq!(
        f32::from_bits(
            shared
                .ui_to_rt
                .param_gate_thresh
                .load(std::sync::atomic::Ordering::Relaxed)
        ),
        -50.0
    );
    assert_eq!(
        shared
            .ui_to_rt
            .param_bypass
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[test]
fn test_integration_v1_round_trip() {
    let mut instance = create_plugin_instance();
    let state_ext = instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("PluginState extension not found");

    // 1. Modify atomic values on the plugin simulating user action
    let raw_plugin_ptr = instance.plugin_handle().as_raw_ptr();
    let shared_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<NamClapPlugin>::handle(
            raw_plugin_ptr,
            |wrapper| Ok(wrapper.shared() as *const nam_plug::clap::plugin::NamClapShared),
        )
        .expect("Failed to obtain plugin wrapper")
    };
    let shared = unsafe { &*shared_ptr };

    shared
        .ui_to_rt
        .param_input_gain
        .store(1.5f32.to_bits(), std::sync::atomic::Ordering::Relaxed);
    shared
        .ui_to_rt
        .param_output_gain
        .store((-2.0f32).to_bits(), std::sync::atomic::Ordering::Relaxed);
    shared
        .ui_to_rt
        .param_gate_thresh
        .store((-65.0f32).to_bits(), std::sync::atomic::Ordering::Relaxed);
    shared
        .ui_to_rt
        .param_bypass
        .store(0, std::sync::atomic::Ordering::Relaxed);

    // 2. Save state
    let mut output_buf = Vec::new();
    {
        let mut handle = instance.plugin_handle();
        state_ext
            .save(&mut handle, &mut output_buf)
            .expect("Failed to save state");
    }

    // 3. Reset atomic values in the plugin
    shared
        .ui_to_rt
        .param_input_gain
        .store(0.0f32.to_bits(), std::sync::atomic::Ordering::Relaxed);
    shared
        .ui_to_rt
        .param_output_gain
        .store(0.0f32.to_bits(), std::sync::atomic::Ordering::Relaxed);
    shared
        .ui_to_rt
        .param_gate_thresh
        .store(0.0f32.to_bits(), std::sync::atomic::Ordering::Relaxed);
    shared
        .ui_to_rt
        .param_bypass
        .store(1, std::sync::atomic::Ordering::Relaxed);

    // 4. Load saved state
    {
        let mut handle = instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut output_buf.as_slice())
            .expect("Failed to load saved state");
    }

    // 5. Verify that values were properly restored
    assert_eq!(
        f32::from_bits(
            shared
                .ui_to_rt
                .param_input_gain
                .load(std::sync::atomic::Ordering::Relaxed)
        ),
        1.5
    );
    assert_eq!(
        f32::from_bits(
            shared
                .ui_to_rt
                .param_output_gain
                .load(std::sync::atomic::Ordering::Relaxed)
        ),
        -2.0
    );
    assert_eq!(
        f32::from_bits(
            shared
                .ui_to_rt
                .param_gate_thresh
                .load(std::sync::atomic::Ordering::Relaxed)
        ),
        -65.0
    );
    assert_eq!(
        shared
            .ui_to_rt
            .param_bypass
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[test]
fn test_integration_forward_v1_to_v2() {
    let mut instance = create_plugin_instance();
    let state_ext = instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("PluginState extension not found");

    // Payload with future version (version: 2) and hypothetical additional parameters
    let v2_json = r#"{
        "version": 2,
        "params": {
            "input_gain_db": 4.5,
            "output_gain_db": -5.0,
            "gate_threshold_db": -60.0,
            "model_path": null,
            "model_basename": null,
            "model_search_paths": [],
            "bypass": false,
            "new_future_gain": 12.0
        }
    }"#;

    // Should load successfully applying the migration (or ignoring unknown fields) without panics
    {
        let mut handle = instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut v2_json.as_bytes())
            .expect("Failed to process future v2 envelope");
    }

    let raw_plugin_ptr = instance.plugin_handle().as_raw_ptr();
    let shared_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<NamClapPlugin>::handle(
            raw_plugin_ptr,
            |wrapper| Ok(wrapper.shared() as *const nam_plug::clap::plugin::NamClapShared),
        )
        .expect("Failed to obtain plugin wrapper")
    };
    let shared = unsafe { &*shared_ptr };

    assert_eq!(
        f32::from_bits(
            shared
                .ui_to_rt
                .param_input_gain
                .load(std::sync::atomic::Ordering::Relaxed)
        ),
        4.5
    );
    assert_eq!(
        f32::from_bits(
            shared
                .ui_to_rt
                .param_output_gain
                .load(std::sync::atomic::Ordering::Relaxed)
        ),
        -5.0
    );
    assert_eq!(
        f32::from_bits(
            shared
                .ui_to_rt
                .param_gate_thresh
                .load(std::sync::atomic::Ordering::Relaxed)
        ),
        -60.0
    );
    assert_eq!(
        shared
            .ui_to_rt
            .param_bypass
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! T1.1 (F-RB-01): `flush_pending_model` must not silently discard a deferred
//! model on resource-build failure. A deferred load flushed with an invalid
//! host rate (forcing `NamResampler::new_simple` to fail) must return `Err`,
//! retain the `PendingModel` for retry, set `RT_STATUS_MODEL_LOAD_FAILED` and
//! surface `ui_load_error` + `ui_load_error_msg`.
//!
//! RED test: fails on current code (model lost via `take()`-before-build,
//! no flag, no visible error).

use crate::clap::test_util;
use clack_host::process::PluginAudioConfiguration;
use neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED;
use std::sync::atomic::Ordering;

#[test]
fn test_flush_pending_failure_retains_model_and_signals_error() {
    let model_path = crate::clap::test_util::model_path("lstm.nam");
    assert!(model_path.exists(), "lstm.nam fixture missing");

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    let raw_ptr = plugin_instance.plugin_handle().as_raw_ptr();
    let main_thread_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |w| Ok(w.main_thread() as *const crate::clap::plugin::NamClapMainThread<'static>),
        )
        .unwrap()
    };
    let mt = unsafe { &*main_thread_ptr };

    // Defer a model: pre-activation (buffer_size == 0) defers to pending_model.
    mt.load_model(&model_path)
        .expect("pre-activation load_model should defer, not fail");
    assert!(
        mt.shared.cold.pending_model.lock().unwrap().is_some(),
        "model must be deferred in pending_model before activation"
    );

    // Force `NamResampler::new_simple` to fail at flush time: host rate 1000 Hz
    // is outside the supported 4_000..=384_000 Hz range. buffer_size > 0 so the
    // flush proceeds past the still-deferred early return.
    mt.shared.cold.sample_rate.store(1000, Ordering::Relaxed);
    mt.shared.cold.buffer_size.store(512, Ordering::Relaxed);

    let res = mt.flush_pending_model();
    assert!(res.is_err(), "flush with invalid host rate must return Err");

    // F-RB-01 contract: the model is retained for retry, the failure is
    // flagged for the audio thread/GUI, and the error is visible.
    assert!(
        mt.shared.cold.pending_model.lock().unwrap().is_some(),
        "F-RB-01: failed flush must retain PendingModel for retry (model silently lost)"
    );
    assert!(
        mt.shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_MODEL_LOAD_FAILED),
        "F-RB-01: RT_STATUS_MODEL_LOAD_FAILED must be set on resampler build failure"
    );
    assert!(
        mt.shared.cold.ui_load_error.load(Ordering::Relaxed),
        "F-RB-01: ui_load_error must be set on resampler build failure"
    );
    assert!(
        !mt.shared.cold.ui_load_error_msg.lock().unwrap().is_empty(),
        "F-RB-01: ui_load_error_msg must carry an actionable message"
    );

    // The harness instance was never activated; drop without audio config.
    let _ = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };
}

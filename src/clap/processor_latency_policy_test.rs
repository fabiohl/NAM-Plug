// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Strict host restart policy with same-latency optimization
//! for model and IR swaps.
//!
//! Validates the decision matrix on the main thread:
//! - A model swap that keeps the *exact* same latency (same model rate) is a
//!   continuous hot swap through the SPSC — no `request_restart()`.
//! - A model swap that changes the latency (different model rate) is staged
//!   and a host restart is requested; the DSP keeps the old, still-reported
//!   latency until `activate()` installs the staged resources.
//! - The first IR load (0 → partition) and the IR clear (partition → 0) are
//!   staged + restart; an IR-for-IR swap (same partition) is continuous.
//! - Coalescing is latest-wins: a same-latency load supersedes a staged
//!   latency-changing load.
//! - The host is notified (RestartRequested) before the physical latency
//!   changes, and `LatencyChanged` fires only after the restart applies it.

use crate::clap::host_harness::{
    extract_plugin_main_thread, extract_plugin_shared, make_harness_audio_processor,
    make_test_plugin_with_harness, perform_restart, process_block_harness,
};
use crate::clap::test_util::{tmp_path, write_model_with_rate};
use clack_host::prelude::*;
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

fn audio_config() -> PluginAudioConfiguration {
    PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 256,
        max_frames_count: 256,
    }
}

/// Writes a synthetic FIR IR WAV (`[1.0, -0.9, 0, ...]`) of `len` samples.
fn write_ir(len: usize, name: &str) -> PathBuf {
    let path = tmp_path(&format!("{name}.wav"));
    let mut ir = vec![0.0f32; len];
    ir[0] = 1.0;
    ir[1] = -0.9;
    neural_amp_modeler_rs::testing::wav::write_wav_f32(&path, &ir, 48000)
        .expect("write synthetic IR WAV");
    path
}

/// Stream latency of a 44.1 kHz model on a 48 kHz host with 256-sample blocks
/// (the expected `current_stream_latency` after the restart installs it).
fn expected_44k_stream_latency() -> u32 {
    crate::clap::plugin::build_stream_adapter(48000, 44100, 256)
        .expect("build stream adapter")
        .latency_samples()
}

/// A model swap that keeps the exact same latency must be a continuous hot
/// swap: no restart requested, nothing staged, and the model lands on the
/// audio thread (observable through the persisted generation at deactivate).
#[test]
fn test_model_swap_same_rate_is_continuous_no_restart() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let mut started = stopped.start_processing().expect("start_processing");

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };

    let model = crate::clap::test_util::model_path("lstm.nam");
    assert!(model.exists(), "lstm.nam fixture missing");
    mt.load_model(&model).expect("load same-rate model");

    assert!(
        !state.restart_requested.load(Ordering::SeqCst),
        "same-latency model swap must not request a host restart"
    );
    assert!(
        mt.staged_swap.is_none(),
        "same-latency model swap must not be staged"
    );
    assert_eq!(
        shared.cold.current_stream_latency.load(Ordering::Relaxed),
        0,
        "stream latency contribution must be unchanged (48k host == 48k model)"
    );

    // The swap must actually have reached the DSP: process a block (draining
    // the SPSC so `cold_load_model()` runs) and confirm the installed model
    // generation at deactivate().
    let mut il = vec![0.3f32; 256];
    let mut ir = vec![0.3f32; 256];
    let mut ol = vec![0.0f32; 256];
    let mut or = vec![0.0f32; 256];
    let _ = process_block_harness(&mut started, &mut il, &mut ir, &mut ol, &mut or, None);
    let stopped = started.stop_processing();
    instance.deactivate(stopped);
    let guard = shared.cold.deactivated_dsp.lock().unwrap();
    let deact = guard.as_ref().expect("deactivated state missing");
    assert_eq!(
        deact.model_generation, 1,
        "continuously-swapped model must be installed on the audio thread"
    );
}

/// A model swap that changes the latency (44.1k over a 48k host) must be
/// staged and request a host restart; the DSP keeps the old latency.
#[test]
fn test_model_swap_diff_rate_triggers_restart_and_stages() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let mut started = stopped.start_processing().expect("start_processing");

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };

    let base = crate::clap::test_util::model_path("lstm.nam");
    let model_44k = write_model_with_rate(&base, 44100);
    mt.load_model(&model_44k).expect("load 44.1k model");

    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "latency-changing model swap must request a host restart"
    );
    assert!(
        mt.staged_swap
            .as_ref()
            .and_then(|s| s.model.as_ref())
            .is_some(),
        "latency-changing model swap must be staged"
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        0,
        "reported latency must NOT change while the restart is pending"
    );
    assert_eq!(
        shared.cold.current_stream_latency.load(Ordering::Relaxed),
        0,
        "the installed stream must be untouched while the restart is pending"
    );

    // Processing audio must not install the staged model either.
    let mut il = vec![0.3f32; 256];
    let mut ir = vec![0.3f32; 256];
    let mut ol = vec![0.0f32; 256];
    let mut or = vec![0.0f32; 256];
    let _ = process_block_harness(&mut started, &mut il, &mut ir, &mut ol, &mut or, None);
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        0,
        "processing must not apply the staged swap or change the latency"
    );

    let _ = std::fs::remove_file(&model_44k);
}

/// After the host restart cycle, `activate()` installs the staged model and
/// publishes the new latency contribution atomically.
#[test]
fn test_restart_cycle_applies_staged_model_and_latency() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };

    let base = crate::clap::test_util::model_path("lstm.nam");
    let model_44k = write_model_with_rate(&base, 44100);
    mt.load_model(&model_44k).expect("load 44.1k model");
    assert!(state.restart_requested.load(Ordering::SeqCst));

    let expected_latency = expected_44k_stream_latency();
    assert!(
        expected_latency > 0,
        "44.1k over 48k must add stream latency"
    );

    // Host restart cycle: deactivate → activate consumes the staged swap.
    let mut started_after = perform_restart(&mut instance, started, &state, audio_config());

    assert_eq!(
        shared.cold.current_stream_latency.load(Ordering::Relaxed),
        expected_latency,
        "activate() must install the staged stream and publish its latency"
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        expected_latency,
        "reported latency must match the active filter chain after the restart"
    );
    assert!(
        mt.staged_swap.is_none(),
        "staged swap must be consumed by activate()"
    );

    // The staged model must be the one running: process blocks, deactivate and
    // confirm the installed generation advanced (it was never installed before).
    let mut il = vec![0.3f32; 256];
    let mut ir = vec![0.3f32; 256];
    let mut ol = vec![0.0f32; 256];
    let mut or = vec![0.0f32; 256];
    let _ = process_block_harness(&mut started_after, &mut il, &mut ir, &mut ol, &mut or, None);
    let stopped = started_after.stop_processing();
    instance.deactivate(stopped);
    let guard = shared.cold.deactivated_dsp.lock().unwrap();
    let deact = guard.as_ref().expect("deactivated state missing");
    assert_eq!(
        deact.model_generation, 1,
        "restart must install the staged 44.1k model on the audio thread"
    );

    let _ = std::fs::remove_file(&model_44k);
}

/// First IR load (0 → partition) must be staged + restart; an IR-for-IR swap
/// (same partition) must be continuous with no restart.
#[test]
fn test_ir_load_and_swap_policy() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };

    let ir1 = write_ir(512, "ir1");
    let ir2 = write_ir(1024, "ir2");

    // ── First IR load: 0 → partition, latency changes ⇒ staged + restart ──
    mt.load_cabsim(&ir1).expect("load first IR");
    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "first IR load must request a host restart (0 → partition latency)"
    );
    assert!(
        mt.staged_swap
            .as_ref()
            .and_then(|s| s.ir.as_ref())
            .is_some(),
        "first IR load must be staged"
    );
    assert_eq!(
        shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
        0,
        "no IR must be installed while the restart is pending"
    );

    // ── Host restart installs the staged IR ──
    let mut started = perform_restart(&mut instance, started, &state, audio_config());
    assert_eq!(
        shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
        256,
        "restart must install the staged IR adapter (partition = 256)"
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        256,
        "reported latency must include the cabsim partition after the restart"
    );

    // ── IR-for-IR swap: same partition ⇒ continuous, no restart ──
    state.restart_requested.store(false, Ordering::SeqCst);
    mt.load_cabsim(&ir2).expect("swap IR");
    assert!(
        !state.restart_requested.load(Ordering::SeqCst),
        "same-partition IR swap must not request a host restart"
    );
    assert!(
        mt.staged_swap.is_none(),
        "same-partition IR swap must not be staged"
    );
    assert_eq!(
        shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
        256,
        "cabsim latency contribution is unchanged by an IR-for-IR swap"
    );

    // Prove the continuous swap landed: process blocks and observe the tail
    // length grow from the 512-sample IR (2 partitions) to the 1024-sample IR
    // (4 partitions).
    for _ in 0..2 {
        let mut il = vec![0.3f32; 256];
        let mut ir = vec![0.3f32; 256];
        let mut ol = vec![0.0f32; 256];
        let mut or = vec![0.0f32; 256];
        let _ = process_block_harness(&mut started, &mut il, &mut ir, &mut ol, &mut or, None);
    }
    assert_eq!(
        shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed),
        4 * 256,
        "the 1024-sample IR must be installed (tail = partitions × partition)"
    );

    let _ = std::fs::remove_file(&ir1);
    let _ = std::fs::remove_file(&ir2);
}

/// Clearing the active IR (partition → 0) must be staged + restart.
#[test]
fn test_ir_clear_triggers_restart() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };

    let ir = write_ir(512, "clear");
    mt.load_cabsim(&ir).expect("load IR");
    assert!(state.restart_requested.load(Ordering::SeqCst));

    let started = perform_restart(&mut instance, started, &state, audio_config());
    assert_eq!(
        shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
        256,
        "IR must be installed after the restart"
    );

    // ── Clear the IR via the UI flag + housekeeping ──
    state.restart_requested.store(false, Ordering::SeqCst);
    shared.cold.ui_clear_ir.store(true, Ordering::Relaxed);
    instance.call_on_main_thread_callback();

    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "IR clear must request a host restart (partition → 0 latency)"
    );
    assert!(
        mt.staged_swap
            .as_ref()
            .and_then(|s| s.ir.as_ref())
            .is_some_and(|ir| ir.is_none()),
        "IR clear must be staged as a clear (None)"
    );
    assert_eq!(
        shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
        256,
        "the DSP must keep the IR while the restart is pending"
    );

    // ── Host restart applies the staged clear ──
    let mut started = perform_restart(&mut instance, started, &state, audio_config());
    assert_eq!(
        shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
        0,
        "restart must apply the staged IR clear"
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        0,
        "reported latency must drop to the non-IR baseline after the clear"
    );

    let mut il = vec![0.3f32; 256];
    let mut ir_buf = vec![0.3f32; 256];
    let mut ol = vec![0.0f32; 256];
    let mut or = vec![0.0f32; 256];
    let _ = process_block_harness(&mut started, &mut il, &mut ir_buf, &mut ol, &mut or, None);
    let _ = std::fs::remove_file(&ir);
}

/// Coalescing (latest-wins): a same-latency model load supersedes a staged
/// latency-changing load — the restart applies nothing new.
#[test]
fn test_same_latency_swap_supersedes_staged() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };

    let base = crate::clap::test_util::model_path("lstm.nam");
    let model_44k = write_model_with_rate(&base, 44100);
    let model_48k = crate::clap::test_util::model_path("wavenet_a1_standard.nam");

    // 44.1k staged + restart…
    mt.load_model(&model_44k).expect("load 44.1k model");
    assert!(state.restart_requested.load(Ordering::SeqCst));
    assert!(
        mt.staged_swap
            .as_ref()
            .and_then(|s| s.model.as_ref())
            .is_some()
    );

    // …then a 48k load (same latency as the installed chain) supersedes it and
    // applies continuously.
    mt.load_model(&model_48k).expect("load 48k model");
    assert!(
        mt.staged_swap.is_none(),
        "a same-latency load must supersede the staged latency-changing load"
    );
    assert_eq!(
        shared.cold.current_stream_latency.load(Ordering::Relaxed),
        0,
        "48k over 48k keeps zero stream latency"
    );

    // The restart cycle lands with the 48k model (generation 2 — both loads
    // allocated generations), never the staged 44.1k.
    let mut started_after = perform_restart(&mut instance, started, &state, audio_config());
    assert_eq!(
        shared.cold.current_stream_latency.load(Ordering::Relaxed),
        0,
        "restart must not apply the superseded 44.1k staged stream"
    );
    let mut il = vec![0.3f32; 256];
    let mut ir = vec![0.3f32; 256];
    let mut ol = vec![0.0f32; 256];
    let mut or = vec![0.0f32; 256];
    let _ = process_block_harness(&mut started_after, &mut il, &mut ir, &mut ol, &mut or, None);
    let stopped = started_after.stop_processing();
    instance.deactivate(stopped);
    let guard = shared.cold.deactivated_dsp.lock().unwrap();
    let deact = guard.as_ref().expect("deactivated state missing");
    assert_eq!(
        deact.model_generation, 2,
        "the 48k model (second generation) must be the installed one"
    );

    let _ = std::fs::remove_file(&model_44k);
}

/// Notification ordering (Política-A acceptance): `RestartRequested` is emitted
/// before any latency change; `LatencyChanged` fires only after the restart
/// cycle applies the new physical latency.
#[test]
fn test_latency_notification_ordering() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");

    // Stabilize the initial reported latency on the main thread.
    instance.call_on_main_thread_callback();

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };

    let base = crate::clap::test_util::model_path("lstm.nam");
    let model_44k = write_model_with_rate(&base, 44100);
    mt.load_model(&model_44k).expect("load 44.1k model");

    let events_after_load = state.snapshot();
    assert!(
        events_after_load
            .iter()
            .any(|e| matches!(e, crate::clap::host_harness::HostEvent::RestartRequested)),
        "RestartRequested must be emitted when a latency-changing swap is staged"
    );
    assert!(
        !events_after_load
            .iter()
            .any(|e| matches!(e, crate::clap::host_harness::HostEvent::LatencyChanged)),
        "LatencyChanged must NOT fire while the restart is pending"
    );

    // Host restart applies the new latency.
    let _started_after = perform_restart(&mut instance, started, &state, audio_config());
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        expected_44k_stream_latency(),
        "activate() must publish the new physical latency"
    );

    // The main thread observes the change and notifies the host exactly once.
    instance.call_on_main_thread_callback();
    let events_after_restart = state.snapshot();
    let latency_changed = events_after_restart
        .iter()
        .filter(|e| matches!(e, crate::clap::host_harness::HostEvent::LatencyChanged))
        .count();
    assert_eq!(
        latency_changed, 1,
        "exactly one LatencyChanged must fire after the restart applies the latency"
    );
    let idx_restart = events_after_restart
        .iter()
        .position(|e| matches!(e, crate::clap::host_harness::HostEvent::RestartRequested));
    let idx_changed = events_after_restart
        .iter()
        .position(|e| matches!(e, crate::clap::host_harness::HostEvent::LatencyChanged));
    assert!(
        idx_restart.is_some_and(|r| idx_changed.is_some_and(|c| r < c)),
        "RestartRequested must precede LatencyChanged: {events_after_restart:#?}"
    );

    let _ = std::fs::remove_file(&model_44k);
}

fn load_state(
    instance: &mut PluginInstance<crate::clap::host_harness::CompleteHost>,
    params: &neural_amp_modeler_rs::common::params::ProcessingParams,
) {
    let state_ext = instance
        .plugin_handle()
        .get_extension::<clack_extensions::state::PluginState>()
        .expect("PluginState extension not found");
    let state_bytes = serde_json::to_vec(params).unwrap();
    let mut handle = instance.plugin_handle();
    state_ext
        .load(&mut handle, &mut state_bytes.as_slice())
        .expect("PluginState::load failed");
}

// ═══════════════════════════════════════════════════════════════════════════
// TR.2 — State Restore under Política A
// ═══════════════════════════════════════════════════════════════════════════

/// A state restore that keeps the exact same latency must be a continuous
/// hot restore through the SPSC: no restart requested, nothing staged, and the
/// restored model and UI parameters land deterministically.
#[test]
fn test_restore_same_latency_is_continuous_no_restart() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let mut started = stopped.start_processing().expect("start_processing");

    let model = crate::clap::test_util::model_path("lstm.nam");
    let params = neural_amp_modeler_rs::common::params::ProcessingParams {
        model_path: Some(model.clone()),
        model_basename: Some("lstm.nam".to_string()),
        model_hash: crate::clap::test_util::asset_hash(&model),
        input_gain_db: 3.5,
        ..Default::default()
    };
    let initial_gen = shared.cold.last_applied_generation.load(Ordering::Relaxed);
    load_state(&mut instance, &params);

    assert!(
        !state.restart_requested.load(Ordering::SeqCst),
        "same-latency restore must not request a host restart"
    );

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };
    assert!(
        mt.staged_restore.is_none(),
        "same-latency restore must not be staged"
    );

    // Process a block to drain SPSC and apply the transaction
    let mut il = vec![0.3f32; 256];
    let mut ir = vec![0.3f32; 256];
    let mut ol = vec![0.0f32; 256];
    let mut or = vec![0.0f32; 256];
    let _ = process_block_harness(&mut started, &mut il, &mut ir, &mut ol, &mut or, None);

    // Run housekeeping to publish acked restore
    instance.call_on_main_thread_callback();

    assert!(
        shared.cold.last_applied_generation.load(Ordering::Relaxed) > initial_gen,
        "restore transaction generation must be applied on the audio thread"
    );
    assert_eq!(
        f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed)),
        3.5,
        "UI params must be published after ack"
    );
}

/// A state restore that changes model rate (44.1k over 48k host) must be staged
/// and request a host restart; the DSP keeps the old latency until `activate()`.
#[test]
fn test_restore_diff_rate_stages_and_requests_restart() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let mut started = stopped.start_processing().expect("start_processing");

    let base = crate::clap::test_util::model_path("lstm.nam");
    let model_44k = write_model_with_rate(&base, 44100);
    let params = neural_amp_modeler_rs::common::params::ProcessingParams {
        model_path: Some(model_44k.clone()),
        model_basename: Some("model_44100.nam".to_string()),
        model_hash: crate::clap::test_util::asset_hash(&model_44k),
        input_gain_db: 4.5,
        ..Default::default()
    };

    let initial_gen = shared.cold.last_applied_generation.load(Ordering::Relaxed);
    load_state(&mut instance, &params);

    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "diff-rate restore must request a host restart"
    );

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };
    assert!(
        mt.staged_restore.is_some(),
        "diff-rate restore must be staged"
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        0,
        "reported latency must remain old while restart is pending"
    );
    assert_eq!(
        shared.cold.current_stream_latency.load(Ordering::Relaxed),
        0,
        "active stream latency must remain untouched before restart"
    );

    // Audio block processing does not install the staged restore
    let mut il = vec![0.3f32; 256];
    let mut ir = vec![0.3f32; 256];
    let mut ol = vec![0.0f32; 256];
    let mut or = vec![0.0f32; 256];
    let _ = process_block_harness(&mut started, &mut il, &mut ir, &mut ol, &mut or, None);
    assert_eq!(
        shared.cold.last_applied_generation.load(Ordering::Relaxed),
        initial_gen,
        "audio processing must not apply the staged restore"
    );

    // Host restart cycle: deactivate -> activate consumes the staged restore
    let _started_after = perform_restart(&mut instance, started, &state, audio_config());

    let expected_lat = expected_44k_stream_latency();
    assert_eq!(
        shared.cold.current_stream_latency.load(Ordering::Relaxed),
        expected_lat,
        "activate() must install the staged stream and set stream latency"
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        expected_lat,
        "reported latency must match new physical latency after restart"
    );
    assert!(
        shared.cold.last_applied_generation.load(Ordering::Relaxed) > initial_gen,
        "generation must advance after restart installs restore"
    );
    assert_eq!(
        f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed)),
        4.5,
        "parameters from restore must be published after restart"
    );

    let _ = std::fs::remove_file(&model_44k);
}

/// First IR restore (0 -> partition) and IR clear restore (partition -> 0)
/// must be staged + restart.
#[test]
fn test_restore_ir_stages_and_requests_restart() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");

    let ir = write_ir(512, "restore_ir");
    let params = neural_amp_modeler_rs::common::params::ProcessingParams {
        ir_path: Some(ir.clone()),
        ir_hash: crate::clap::test_util::asset_hash(&ir),
        ..Default::default()
    };

    load_state(&mut instance, &params);
    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "first IR restore (0 -> 256) must request restart"
    );

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };
    assert!(
        mt.staged_restore.is_some(),
        "first IR restore must be staged"
    );

    let started = perform_restart(&mut instance, started, &state, audio_config());
    assert_eq!(
        shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
        256,
        "restart must install the restored IR adapter"
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        256,
        "reported latency must include cabsim partition after restart"
    );

    // Clear IR via restore
    state.restart_requested.store(false, Ordering::SeqCst);
    let params_clear = neural_amp_modeler_rs::common::params::ProcessingParams {
        ir_path: None,
        ir_hash: None,
        ..Default::default()
    };
    load_state(&mut instance, &params_clear);
    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "IR clear restore (256 -> 0) must request restart"
    );

    let _started_after = perform_restart(&mut instance, started, &state, audio_config());
    assert_eq!(
        shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
        0,
        "restart must clear cabsim latency"
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        0,
        "reported latency must drop back to 0 after IR clear restore"
    );

    let _ = std::fs::remove_file(&ir);
}

/// Coalescing (latest-wins): a second restore replaces any pending staged restore;
/// if the second restore has the same latency as baseline, it applies continuously.
#[test]
fn test_restore_coalescing_latest_wins() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");

    let base = crate::clap::test_util::model_path("lstm.nam");
    let model_44k = write_model_with_rate(&base, 44100);
    let model_48k = crate::clap::test_util::model_path("wavenet_a1_standard.nam");

    // 1. First restore: 44.1k (staged + restart)
    let params_44k = neural_amp_modeler_rs::common::params::ProcessingParams {
        model_path: Some(model_44k.clone()),
        model_basename: Some("model_44100.nam".to_string()),
        model_hash: crate::clap::test_util::asset_hash(&model_44k),
        input_gain_db: 1.0,
        ..Default::default()
    };
    load_state(&mut instance, &params_44k);
    assert!(state.restart_requested.load(Ordering::SeqCst));

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };
    assert!(mt.staged_restore.is_some());

    // 2. Second restore before restart: 48k (same rate as baseline)
    let params_48k = neural_amp_modeler_rs::common::params::ProcessingParams {
        model_path: Some(model_48k.clone()),
        model_basename: Some("wavenet_a1_standard.nam".to_string()),
        model_hash: crate::clap::test_util::asset_hash(&model_48k),
        input_gain_db: 2.0,
        ..Default::default()
    };
    load_state(&mut instance, &params_48k);

    assert!(
        mt.staged_restore.is_none(),
        "second same-rate restore must supersede the staged restore"
    );

    // Perform restart cycle
    let mut started_after = perform_restart(&mut instance, started, &state, audio_config());
    assert_eq!(
        shared.cold.current_stream_latency.load(Ordering::Relaxed),
        0,
        "restart must not install the superseded 44.1k model"
    );

    let mut il = vec![0.3f32; 256];
    let mut ir = vec![0.3f32; 256];
    let mut ol = vec![0.0f32; 256];
    let mut or = vec![0.0f32; 256];
    let _ = process_block_harness(&mut started_after, &mut il, &mut ir, &mut ol, &mut or, None);
    let stopped = started_after.stop_processing();
    instance.deactivate(stopped);

    let guard = shared.cold.deactivated_dsp.lock().unwrap();
    let deact = guard.as_ref().expect("deactivated state missing");
    assert_eq!(
        deact.model_generation, 2,
        "generation 2 (wavenet) must be installed"
    );

    let _ = std::fs::remove_file(&model_44k);
}

/// A restore combining a diff-rate model and an oversample factor change requests
/// a single host restart and correctly installs both contributions on activate().
#[test]
fn test_restore_combined_model_and_oversample_single_restart() {
    use crate::clap::plugin::PendingRestartOs;

    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");

    let base = crate::clap::test_util::model_path("lstm.nam");
    let model_44k = write_model_with_rate(&base, 44100);

    let params = neural_amp_modeler_rs::common::params::ProcessingParams {
        model_path: Some(model_44k.clone()),
        model_basename: Some("model_44100.nam".to_string()),
        model_hash: crate::clap::test_util::asset_hash(&model_44k),
        oversample: OversampleFactor::X2,
        ..Default::default()
    };

    load_state(&mut instance, &params);
    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "combined model + OS change must request a restart"
    );

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };
    assert!(mt.staged_restore.is_some());
    assert_eq!(
        PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed),
        PendingRestartOs::Pending(OversampleFactor::X2)
    );

    // Single restart cycle installs BOTH
    let _started_after = perform_restart(&mut instance, started, &state, audio_config());

    let stream_lat = expected_44k_stream_latency();
    let os_lat =
        neural_amp_modeler_rs::dsp::oversample::OversampleEngine::new(OversampleFactor::X2, 256)
            .expect("build os engine")
            .latency_samples() as u32;
    let total_expected = stream_lat + os_lat;

    assert_eq!(
        shared.cold.current_stream_latency.load(Ordering::Relaxed),
        stream_lat
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        total_expected,
        "combined restore + OS must report sum of stream and OS latency"
    );

    let _ = std::fs::remove_file(&model_44k);
}

/// A restore occurring before plugin activation (buffer_size == 0) remains a
/// direct local commit with no restart requested.
#[test]
fn test_restore_pre_activate_remains_local_commit() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let base = crate::clap::test_util::model_path("lstm.nam");
    let model_44k = write_model_with_rate(&base, 44100);

    let params = neural_amp_modeler_rs::common::params::ProcessingParams {
        model_path: Some(model_44k.clone()),
        model_basename: Some("model_44100.nam".to_string()),
        model_hash: crate::clap::test_util::asset_hash(&model_44k),
        input_gain_db: 6.0,
        ..Default::default()
    };

    // Load state before activate (buffer_size == 0)
    load_state(&mut instance, &params);

    assert!(
        !state.restart_requested.load(Ordering::SeqCst),
        "pre-activate restore must not request a restart"
    );

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };
    assert!(
        mt.staged_restore.is_none(),
        "pre-activate restore must not be staged"
    );

    // Now activate:
    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");

    let expected_lat = expected_44k_stream_latency();
    assert_eq!(
        shared.cold.current_stream_latency.load(Ordering::Relaxed),
        expected_lat,
        "activate must install the restored 44.1k model with resampler"
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        expected_lat
    );

    let _ = started.stop_processing();
    let _ = std::fs::remove_file(&model_44k);
}

/// ForPreset restore without a model retains the active model across restart.
#[test]
fn test_restore_for_preset_without_model_preserves_active_model() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config())
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");

    let mt_ptr = extract_plugin_main_thread(&mut instance);
    let mt = unsafe { &mut *mt_ptr };

    // Initial 48k model load
    let model = crate::clap::test_util::model_path("lstm.nam");
    mt.load_model(&model).expect("load initial model");

    // Process a block to install model generation 1
    let mut started = started;
    let mut il = vec![0.3f32; 256];
    let mut ir_buf = vec![0.3f32; 256];
    let mut ol = vec![0.0f32; 256];
    let mut or = vec![0.0f32; 256];
    let _ = process_block_harness(&mut started, &mut il, &mut ir_buf, &mut ol, &mut or, None);

    // Restore with ForPreset context (e.g. preset change without model, but with an IR)
    let ir = write_ir(512, "preset_ir");
    let params = neural_amp_modeler_rs::common::params::ProcessingParams {
        model_path: None,
        model_basename: None,
        model_hash: None,
        ir_path: Some(ir.clone()),
        ir_hash: crate::clap::test_util::asset_hash(&ir),
        input_gain_db: 7.0,
        ..Default::default()
    };

    // Load state with state context = ForPreset
    let state_ctx_ext = instance
        .plugin_handle()
        .get_extension::<clack_extensions::state_context::PluginStateContext>()
        .expect("PluginStateContext extension");
    let state_bytes = serde_json::to_vec(&params).unwrap();
    let mut handle = instance.plugin_handle();
    state_ctx_ext
        .load(
            &mut handle,
            &mut state_bytes.as_slice(),
            clack_extensions::state_context::StateContextType::ForPreset,
        )
        .expect("StateContext::load for preset must succeed");

    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "preset restore adding an IR must request a restart"
    );

    let started_after = perform_restart(&mut instance, started, &state, audio_config());
    assert_eq!(
        shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
        256,
        "restart must install cabsim adapter"
    );

    // Verify existing model was preserved across restart
    let stopped = started_after.stop_processing();
    instance.deactivate(stopped);
    let guard = shared.cold.deactivated_dsp.lock().unwrap();
    let deact = guard.as_ref().expect("deactivated state missing");
    assert_eq!(
        deact.model_generation, 1,
        "existing model generation 1 must be preserved during ForPreset restore"
    );

    let _ = std::fs::remove_file(&ir);
}

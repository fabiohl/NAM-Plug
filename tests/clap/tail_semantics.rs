// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLAP tail extension (`clap_plugin_tail`) end-to-end through the real plugin
//! lifecycle: state load (model and optional IR), asynchronous model drain,
//! activation, processing, host-side `PluginTail::get`, deactivation and drop.
//!
//! Expectations are contracts observed through the host API, not a copy of the
//! production formula: with an IR loaded the reported tail must cover the full
//! IR ring-out, and without an IR it must stay within the fixed-latency budget.
//! Every instance is stopped and deactivated before it is dropped; nothing is
//! retained with `mem::forget`.

use clack_extensions::state::PluginState;
use clack_extensions::tail::{PluginTail, TailLength};
use clack_host::prelude::*;
use nam_plug::clap::test_util::{self, TestHost};
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::testing::wav::write_wav_f32;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: u32 = 256;
const IR_LEN: usize = 48_000;
const WARMUP_BLOCKS: usize = 64;
/// Measured: host tail without an IR is 0 samples for `lstm.nam` at 48 kHz (no resampler latency).
const NO_IR_TAIL: u32 = 0;
/// Measured: host tail with the `IR_LEN` IR is `IR_LEN + 384` samples at 48 kHz; the
/// excess must stay within one 512-sample convolution partition.
const IR_TAIL_SLACK: u32 = 512;

fn fixture_model() -> PathBuf {
    let model = test_util::model_path("lstm.nam");
    assert!(
        model.exists(),
        "lstm.nam fixture missing: {}",
        model.display()
    );
    model
}

/// Writes a `IR_LEN`-tap IR whose last tap is non-zero, so the ring-out really spans the whole IR.
fn write_long_ir(tag: &str) -> PathBuf {
    let mut ir = vec![0.0f32; IR_LEN];
    ir[0] = 1.0;
    ir[IR_LEN - 1] = 0.5;
    let path = std::env::temp_dir().join(format!(
        "nam_plug_tail_semantics_{tag}_{}.wav",
        std::process::id()
    ));
    write_wav_f32(&path, &ir, SAMPLE_RATE as u32).expect("failed to write IR WAV");
    path
}

fn load_state(instance: &mut PluginInstance<TestHost>, model: &Path, ir: Option<&Path>) {
    let mut params = ProcessingParams::default();
    params.model_path = Some(model.to_path_buf());
    params.model_hash = test_util::asset_hash(model);
    if let Some(ir) = ir {
        params.ir_path = Some(ir.to_path_buf());
        params.ir_hash = test_util::asset_hash(ir);
    }
    let state_bytes = serde_json::to_vec(&params).expect("serialize state");
    let state_ext = instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("PluginState extension must be exported");
    let handle = instance.plugin_handle();
    state_ext
        .load(&handle, &mut state_bytes.as_slice())
        .expect("state load must succeed");
    // SAFETY: `extract_shared` returns a pointer into the live plugin instance owned by this integration test; the instance is kept alive and unmoved for the borrow, access is through atomics/interior-mutable locks, and no `&mut` to the shared state is live.
    let shared = unsafe { &*test_util::extract_shared(instance) };
    test_util::drain_async_model_loading(instance, shared);
}

fn activate(instance: &mut PluginInstance<TestHost>) -> StartedPluginAudioProcessor<TestHost> {
    let stopped = instance
        .activate(
            |_, _| (),
            PluginAudioConfiguration {
                sample_rate: SAMPLE_RATE,
                min_frames_count: BLOCK,
                max_frames_count: BLOCK,
            },
        )
        .expect("activate must succeed");
    stopped
        .start_processing()
        .expect("start_processing must succeed")
}

fn process_silence(started: &mut StartedPluginAudioProcessor<TestHost>, blocks: usize) {
    let n = BLOCK as usize;
    for _ in 0..blocks {
        let mut in_l = vec![0.0f32; n];
        let mut in_r = vec![0.0f32; n];
        let mut out_l = vec![0.0f32; n];
        let mut out_r = vec![0.0f32; n];

        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);
        let in_ch = [in_l.as_mut_slice(), in_r.as_mut_slice()];
        let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                in_ch.into_iter().map(InputChannel::constant),
            ),
        }]);
        let out_ch = [out_l.as_mut_slice(), out_r.as_mut_slice()];
        let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
        }]);
        let mut events = EventBuffer::new();
        let mut out_events = OutputEvents::from_buffer(&mut events);

        started
            .process(
                &input_audio,
                &mut output_audio,
                &InputEvents::empty(),
                &mut out_events,
                None,
                None,
            )
            .expect("process must succeed");
    }
}

/// Reads the tail through the host-side CLAP call, exactly as a host does.
fn host_reported_tail(started: &mut StartedPluginAudioProcessor<TestHost>) -> u32 {
    let handle = started.plugin_handle();
    let tail_ext = handle
        .get_extension::<PluginTail>()
        .expect("PluginTail extension must be exported");
    match tail_ext.get(&handle) {
        TailLength::Finite(samples) => samples,
        TailLength::Infinite => panic!("tail must be finite for a finite IR"),
    }
}

fn teardown(
    instance: &mut PluginInstance<TestHost>,
    started: StartedPluginAudioProcessor<TestHost>,
) {
    let stopped = started.stop_processing();
    instance.deactivate(stopped);
}

#[test]
fn tail_without_ir_stays_within_fixed_latency_budget() {
    let model = fixture_model();
    let (_entry, _host, mut instance) = test_util::make_test_plugin();
    load_state(&mut instance, &model, None);

    let mut started = activate(&mut instance);
    process_silence(&mut started, WARMUP_BLOCKS);
    let tail = host_reported_tail(&mut started);

    assert_eq!(
        tail, NO_IR_TAIL,
        "tail without IR must equal the fixed-latency budget ({NO_IR_TAIL} samples)"
    );
    teardown(&mut instance, started);
}

#[test]
fn tail_with_ir_covers_full_ring_out_and_stays_bounded() {
    let model = fixture_model();
    let ir = write_long_ir("covers");
    let (_entry, _host, mut instance) = test_util::make_test_plugin();
    load_state(&mut instance, &model, Some(&ir));

    let mut started = activate(&mut instance);
    process_silence(&mut started, WARMUP_BLOCKS);
    let tail = host_reported_tail(&mut started);

    let _ = std::fs::remove_file(&ir);
    teardown(&mut instance, started);

    assert!(
        tail >= IR_LEN as u32,
        "host tail ({tail}) must cover the IR ring-out ({IR_LEN} samples)"
    );
    assert!(
        tail <= IR_LEN as u32 + IR_TAIL_SLACK,
        "host tail ({tail}) exceeds IR ({IR_LEN}) plus fixed budget ({IR_TAIL_SLACK})"
    );
}

#[test]
fn tail_returns_to_fixed_budget_after_ir_removed_on_reactivation() {
    let model = fixture_model();
    let ir = write_long_ir("removed");
    let (_entry, _host, mut instance) = test_util::make_test_plugin();

    load_state(&mut instance, &model, Some(&ir));
    let mut started = activate(&mut instance);
    process_silence(&mut started, WARMUP_BLOCKS);
    let tail_with_ir = host_reported_tail(&mut started);
    teardown(&mut instance, started);
    assert!(
        tail_with_ir >= IR_LEN as u32,
        "tail with IR ({tail_with_ir}) must cover IR ({IR_LEN})"
    );

    load_state(&mut instance, &model, None);
    let mut started = activate(&mut instance);
    process_silence(&mut started, WARMUP_BLOCKS);
    let tail_after_removal = host_reported_tail(&mut started);
    teardown(&mut instance, started);

    let _ = std::fs::remove_file(&ir);
    assert_eq!(
        tail_after_removal, NO_IR_TAIL,
        "tail after IR removal must return to the fixed budget ({NO_IR_TAIL} samples)"
    );
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! End-to-end integration tests for S08 — Single Cabinet Slot (WAV / NAM / NAMB) & Cab Bypass.
//!
//! Covers:
//! 1. Single exclusive slot accepting `.wav`, `.nam` (Linear, Sequential, LSTM), and `.namb`.
//! 2. Fail-closed rejection of WaveNet post-stack `head`, multi-channel models, and unsupported
//!    extensions in the cabinet slot while preserving the prior active cabinet cleanly (and leaving
//!    the amp slot completely unaffected).
//! 3. Strict isolation of cabinet `.nam`/`.namb` from amp calibration (`input_level_dbu`,
//!    `output_level_dbu`, `loudness`) and WAV `IrLevelPolicy`.
//! 4. Exact `f64` oracle parity for Linear & Sequential cabinet models at 48 kHz and multi-rate
//!    continuity across 44.1 / 48 / 96 kHz + non-aligned block sizes (`1`, `17`, `64`, `256`).
//! 5. Independent `PARAM_CAB_BYPASS` (constant latency, phase-aligned dry delay, click-free
//!    64-sample cosine crossfade, lazy filter reset on re-engage, master vs cab bypass separation,
//!    gate tail draining, and stopped/deactivated/reactivated lifecycle).
//! 6. Atomic state save/restore (`v1` -> `v2` compatibility, `cab_bypass`, missing cab fail-closed).
//! 7. Zero RT heap allocations (`--features heap-audit`).

use clack_extensions::latency::PluginLatency;
use clack_extensions::state::PluginState;
#[cfg(feature = "heap-audit")]
use clack_extensions::tail::{PluginTail, TailLength};
#[cfg(feature = "heap-audit")]
use clack_host::events::event_types::ParamValueEvent;
use clack_host::prelude::*;
use nam_plug::clap::extensions::calibration::{CalibrationConfig, OutputMode};
#[cfg(feature = "heap-audit")]
use nam_plug::clap::extensions::params::{PARAM_BYPASS, PARAM_CAB_BYPASS, PARAM_OUTPUT_MODE};
use nam_plug::clap::test_util::{self, TestHost};
#[cfg(feature = "heap-audit")]
use neural_amp_modeler_rs::common::alloc_audit::{TrackingGuard, get_alloc_count};
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::loader::nam_json::{WeightsLayout, parse_nam_json};
use neural_amp_modeler_rs::loader::namb_encoder::encode_namb;
use neural_amp_modeler_rs::testing::reference_oracle::{PrecisionConfig, oracle_forward};
use neural_amp_modeler_rs::testing::wav::write_wav_f32;
use std::io::Cursor;
use std::path::{Path, PathBuf};
#[cfg(feature = "heap-audit")]
use std::sync::Arc;
#[cfg(feature = "heap-audit")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(feature = "heap-audit")]
struct CabAuditHost;

#[cfg(feature = "heap-audit")]
struct CabAuditShared {
    restart_was_called: Arc<AtomicBool>,
}

#[cfg(feature = "heap-audit")]
impl SharedHandler<'_> for CabAuditShared {
    fn initializing(&self, _instance: InitializingPluginHandle<'_>) {}
    fn request_restart(&self) {
        self.restart_was_called.store(true, Ordering::SeqCst);
    }
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

#[cfg(feature = "heap-audit")]
impl HostHandlers for CabAuditHost {
    type Shared<'a> = CabAuditShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

const BLOCK: usize = 256;

fn unique_tmp_path(tag: &str, ext: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "nam_plug_s08_{tag}_{}_{nanos}.{ext}",
        std::process::id()
    ))
}

fn write_text_asset(tag: &str, ext: &str, content: &str) -> PathBuf {
    let path = unique_tmp_path(tag, ext);
    std::fs::write(&path, content.as_bytes()).expect("write text asset");
    path
}

fn linear_cab_json(
    gain: f32,
    sample_rate: u32,
    gear_type: Option<&str>,
    amp_cal_tags: bool,
) -> String {
    let mut meta_entries = Vec::new();
    if let Some(gt) = gear_type {
        meta_entries.push(format!(
            r#""gear_type":"{gt}","gear_make":"FixtureCo","gear_model":"V30-1x12""#
        ));
    }
    if amp_cal_tags {
        meta_entries
            .push(r#""input_level_dbu":24.0,"output_level_dbu":-12.0,"loudness":-6.0"#.to_string());
    }
    let meta_block = if meta_entries.is_empty() {
        String::new()
    } else {
        format!(r#","metadata":{{{}}}"#, meta_entries.join(","))
    };
    format!(
        r#"{{
            "version":"0.6.0",
            "architecture":"Linear",
            "sample_rate":{sample_rate},
            "config":{{"receptive_field":2,"bias":false}},
            "weights":[{gain}, 0.0]
            {meta_block}
        }}"#
    )
}

fn sequential_cab_json(sample_rate: u32) -> String {
    format!(
        r#"{{
            "version":"0.6.0",
            "architecture":"Sequential",
            "sample_rate":{sample_rate},
            "config":{{
                "models":[
                    {{
                        "version":"0.6.0",
                        "architecture":"Linear",
                        "sample_rate":{sample_rate},
                        "config":{{"receptive_field":1,"bias":false}},
                        "weights":[0.5]
                    }},
                    {{
                        "version":"0.6.0",
                        "architecture":"Linear",
                        "sample_rate":{sample_rate},
                        "config":{{"receptive_field":1,"bias":true}},
                        "weights":[0.8, 0.0]
                    }}
                ]
            }},
            "weights":[]
        }}"#
    )
}

fn lstm_cab_json(sample_rate: u32) -> String {
    format!(
        r#"{{
            "version":"0.6.0",
            "architecture":"LSTM",
            "sample_rate":{sample_rate},
            "config":{{"num_layers":1,"input_size":1,"hidden_size":1}},
            "weights":[
                0.0, 0.0, 0.0, 0.0,
                0.0, 0.0, 0.0, 0.0,
                0.0, 0.0, 0.0, 0.0,
                0.0, 0.0,
                0.75, 0.0
            ]
        }}"#
    )
}

fn wavenet_with_head_json(sample_rate: u32) -> String {
    format!(
        r#"{{
            "version":"0.6.0",
            "architecture":"WaveNet",
            "sample_rate":{sample_rate},
            "config":{{
                "layers":[{{
                    "input_size":1,"condition_size":1,"channels":1,"head_size":1,
                    "kernel_size":1,"dilations":[1],"activation":"ReLU","gated":false,"head_bias":false
                }}],
                "head_scale":1.0,
                "head":{{"in_channels":1,"channels":1,"activation":"Tanh","out_channels":1,"kernel_sizes":[1]}}
            }},
            "weights":[1.0,0.0,1.0,1.0,0.0,1.0,1.0,0.0]
        }}"#
    )
}

fn wavenet_two_channel_json(sample_rate: u32) -> String {
    format!(
        r#"{{
            "version":"0.6.0",
            "architecture":"WaveNet",
            "sample_rate":{sample_rate},
            "config":{{
                "in_channels":2,
                "layers":[{{
                    "input_size":2,"condition_size":2,"channels":1,"head_size":1,
                    "kernel_size":1,"dilations":[1],"activation":"ReLU","gated":false,"head_bias":false
                }}],
                "head_scale":1.0
            }},
            "weights":[1.0,0.0,0.0,1.0,1.0,1.0,0.0,1.0,0.0]
        }}"#
    )
}

fn cab_kind_str(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?;
    if ext.eq_ignore_ascii_case("wav") {
        Some("wav-ir")
    } else if ext.eq_ignore_ascii_case("nam") {
        Some("nam-model")
    } else if ext.eq_ignore_ascii_case("namb") {
        Some("namb-model")
    } else {
        None
    }
}

fn make_v2_state(
    model_path: Option<&Path>,
    cab_path: Option<&Path>,
    ir_policy_str: &str,
    cal: CalibrationConfig,
    cab_bypass: bool,
) -> Vec<u8> {
    let mut params = ProcessingParams::default();
    params.gate_threshold_db = -96.0;
    if let Some(m) = model_path {
        params.model_path = Some(m.to_path_buf());
        params.model_hash = test_util::asset_hash(m);
    }
    if let Some(c) = cab_path {
        params.ir_path = Some(c.to_path_buf());
        params.ir_hash = test_util::asset_hash(c);
    }
    let cab_kind = cab_path.and_then(cab_kind_str);
    let envelope = serde_json::json!({
        "version": 2,
        "params": params,
        "ir_level_policy": ir_policy_str,
        "calibration": cal,
        "gate_mode": "silence",
        "cab_bypass": cab_bypass,
        "cab_kind": cab_kind,
    });
    serde_json::to_vec(&envelope).expect("serialize state envelope")
}

fn load_state_generic<H: HostHandlers>(
    instance: &mut PluginInstance<H>,
    bytes: &[u8],
) -> Result<(), String> {
    let state_ext = instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("PluginState");
    let handle = instance.plugin_handle();
    let mut cursor = Cursor::new(bytes);
    state_ext
        .load(&handle, &mut cursor)
        .map_err(|e| format!("{e:?}"))
}

fn load_state(instance: &mut PluginInstance<TestHost>, bytes: &[u8]) -> Result<(), String> {
    load_state_generic(instance, bytes)
}

#[cfg(feature = "heap-audit")]
fn save_state(instance: &mut PluginInstance<TestHost>) -> Vec<u8> {
    let state_ext = instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("PluginState");
    let handle = instance.plugin_handle();
    let mut out = Vec::new();
    state_ext.save(&handle, &mut out).expect("save state");
    out
}

fn activate_with_block<H>(
    instance: &mut PluginInstance<H>,
    sample_rate: f64,
    block_size: usize,
) -> StartedPluginAudioProcessor<H>
where
    H: for<'a> HostHandlers<AudioProcessor<'a> = ()>,
{
    let stopped = instance
        .activate(
            |_, _| (),
            PluginAudioConfiguration {
                sample_rate,
                min_frames_count: 1,
                max_frames_count: block_size.max(BLOCK) as u32,
            },
        )
        .expect("activate");
    stopped.start_processing().expect("start_processing")
}

fn process_blocks_impl<H: HostHandlers>(
    proc: &mut StartedPluginAudioProcessor<H>,
    in_l: &[f32],
    in_r: &[f32],
    block_size: usize,
    audit_rt: bool,
    mut per_block_events: impl FnMut(usize, &mut EventBuffer),
) -> (Vec<f32>, Vec<f32>) {
    assert_eq!(in_l.len(), in_r.len());
    let mut out_l = Vec::with_capacity(in_l.len());
    let mut out_r = Vec::with_capacity(in_r.len());

    let mut blk_in_l = vec![0.0f32; block_size];
    let mut blk_in_r = vec![0.0f32; block_size];
    let mut blk_out_l = vec![0.0f32; block_size];
    let mut blk_out_r = vec![0.0f32; block_size];
    let mut ev_buf = EventBuffer::new();
    let mut in_ports = AudioPorts::with_capacity(2, 1);
    let mut out_ports = AudioPorts::with_capacity(2, 1);

    let mut offset = 0usize;
    let mut block_idx = 0usize;
    while offset < in_l.len() {
        let n = (in_l.len() - offset).min(block_size);
        blk_in_l[..n].copy_from_slice(&in_l[offset..offset + n]);
        blk_in_r[..n].copy_from_slice(&in_r[offset..offset + n]);
        blk_out_l[..n].fill(0.0);
        blk_out_r[..n].fill(0.0);

        let mut in_ch = [&mut blk_in_l[..n], &mut blk_in_r[..n]];
        let in_audio = in_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                in_ch.iter_mut().map(InputChannel::constant),
            ),
        }]);
        let out_ch = [&mut blk_out_l[..n], &mut blk_out_r[..n]];
        let mut out_audio = out_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
        }]);

        ev_buf.clear();
        per_block_events(block_idx, &mut ev_buf);
        let in_events = InputEvents::from_buffer(&ev_buf);
        let mut out_events = OutputEvents::void();

        #[cfg(feature = "heap-audit")]
        let allocs = if audit_rt {
            let _guard = TrackingGuard::new();
            proc.process(
                &in_audio,
                &mut out_audio,
                &in_events,
                &mut out_events,
                None,
                None,
            )
            .expect("process");
            get_alloc_count()
        } else {
            proc.process(
                &in_audio,
                &mut out_audio,
                &in_events,
                &mut out_events,
                None,
                None,
            )
            .expect("process");
            0
        };
        #[cfg(not(feature = "heap-audit"))]
        let allocs = {
            let _ = audit_rt;
            proc.process(
                &in_audio,
                &mut out_audio,
                &in_events,
                &mut out_events,
                None,
                None,
            )
            .expect("process");
            0usize
        };
        assert_eq!(
            allocs, 0,
            "RT proc.process() must allocate zero heap memory at block {block_idx}"
        );

        out_l.extend_from_slice(&blk_out_l[..n]);
        out_r.extend_from_slice(&blk_out_r[..n]);
        offset += n;
        block_idx += 1;
    }

    (out_l, out_r)
}

fn process_blocks<H: HostHandlers>(
    proc: &mut StartedPluginAudioProcessor<H>,
    in_l: &[f32],
    in_r: &[f32],
    block_size: usize,
    per_block_events: impl FnMut(usize, &mut EventBuffer),
) -> (Vec<f32>, Vec<f32>) {
    process_blocks_impl(proc, in_l, in_r, block_size, false, per_block_events)
}

#[cfg(feature = "heap-audit")]
fn process_blocks_audited<H: HostHandlers>(
    proc: &mut StartedPluginAudioProcessor<H>,
    in_l: &[f32],
    in_r: &[f32],
    block_size: usize,
    per_block_events: impl FnMut(usize, &mut EventBuffer),
) -> (Vec<f32>, Vec<f32>) {
    process_blocks_impl(proc, in_l, in_r, block_size, true, per_block_events)
}

#[cfg(feature = "heap-audit")]
fn push_bool_param(ev: &mut EventBuffer, param_id: u32, val: bool) {
    let event = ParamValueEvent::new(
        0,
        ClapId::new(param_id),
        Pckn::match_all(),
        if val { 1.0 } else { 0.0 },
    );
    ev.push(&event);
}

#[cfg(feature = "heap-audit")]
fn push_u32_param(ev: &mut EventBuffer, param_id: u32, val: u32) {
    let event = ParamValueEvent::new(0, ClapId::new(param_id), Pckn::match_all(), val as f64);
    ev.push(&event);
}

#[test]
fn t08_2_fail_closed_guards_preserve_active_cab_and_leave_amp_unaffected() {
    let (_entry, _host, mut instance) = test_util::make_test_plugin();
    let valid_cab = write_text_asset(
        "valid_cab",
        "nam",
        &linear_cab_json(0.5, 48_000, Some("Cabinet"), false),
    );
    let head_cab = write_text_asset("head_cab", "nam", &wavenet_with_head_json(48_000));
    let stereo_cab = write_text_asset("stereo_cab", "nam", &wavenet_two_channel_json(48_000));
    let txt_cab = write_text_asset("bad_ext", "txt", "not a cab");

    // 1. Install a valid 0.5x linear cabinet via atomic state restore.
    let valid_state = make_v2_state(
        None,
        Some(&valid_cab),
        "peak-normalized",
        CalibrationConfig::default(),
        false,
    );
    load_state(&mut instance, &valid_state).expect("valid cabinet state must load");

    // 2. Reject invalid cabinet state reloads fail-closed without disturbing the active cabinet.
    for bad_path in [&head_cab, &stereo_cab, &txt_cab] {
        let mut params = ProcessingParams::default();
        params.gate_threshold_db = -96.0;
        params.ir_path = Some(bad_path.clone());
        params.ir_hash = test_util::asset_hash(bad_path);
        let bad_state = serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "params": params,
            "ir_level_policy": "peak-normalized",
            "calibration": CalibrationConfig::default(),
        }))
        .expect("serialize bad state");
        assert!(
            load_state(&mut instance, &bad_state).is_err(),
            "cabinet state pointing to {:?} must fail closed",
            bad_path
        );
    }

    // 3. Process audio: the preserved 0.5x linear cabinet must remain active!
    let mut proc = activate_with_block(&mut instance, 48_000.0, BLOCK);
    let inp = vec![0.4f32; BLOCK];
    let (out_l, out_r) = process_blocks(&mut proc, &inp, &inp, BLOCK, |_, _| {});
    assert!(
        (out_l[10] - 0.2).abs() < 1e-5,
        "expected 0.2, got {}",
        out_l[10]
    );
    assert!((out_r[10] - 0.2).abs() < 1e-5);
    instance.deactivate(proc.stop_processing());

    // 4. Amp slot still accepts standard WaveNet & layer-head A2 models while cabinet holds 1x1 NAM.
    let amp_fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models/a2_example.nam");
    let amp_state = make_v2_state(
        Some(&amp_fixture),
        Some(&valid_cab),
        "peak-normalized",
        CalibrationConfig::default(),
        false,
    );
    load_state(&mut instance, &amp_state)
        .expect("A2 layer-head amp model remains allowed in AMP slot while cabinet holds 1x1 NAM");

    // 5. Legacy v1 state compatibility: restores cab and defaults cab_bypass to false.
    let mut v1_params = ProcessingParams::default();
    v1_params.gate_threshold_db = -96.0;
    v1_params.ir_path = Some(valid_cab.clone());
    v1_params.ir_hash = test_util::asset_hash(&valid_cab);
    let v1_bytes = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "params": v1_params,
    }))
    .expect("serialize v1 state");
    load_state(&mut instance, &v1_bytes).expect("v1 state must load with cab_bypass=false");

    for p in [valid_cab, head_cab, stereo_cab, txt_cab] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn t08_2_single_slot_exclusivity_metadata_isolation_and_oracle_parity() {
    let wav_ir = unique_tmp_path("unity_wav", "wav");
    write_wav_f32(&wav_ir, &[0.25, 0.0, 0.0, 0.0], 48_000).expect("write wav ir");
    let lin_json = linear_cab_json(0.5, 48_000, Some("Amp"), true);
    let lin_nam = write_text_asset("oracle_lin", "nam", &lin_json);
    let seq_json = sequential_cab_json(48_000);
    let seq_nam = write_text_asset("oracle_seq", "nam", &seq_json);
    let lstm_nam = write_text_asset("oracle_lstm", "nam", &lstm_cab_json(48_000));

    // Also encode Linear cabinet to `.namb` binary.
    let lin_parsed = parse_nam_json(&lin_json).expect("parse linear json");
    let namb_bytes = encode_namb(&lin_parsed, 2, WeightsLayout::Original).expect("encode namb");
    let lin_namb = unique_tmp_path("oracle_lin", "namb");
    std::fs::write(&lin_namb, &namb_bytes).expect("write namb");

    let (_entry, _host, mut instance) = test_util::make_test_plugin();

    // Signal input (asymmetric stereo to verify true dual-mono processing).
    let in_l: Vec<f32> = (0..BLOCK).map(|i| 0.3 * (i as f32 * 0.05).sin()).collect();
    let in_r: Vec<f32> = (0..BLOCK)
        .map(|i| -0.15 * (i as f32 * 0.08).cos())
        .collect();

    // A) WAV IR with PeakNormalized -> latency=BLOCK.
    let wav_state = make_v2_state(
        None,
        Some(&wav_ir),
        "peak-normalized",
        CalibrationConfig::default(),
        false,
    );
    load_state(&mut instance, &wav_state).expect("load WAV");
    let mut proc = activate_with_block(&mut instance, 48_000.0, BLOCK);
    let _ = process_blocks(
        &mut proc,
        &vec![0.0; BLOCK],
        &vec![0.0; BLOCK],
        BLOCK,
        |_, _| {},
    );
    let _ = process_blocks(&mut proc, &in_l, &in_r, BLOCK, |_, _| {});
    instance.call_on_main_thread_callback();
    let lat_ext = instance
        .plugin_handle()
        .get_extension::<PluginLatency>()
        .expect("PluginLatency");
    let h = instance.plugin_handle();
    assert_eq!(lat_ext.get(&h), BLOCK as u32);
    instance.deactivate(proc.stop_processing());

    // B) Replace single slot with Linear `.nam` stamped with heavy amp calibration tags + CalibrateInput=true + Calibrated.
    // Cabinet `.nam` must IGNORE all amp calibration tags and WAV IrLevelPolicy!
    let cal = CalibrationConfig {
        calibrate_input: true,
        input_calibration_level_dbu: 6.0,
        output_mode: OutputMode::Calibrated,
    };
    let lin_state = make_v2_state(None, Some(&lin_nam), "legacy-nam-plugin", cal, false);
    load_state(&mut instance, &lin_state).expect("replace WAV with Linear NAM");
    let mut proc = activate_with_block(&mut instance, 48_000.0, BLOCK);
    let (out_l, out_r) = process_blocks(&mut proc, &in_l, &in_r, BLOCK, |_, _| {});
    instance.call_on_main_thread_callback();
    let h = instance.plugin_handle();
    assert_eq!(
        lat_ext.get(&h),
        0,
        "48kHz NAM cab has 0 latency (replaced WAV exclusivity)"
    );

    let data = parse_nam_json(&lin_json).expect("parse");
    let in_l64: Vec<f64> = in_l.iter().map(|&x| f64::from(x)).collect();
    let in_r64: Vec<f64> = in_r.iter().map(|&x| f64::from(x)).collect();
    let oracle_l = oracle_forward(&data, &in_l64, &PrecisionConfig::default());
    let oracle_r = oracle_forward(&data, &in_r64, &PrecisionConfig::default());
    for i in 4..BLOCK {
        assert!(
            (f64::from(out_l[i]) - oracle_l[i]).abs() < 1e-6,
            "Linear L mismatch at {i}: got {}, expected {}",
            out_l[i],
            oracle_l[i]
        );
        assert!(
            (f64::from(out_r[i]) - oracle_r[i]).abs() < 1e-6,
            "Linear dual-mono R mismatch at {i}: got {}, expected {}",
            out_r[i],
            oracle_r[i]
        );
    }
    instance.deactivate(proc.stop_processing());

    // C) Replace with Sequential `.nam` & compare vs composed f64 submodel oracle.
    let seq_state = make_v2_state(None, Some(&seq_nam), "preserved", cal, false);
    load_state(&mut instance, &seq_state).expect("load Sequential NAM");
    let mut proc = activate_with_block(&mut instance, 48_000.0, BLOCK);
    let (out_l, _) = process_blocks(&mut proc, &in_l, &in_r, BLOCK, |_, _| {});
    let seq_data = parse_nam_json(&seq_json).expect("parse seq");
    let mut seq_oracle = in_l64.clone();
    for child in seq_data
        .config
        .models
        .as_deref()
        .expect("Sequential child models")
    {
        let sub_data = parse_nam_json(&child.to_string()).expect("parse Sequential child");
        seq_oracle = oracle_forward(&sub_data, &seq_oracle, &PrecisionConfig::default());
    }
    for i in 4..BLOCK {
        assert!(
            (f64::from(out_l[i]) - seq_oracle[i]).abs() < 1e-5,
            "Sequential mismatch at {i}: got {}, expected {}",
            out_l[i],
            seq_oracle[i]
        );
    }
    instance.deactivate(proc.stop_processing());

    // D) Replace with LSTM `.nam` and `.namb` binary (matching Linear oracle).
    let lstm_state = make_v2_state(None, Some(&lstm_nam), "preserved", cal, false);
    load_state(&mut instance, &lstm_state).expect("load LSTM NAM");
    let mut proc = activate_with_block(&mut instance, 48_000.0, BLOCK);
    let (lstm_l, _) = process_blocks(&mut proc, &in_l, &in_r, BLOCK, |_, _| {});
    assert!(lstm_l.iter().all(|x| x.is_finite()));
    instance.deactivate(proc.stop_processing());

    let namb_state = make_v2_state(None, Some(&lin_namb), "preserved", cal, false);
    load_state(&mut instance, &namb_state).expect("load Linear NAMB");
    let mut proc = activate_with_block(&mut instance, 48_000.0, BLOCK);
    let (namb_l, namb_r) = process_blocks(&mut proc, &in_l, &in_r, BLOCK, |_, _| {});
    for i in 4..BLOCK {
        assert!((f64::from(namb_l[i]) - oracle_l[i]).abs() < 1e-6);
        assert!((f64::from(namb_r[i]) - oracle_r[i]).abs() < 1e-6);
    }
    instance.deactivate(proc.stop_processing());

    for p in [wav_ir, lin_nam, seq_nam, lstm_nam, lin_namb] {
        let _ = std::fs::remove_file(p);
    }
}

// long: heap-audit only; the allocation assertion is meaningless without
// `--features heap-audit`, so this test must not run in the plain quick suite.
#[cfg(feature = "heap-audit")]
#[test]
fn t08_3_cab_bypass_constant_latency_phase_crossfade_and_heap_audit() {
    let wav_ir = unique_tmp_path("byp_wav", "wav");
    write_wav_f32(&wav_ir, &[0.5, 0.25, 0.0, 0.0], 48_000).expect("write wav");
    let resamp_nam = write_text_asset(
        "byp_44k",
        "nam",
        &linear_cab_json(0.5, 44_100, Some("Cabinet"), false),
    );
    let eq_nam = write_text_asset(
        "byp_48k",
        "nam",
        &linear_cab_json(0.5, 48_000, Some("Cabinet"), false),
    );

    // Part 1: 48 kHz Linear NAM at 48 kHz (zero latency, exact 0.5x vs 1.0x cab bypass comparison).
    let (_entry, _host, mut instance) = test_util::make_test_plugin();
    let state_bytes = make_v2_state(
        None,
        Some(&eq_nam),
        "preserved",
        CalibrationConfig::default(),
        false,
    );
    load_state(&mut instance, &state_bytes).expect("load 48k cab");
    let mut proc = activate_with_block(&mut instance, 48_000.0, BLOCK);

    let total = BLOCK * 6;
    let signal: Vec<f32> = (0..total)
        .map(|i| 0.4 * (2.0 * std::f32::consts::PI * 220.0 * (i as f32) / 48_000.0).sin())
        .collect();

    let (out_l, out_r) = process_blocks_audited(&mut proc, &signal, &signal, BLOCK, |blk, ev| {
        if blk == 1 {
            push_bool_param(ev, PARAM_CAB_BYPASS, true);
        } else if blk == 3 {
            push_bool_param(ev, PARAM_CAB_BYPASS, false);
        } else if blk == 4 {
            push_u32_param(ev, PARAM_OUTPUT_MODE, 0);
        } else if blk == 5 {
            push_bool_param(ev, PARAM_BYPASS, true);
        }
    });

    assert!((out_l[50] - 0.5 * signal[50]).abs() < 1e-4);
    assert!((out_r[50] - 0.5 * signal[50]).abs() < 1e-4);

    let b2 = BLOCK * 2 + 32;
    assert!((out_l[b2] - signal[b2]).abs() < 1e-4);

    let b4 = BLOCK * 4 + 32;
    assert!((out_l[b4] - 0.5 * signal[b4]).abs() < 1e-4);

    for w_start in [BLOCK - 1, BLOCK * 3 - 1] {
        for i in w_start..(w_start + 80).min(total - 1) {
            let step = (out_l[i + 1] - out_l[i]).abs();
            assert!(
                step < 0.08,
                "discontinuity at sample {i} around bypass transition: step={step}"
            );
        }
    }
    instance.deactivate(proc.stop_processing());

    // Part 2: Resampled 44.1 kHz NAM cab @ 48 kHz host -> constant latency + phase-aligned dry delay + zero host restarts on toggle!
    let restart_flag = Arc::new(AtomicBool::new(false));
    let (resamp_entry, resamp_host_info) = (
        PluginEntry::load_from_clack::<
            clack_plugin::entry::SinglePluginEntry<nam_plug::clap::plugin::NamClapPlugin>,
        >(c"/test")
        .expect("load entry"),
        HostInfo::new("Test", "Test", "Test", "0.1.0").expect("host info"),
    );
    let mut resamp_instance = PluginInstance::<CabAuditHost>::new(
        {
            let restart_flag = Arc::clone(&restart_flag);
            move |_| CabAuditShared {
                restart_was_called: restart_flag,
            }
        },
        |_| (),
        &resamp_entry,
        c"br.eti.fabiolima.nam-plug",
        &resamp_host_info,
    )
    .expect("instantiate plugin");
    let resamp_state = make_v2_state(
        None,
        Some(&resamp_nam),
        "preserved",
        CalibrationConfig::default(),
        false,
    );
    load_state_generic(&mut resamp_instance, &resamp_state).expect("load 44.1k resamp cab");
    let mut proc = activate_with_block(&mut resamp_instance, 48_000.0, BLOCK);
    let _ = process_blocks(
        &mut proc,
        &vec![0.0; BLOCK],
        &vec![0.0; BLOCK],
        BLOCK,
        |_, _| {},
    );
    resamp_instance.call_on_main_thread_callback();
    let cab_latency = {
        let h = resamp_instance.plugin_handle();
        let lat_ext = h.get_extension::<PluginLatency>().expect("PluginLatency");
        lat_ext.get(&h)
    };
    assert!(
        cab_latency > 0,
        "44.1 kHz cab @ 48 kHz host must report non-zero resampler latency"
    );
    restart_flag.store(false, Ordering::SeqCst);

    let (resamp_out, _) = process_blocks(&mut proc, &signal, &signal, BLOCK, |blk, ev| {
        if blk == 2 {
            push_bool_param(ev, PARAM_CAB_BYPASS, true);
        }
    });
    assert!(
        !restart_flag.load(Ordering::SeqCst),
        "toggling PARAM_CAB_BYPASS must NEVER trigger host restart"
    );
    resamp_instance.call_on_main_thread_callback();
    let cab_latency_after = {
        let h = resamp_instance.plugin_handle();
        let lat_ext = h.get_extension::<PluginLatency>().expect("PluginLatency");
        lat_ext.get(&h)
    };
    assert_eq!(
        cab_latency_after, cab_latency,
        "announced latency must stay constant when cabinet is bypassed"
    );

    let check_idx = BLOCK * 4 + cab_latency as usize + 16;
    assert!(
        (resamp_out[check_idx] - signal[check_idx - cab_latency as usize]).abs() < 1e-4,
        "phase-aligned cab bypass mismatch at {check_idx}: got {}, expected {}",
        resamp_out[check_idx],
        signal[check_idx - cab_latency as usize]
    );
    resamp_instance.deactivate(proc.stop_processing());

    // Part 3: WAV IR cab bypass, tail reporting, deactivation/reactivation persistence, and multi-rate + unaligned block sizes!
    let wav_state_byp = make_v2_state(
        None,
        Some(&wav_ir),
        "peak-normalized",
        CalibrationConfig::default(),
        true,
    );
    load_state(&mut instance, &wav_state_byp).expect("load WAV with cab_bypass=true");

    for (rate, blk) in [(44_100.0, 17usize), (48_000.0, 64), (96_000.0, 256)] {
        let mut proc = activate_with_block(&mut instance, rate, blk);
        let tiny_in = vec![0.25f32; blk * 4];
        let (out_byp, _) = process_blocks(&mut proc, &tiny_in, &tiny_in, blk, |_, _| {});
        assert!(out_byp.iter().all(|x| x.is_finite()));
        let mut stopped = proc.stop_processing();
        let tail_ext = instance.plugin_handle().get_extension::<PluginTail>();
        if let Some(tail) = tail_ext {
            let ph = stopped.plugin_handle();
            let t = tail.get(&ph);
            assert!(matches!(t, TailLength::Finite(_)));
        }
        instance.deactivate(stopped);
    }

    let saved = save_state(&mut instance);
    let parsed_saved: serde_json::Value = serde_json::from_slice(&saved).expect("json");
    assert_eq!(parsed_saved["cab_bypass"], true);

    for p in [wav_ir, resamp_nam, eq_nam] {
        let _ = std::fs::remove_file(p);
    }
}

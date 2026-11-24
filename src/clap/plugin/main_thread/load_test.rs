// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

fn write_temp_nam(name: &str, json: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("nam_plug_s06_{}_{name}.nam", std::process::id()));
    std::fs::write(&path, json).expect("write temp .nam fixture");
    path
}

#[test]
fn prepare_model_preserves_unknown_activation_code() {
    let sys = SystemSnapshot::capture();

    let unknown_act_json = r#"{
        "version": "0.6.0",
        "architecture": "WaveNet",
        "sample_rate": 48000,
        "config": {
            "layers": [{
                "input_size": 1,
                "condition_size": 1,
                "channels": 1,
                "head_size": 1,
                "kernel_size": 1,
                "dilations": [1],
                "activation": "NotARealActivation",
                "gated": false,
                "head_bias": false
            }],
            "head_scale": 1.0
        },
        "weights": [1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0, 0.0]
    }"#;
    let p1 = write_temp_nam("unknown_act", unknown_act_json);
    let err1 = prepare_model_offthread(&p1, 48000, 128, &sys)
        .err()
        .expect("unknown activation must fail");
    let _ = std::fs::remove_file(&p1);
    assert_eq!(err1.error_code(), NamErrorCode::NamJsonUnknownActivation);
}

#[test]
fn prepare_model_preserves_invalid_gating_mode_code() {
    let sys = SystemSnapshot::capture();

    let bad_gating_json = r#"{
        "version": "0.6.0",
        "architecture": "WaveNet",
        "sample_rate": 48000,
        "config": {
            "layers": [{
                "input_size": 1,
                "condition_size": 1,
                "channels": 1,
                "head_size": 1,
                "kernel_size": 1,
                "dilations": [1],
                "activation": "ReLU",
                "gating_mode": "mystery_gate",
                "head_bias": false
            }],
            "head_scale": 1.0
        },
        "weights": [1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0, 0.0]
    }"#;
    let p2 = write_temp_nam("bad_gating", bad_gating_json);
    let err2 = prepare_model_offthread(&p2, 48000, 128, &sys)
        .err()
        .expect("invalid gating_mode must fail");
    let _ = std::fs::remove_file(&p2);
    assert_eq!(err2.error_code(), NamErrorCode::InvalidModelTopology);
}

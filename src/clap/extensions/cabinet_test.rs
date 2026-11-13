// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

fn minimal_linear_json(sample_rate: u32, gear_type: Option<&str>) -> Vec<u8> {
    let meta = match gear_type {
        Some(gt) => {
            format!(
                r#","metadata":{{"gear_type":"{gt}","gear_make":"Test","gear_model":"Cab112"}}"#
            )
        }
        None => String::new(),
    };
    format!(
        r#"{{
            "version":"0.6.0",
            "architecture":"Linear",
            "sample_rate":{sample_rate},
            "config":{{"receptive_field":1,"bias":true}},
            "weights":[0.5,0.0]
            {meta}
        }}"#
    )
    .into_bytes()
}

#[test]
fn rejects_wavenet_post_stack_head_in_cabinet_slot() {
    let sys = SystemSnapshot::capture();
    let json = r#"{
        "version":"0.6.0",
        "architecture":"WaveNet",
        "sample_rate":48000,
        "config":{
            "layers":[{
                "input_size":1,"condition_size":1,"channels":1,"head_size":1,
                "kernel_size":1,"dilations":[1],"activation":"ReLU","gated":false,"head_bias":false
            }],
            "head_scale":1.0,
            "head":{"in_channels":1,"channels":1,"activation":"Tanh","out_channels":1,"kernel_sizes":[1]}
        },
        "weights":[1.0,0.0,1.0,1.0,0.0,1.0,1.0,0.0]
    }"#;
    let err = build_cab_nam_from_bytes(
        json.as_bytes(),
        CabAssetKind::NamModel,
        Path::new("/tmp/cab_head.nam"),
        48_000,
        128,
        true,
        &sys,
    )
    .err()
    .expect("WaveNet post-stack head must be rejected in cab slot");
    assert!(
        err.user_message().contains("head"),
        "unexpected error: {}",
        err.user_message()
    );
}

#[test]
fn accepts_linear_regardless_of_gear_type_metadata_and_sets_disambiguation() {
    let sys = SystemSnapshot::capture();
    let bytes = minimal_linear_json(48_000, Some("Amp"));
    let (mut slot, meta) = build_cab_nam_from_bytes(
        &bytes,
        CabAssetKind::NamModel,
        Path::new("/presets/roomA/cab.nam"),
        48_000,
        128,
        true,
        &sys,
    )
    .expect("1x1 linear model tagged Amp must still load in cab slot");
    assert_eq!(meta.gear_type.as_deref(), Some("Amp"));
    assert_eq!(slot.latency_samples(), 0);
    assert_eq!(
        disambiguated_cab_label(Path::new("/presets/roomA/cab.nam")),
        "roomA/cab.nam"
    );

    let inp = [0.2f32, -0.4, 0.8, 0.0];
    let mut out_l = [0.0f32; 4];
    let mut out_r = [0.0f32; 4];
    slot.process_block(&inp, &inp, &mut out_l, &mut out_r, 4, false);
    assert!((out_l[0] - 0.1).abs() < 1e-6);
    assert!((out_r[1] + 0.2).abs() < 1e-6);
}

#[test]
fn ux_metadata_disambiguation_and_all_gear_tags_accepted() {
    let sys = SystemSnapshot::capture();
    for gear_tag in [Some("Amp"), Some("Preamp"), Some("Cabinet"), None] {
        let bytes = minimal_linear_json(48_000, gear_tag);
        let path = Path::new("/bank/suiteA/v30_cab.nam");
        let (slot, meta) = build_cab_nam_from_bytes(
            &bytes,
            CabAssetKind::NamModel,
            path,
            48_000,
            256,
            true,
            &sys,
        )
        .expect("any 1x1 architecture tag must load in cabinet slot");
        assert_eq!(meta.architecture, "Linear");
        assert_eq!(meta.gear_type.as_deref(), gear_tag);
        assert_eq!(slot.latency_samples(), 0);

        let ui = CabUiMeta::from_nam(path, &meta);
        assert_eq!(ui.disambiguated_name, "suiteA/v30_cab.nam");
        assert_eq!(ui.kind.badge_label(), "NAM");
    }

    assert_eq!(
        disambiguated_cab_label(Path::new("/pack1/dirA/cab.wav")),
        "dirA/cab.wav"
    );
    assert_eq!(
        disambiguated_cab_label(Path::new("/pack1/dirB/cab.wav")),
        "dirB/cab.wav"
    );
    assert_eq!(CabAssetKind::WavIr.badge_label(), "WAV");
    assert_eq!(CabAssetKind::NamModel.badge_label(), "NAM");
    assert_eq!(CabAssetKind::NambModel.badge_label(), "NAMB");
    assert_eq!(
        CabAssetKind::detect_from_path(Path::new("a.NAMB")),
        Some(CabAssetKind::NambModel)
    );
    assert_eq!(CabAssetKind::detect_from_path(Path::new("a.txt")), None);
}

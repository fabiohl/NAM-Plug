// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::clap::host_harness::{
    HostEvent, extract_plugin_main_thread, make_harness_audio_processor,
    make_test_plugin_with_harness,
};
use crate::clap::test_util::{self, make_default_params, model_path};
use clack_extensions::params::{ParamInfoBuffer, ParamInfoFlags, ParamRescanFlags, PluginParams};
use clack_extensions::state::PluginState;
use clack_host::process::PluginAudioConfiguration;
use clack_plugin::prelude::ClapId;
use neural_amp_modeler_rs::math::constants::{GAIN_MAX_DB, GAIN_MIN_DB};
use std::collections::HashSet;

/// T-D.1 (a): Após carga de modelo via load_model, o rescan pedido
/// pelo plugin ao host DEVE conter `VALUES | TEXT` (F-05).
#[test]
fn test_model_load_rescan_includes_values_and_text() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();

    let model_file = model_path("lstm.nam");
    assert!(model_file.exists(), "lstm.nam fixture must exist");

    // Activate the plugin so audio thread and command ring are live.
    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };
    let _stopped = instance
        .activate(
            {
                let state = state.clone();
                move |_, _| make_harness_audio_processor(&state)
            },
            audio_config,
        )
        .expect("activate failed");

    let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };
    mt.load_model(&model_file).expect("load_model failed");

    let rescan_events: Vec<_> = state
        .snapshot()
        .into_iter()
        .filter_map(|e| match e {
            HostEvent::ParamsRescan { flags } => Some(flags),
            _ => None,
        })
        .collect();

    assert!(
        !rescan_events.is_empty(),
        "Expected at least one ParamsRescan event after model load"
    );

    let expected_flags = ParamRescanFlags::VALUES | ParamRescanFlags::TEXT;
    for flags in &rescan_events {
        assert!(
            flags.contains(expected_flags),
            "F-05: ParamsRescan after load_model must contain VALUES | TEXT, got: {:?}",
            flags
        );
    }
}

/// T-D.1 (a): Após restauração de estado (PluginState::load), o rescan pedido
/// pelo plugin ao host DEVE conter `VALUES | TEXT` (F-05).
#[test]
fn test_state_restore_rescan_includes_values_and_text() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();

    let model_file = model_path("lstm.nam");
    assert!(model_file.exists(), "lstm.nam fixture must exist");

    let params = make_default_params(Some(model_file));
    let state_bytes = serde_json::to_vec(&params).expect("Failed to serialize params");

    let state_ext = instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("PluginState extension not found");
    let handle = instance.plugin_handle();

    state_ext
        .load(&handle, &mut state_bytes.as_slice())
        .expect("PluginState::load failed");

    let rescan_events: Vec<_> = state
        .snapshot()
        .into_iter()
        .filter_map(|e| match e {
            HostEvent::ParamsRescan { flags } => Some(flags),
            _ => None,
        })
        .collect();

    assert!(
        !rescan_events.is_empty(),
        "Expected at least one ParamsRescan event after state restore"
    );

    let expected_flags = ParamRescanFlags::VALUES | ParamRescanFlags::TEXT;
    for flags in &rescan_events {
        assert!(
            flags.contains(expected_flags),
            "F-05: ParamsRescan after state restore must contain VALUES | TEXT, got: {:?}",
            flags
        );
    }
}

/// T-D.1 (b): Os 4 parâmetros discretos (`Adaptive Compute`, `Slim Override`,
/// `Oversampling`, `Activation Precision`) DEVEM ter `IS_ENUM` e `IS_STEPPED`,
/// e `value_to_text()` não pode ser vazio nem duplicado em todo o domínio [min, max] (F-04).
#[test]
fn test_four_enum_parameters_flags_and_distinct_text_in_domain() {
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let params_ext = instance
        .plugin_handle()
        .get_extension::<PluginParams>()
        .expect("PluginParams extension not found");
    let handle = instance.plugin_handle();

    let target_params = [
        (PARAM_ADAPTIVE_COMPUTE, "Adaptive Compute", 0.0, 2.0),
        (PARAM_SLIM_OVERRIDE, "Slim Override", 0.0, 2.0),
        (PARAM_OVERSAMPLE, "Oversampling", 0.0, 2.0),
        (PARAM_ACTIVATION, "Activation Precision", 0.0, 1.0),
    ];

    let mut info_buf = ParamInfoBuffer::new();

    for (id, name, expected_min, expected_max) in target_params {
        let info = params_ext
            .get_info(&handle, id, &mut info_buf)
            .unwrap_or_else(|| panic!("get_info failed for {name} ({id})"));

        assert_eq!(info.id.get(), id, "{name} id mismatch");
        assert_eq!(info.min_value, expected_min, "{name} min_value mismatch");
        assert_eq!(info.max_value, expected_max, "{name} max_value mismatch");

        // F-04: Requirement of IS_STEPPED and IS_ENUM
        assert!(
            info.flags.contains(ParamInfoFlags::IS_STEPPED),
            "F-04: {name} must have IS_STEPPED"
        );
        assert!(
            info.flags.contains(ParamInfoFlags::IS_ENUM),
            "F-04: {name} must have IS_ENUM"
        );

        // Verify value_to_text is non-empty and distinct across every discrete step in [min, max]
        let mut seen_texts = HashSet::new();
        let min_step = info.min_value as i32;
        let max_step = info.max_value as i32;

        for step in min_step..=max_step {
            let mut text_buf = [0u8; 128];
            let res = params_ext
                .value_to_text(&handle, ClapId::new(id), step as f64, &mut text_buf)
                .unwrap_or_else(|_| panic!("value_to_text failed for {name} step {step}"));
            let text = std::str::from_utf8(res)
                .expect("value_to_text must return valid UTF-8")
                .trim_end_matches('\0');

            assert!(
                !text.is_empty(),
                "F-04: {name} text for step {step} must not be empty"
            );
            assert!(
                seen_texts.insert(text.to_string()),
                "F-04: {name} text '{text}' for step {step} is not distinct (seen: {seen_texts:?})"
            );
        }
    }
}

/// T-D.1 (c): Não-regressão de parâmetros — snapshot completo dos 9 parâmetros (IDs 0–8),
/// `count() == 9`, min/max/default e `get_value` inalterados.
/// `Active Model` (ID 4) continua estritamente `IS_READONLY` (sem `IS_ENUM`, sem `IS_STEPPED`).
#[test]
fn test_parameters_non_regression_snapshot() {
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let params_ext = instance
        .plugin_handle()
        .get_extension::<PluginParams>()
        .expect("PluginParams extension not found");
    let handle = instance.plugin_handle();

    assert_eq!(
        params_ext.count(&handle),
        9,
        "Total parameter count must remain 9"
    );

    struct ParamExpectation {
        id: u32,
        name: &'static [u8],
        min: f64,
        max: f64,
        default: f64,
        initial_value: f64,
        required_flags: ParamInfoFlags,
        forbidden_flags: ParamInfoFlags,
    }

    let expectations = [
        ParamExpectation {
            id: PARAM_INPUT_GAIN,
            name: b"Input Gain",
            min: GAIN_MIN_DB as f64,
            max: GAIN_MAX_DB as f64,
            default: 0.0,
            initial_value: 0.0,
            required_flags: ParamInfoFlags::IS_AUTOMATABLE | ParamInfoFlags::IS_MODULATABLE,
            forbidden_flags: ParamInfoFlags::IS_STEPPED
                | ParamInfoFlags::IS_READONLY
                | ParamInfoFlags::IS_ENUM,
        },
        ParamExpectation {
            id: PARAM_OUTPUT_GAIN,
            name: b"Output Gain",
            min: GAIN_MIN_DB as f64,
            max: GAIN_MAX_DB as f64,
            default: 0.0,
            initial_value: 0.0,
            required_flags: ParamInfoFlags::IS_AUTOMATABLE | ParamInfoFlags::IS_MODULATABLE,
            forbidden_flags: ParamInfoFlags::IS_STEPPED
                | ParamInfoFlags::IS_READONLY
                | ParamInfoFlags::IS_ENUM,
        },
        ParamExpectation {
            id: PARAM_GATE_THRESH,
            name: b"Gate Threshold",
            min: -90.0,
            max: 0.0,
            default: -90.0,
            initial_value: -90.0,
            required_flags: ParamInfoFlags::IS_AUTOMATABLE | ParamInfoFlags::IS_MODULATABLE,
            forbidden_flags: ParamInfoFlags::IS_STEPPED
                | ParamInfoFlags::IS_READONLY
                | ParamInfoFlags::IS_ENUM,
        },
        ParamExpectation {
            id: PARAM_BYPASS,
            name: b"Bypass",
            min: 0.0,
            max: 1.0,
            default: 0.0,
            initial_value: 0.0,
            required_flags: ParamInfoFlags::IS_AUTOMATABLE
                | ParamInfoFlags::IS_STEPPED
                | ParamInfoFlags::IS_BYPASS,
            forbidden_flags: ParamInfoFlags::IS_READONLY | ParamInfoFlags::IS_ENUM,
        },
        ParamExpectation {
            id: PARAM_ACTIVE_MODEL,
            name: b"Active Model",
            min: 0.0,
            max: 1000.0,
            default: 0.0,
            initial_value: 0.0,
            required_flags: ParamInfoFlags::IS_READONLY,
            // Active Model is readonly: must NOT be stepped, enum, or automatable
            forbidden_flags: ParamInfoFlags::IS_AUTOMATABLE
                | ParamInfoFlags::IS_STEPPED
                | ParamInfoFlags::IS_ENUM,
        },
        ParamExpectation {
            id: PARAM_ADAPTIVE_COMPUTE,
            name: b"Adaptive Compute",
            min: 0.0,
            max: 2.0,
            default: 1.0,
            initial_value: 1.0,
            required_flags: ParamInfoFlags::IS_AUTOMATABLE
                | ParamInfoFlags::IS_STEPPED
                | ParamInfoFlags::IS_ENUM,
            forbidden_flags: ParamInfoFlags::IS_READONLY | ParamInfoFlags::IS_BYPASS,
        },
        ParamExpectation {
            id: PARAM_SLIM_OVERRIDE,
            name: b"Slim Override",
            min: 0.0,
            max: 2.0,
            default: 0.0,
            initial_value: 0.0,
            required_flags: ParamInfoFlags::IS_AUTOMATABLE
                | ParamInfoFlags::IS_STEPPED
                | ParamInfoFlags::IS_ENUM,
            forbidden_flags: ParamInfoFlags::IS_READONLY | ParamInfoFlags::IS_BYPASS,
        },
        ParamExpectation {
            id: PARAM_OVERSAMPLE,
            name: b"Oversampling",
            min: 0.0,
            max: 2.0,
            default: 0.0,
            initial_value: 0.0,
            required_flags: ParamInfoFlags::IS_AUTOMATABLE
                | ParamInfoFlags::IS_STEPPED
                | ParamInfoFlags::IS_ENUM,
            forbidden_flags: ParamInfoFlags::IS_READONLY | ParamInfoFlags::IS_BYPASS,
        },
        ParamExpectation {
            id: PARAM_ACTIVATION,
            name: b"Activation Precision",
            min: 0.0,
            max: 1.0,
            default: 1.0,
            initial_value: 1.0,
            required_flags: ParamInfoFlags::IS_AUTOMATABLE
                | ParamInfoFlags::IS_STEPPED
                | ParamInfoFlags::IS_ENUM,
            forbidden_flags: ParamInfoFlags::IS_READONLY | ParamInfoFlags::IS_BYPASS,
        },
    ];

    let mut info_buf = ParamInfoBuffer::new();

    for exp in expectations {
        let info = params_ext
            .get_info(&handle, exp.id, &mut info_buf)
            .unwrap_or_else(|| panic!("get_info failed for id {}", exp.id));

        assert_eq!(info.id.get(), exp.id, "ID mismatch");
        assert_eq!(info.name, exp.name, "Name mismatch for ID {}", exp.id);
        assert_eq!(info.min_value, exp.min, "Min mismatch for ID {}", exp.id);
        assert_eq!(info.max_value, exp.max, "Max mismatch for ID {}", exp.id);
        assert_eq!(
            info.default_value, exp.default,
            "Default mismatch for ID {}",
            exp.id
        );

        assert!(
            info.flags.contains(exp.required_flags),
            "Flags missing required for ID {}: expected {:?}",
            exp.id,
            exp.required_flags
        );
        assert!(
            !info.flags.intersects(exp.forbidden_flags),
            "Flags contain forbidden for ID {}: forbidden {:?}",
            exp.id,
            exp.forbidden_flags
        );

        let val = params_ext
            .get_value(&handle, ClapId::new(exp.id))
            .unwrap_or_else(|| panic!("get_value failed for id {}", exp.id));
        assert_eq!(
            val, exp.initial_value,
            "Initial get_value mismatch for ID {}",
            exp.id
        );
    }
}

#[test]
fn test_gate_threshold_contract_alignment() {
    // S02 / T02.1: Contract alignment for PARAM_GATE_THRESH to [-90.0, 0.0] dB.
    // 1. Min/Max boundaries
    assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, -90.0), -90.0);
    assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, 0.0), 0.0);

    // 2. Out-of-bounds values clamp strictly to range
    assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, -90.1), -90.0);
    assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, -150.0), -90.0);
    assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, 0.1), 0.0);
    assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, 50.0), 0.0);

    // 3. Previously clamped range [-40.0, 0.0] is now preserved without truncation
    assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, -40.0), -40.0);
    assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, -30.0), -30.0);
    assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, -15.5), -15.5);
    assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, -6.0), -6.0);

    // 4. Non-finites fall back to the safe default (-90.0 dB, gate parked/off)
    assert_eq!(sanitize_param_value(PARAM_GATE_THRESH, f32::NAN), -90.0);
    assert_eq!(
        sanitize_param_value(PARAM_GATE_THRESH, f32::INFINITY),
        -90.0
    );
    assert_eq!(
        sanitize_param_value(PARAM_GATE_THRESH, f32::NEG_INFINITY),
        -90.0
    );

    // 5. Double precision f64 variant parity
    assert_eq!(sanitize_param_value_f64(PARAM_GATE_THRESH, -90.0), -90.0);
    assert_eq!(sanitize_param_value_f64(PARAM_GATE_THRESH, 0.0), 0.0);
    assert_eq!(sanitize_param_value_f64(PARAM_GATE_THRESH, 10.0), 0.0);
    assert_eq!(sanitize_param_value_f64(PARAM_GATE_THRESH, f64::NAN), -90.0);
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # Headless Tests for GUI Alert Status Pill (Sprint R3 / T-R3.2)
//!
//! Validates that `SlintViewModel` reflects RT status flags in `MainWindow`:
//! - Priority ordering across all 7 alert conditions.
//! - Correct alert text and severity color (Red vs. Amber).
//! - Clean deactivation when all alert flags are cleared.

use std::sync::Arc;

use nam_plug::clap::gui::{MainWindow, SlintViewModel};
use nam_plug::clap::plugin::GuiSharedState;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_CABSIM_CONTRACT_VIOLATION, RT_STATUS_DEGRADE_MINIMAL, RT_STATUS_DEGRADE_REDUCED,
    RT_STATUS_HOST_CONTRACT_VIOLATION, RT_STATUS_NON_FINITE_INPUT_DETECTED,
    RT_STATUS_PROCESSOR_POISONED, RT_STATUS_RESAMPLER_REBUILD_FAILED,
};
use slint::ComponentHandle;

#[test]
fn test_slint_alert_pill_priorities_and_deactivation() {
    let shared = GuiSharedState::new_test();
    let window = MainWindow::new().expect("Failed to create MainWindow");
    let mut vm = SlintViewModel::new(window.clone_strong(), Arc::clone(&shared), None, "Test");

    let red = slint::Color::from_argb_encoded(0xFF_F7_4E_4E);
    let amber = slint::Color::from_argb_encoded(0xFF_EB_CB_8B);

    // Initial state: no alerts active
    vm.tick_telemetry();
    assert!(
        !window.get_alert_active(),
        "Alert pill must be inactive initially"
    );
    assert_eq!(window.get_alert_text(), "");

    // 1. NON_FINITE_INPUT
    shared
        .cold
        .rt_status
        .set_flag(RT_STATUS_NON_FINITE_INPUT_DETECTED);
    vm.tick_telemetry();
    assert!(window.get_alert_active());
    assert_eq!(
        window.get_alert_text(),
        "Entrada inválida (NaN) — reconvergindo"
    );
    assert_eq!(window.get_alert_color(), amber);

    // 2. DEGRADE_REDUCED (preempts NON_FINITE_INPUT)
    shared.cold.rt_status.set_flag(RT_STATUS_DEGRADE_REDUCED);
    vm.tick_telemetry();
    assert!(window.get_alert_active());
    assert_eq!(window.get_alert_text(), "Qualidade reduzida: CPU alta");
    assert_eq!(window.get_alert_color(), amber);

    // 3. DEGRADE_MINIMAL (preempts DEGRADE_REDUCED)
    shared.cold.rt_status.set_flag(RT_STATUS_DEGRADE_MINIMAL);
    vm.tick_telemetry();
    assert!(window.get_alert_active());
    assert_eq!(window.get_alert_text(), "Qualidade mínima: CPU alta");
    assert_eq!(window.get_alert_color(), amber);

    // 4. CABSIM_CONTRACT (preempts DEGRADE_MINIMAL, red)
    shared
        .cold
        .rt_status
        .set_flag(RT_STATUS_CABSIM_CONTRACT_VIOLATION);
    vm.tick_telemetry();
    assert!(window.get_alert_active());
    assert_eq!(window.get_alert_text(), "Violação de partição do CabSim");
    assert_eq!(window.get_alert_color(), red);

    // 5. RESAMPLER_FAILED (preempts CABSIM_CONTRACT, red)
    shared
        .cold
        .rt_status
        .set_flag(RT_STATUS_RESAMPLER_REBUILD_FAILED);
    vm.tick_telemetry();
    assert!(window.get_alert_active());
    assert_eq!(window.get_alert_text(), "Falha ao reconstruir resampler");
    assert_eq!(window.get_alert_color(), red);

    // 6. HOST_CONTRACT (preempts RESAMPLER_FAILED, red)
    shared
        .cold
        .rt_status
        .set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
    vm.tick_telemetry();
    assert!(window.get_alert_active());
    assert_eq!(window.get_alert_text(), "Bloco do host acima do limite");
    assert_eq!(window.get_alert_color(), red);

    // 7. POISONED (highest priority, red)
    shared.cold.rt_status.set_flag(RT_STATUS_PROCESSOR_POISONED);
    vm.tick_telemetry();
    assert!(window.get_alert_active());
    assert_eq!(window.get_alert_text(), "Plugin parado — reiniciando…");
    assert_eq!(window.get_alert_color(), red);

    // 8. Deactivation: clear all flags
    shared
        .cold
        .rt_status
        .clear_flag(RT_STATUS_PROCESSOR_POISONED);
    shared
        .cold
        .rt_status
        .clear_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
    shared
        .cold
        .rt_status
        .clear_flag(RT_STATUS_RESAMPLER_REBUILD_FAILED);
    shared
        .cold
        .rt_status
        .clear_flag(RT_STATUS_CABSIM_CONTRACT_VIOLATION);
    shared.cold.rt_status.clear_flag(RT_STATUS_DEGRADE_MINIMAL);
    shared.cold.rt_status.clear_flag(RT_STATUS_DEGRADE_REDUCED);
    shared
        .cold
        .rt_status
        .clear_flag(RT_STATUS_NON_FINITE_INPUT_DETECTED);

    vm.tick_telemetry();
    assert!(
        !window.get_alert_active(),
        "Alert pill must deactivate when all flags are cleared"
    );
    assert_eq!(
        window.get_alert_text(),
        "",
        "Alert text must be cleared when inactive"
    );
}

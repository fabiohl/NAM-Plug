// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLAP-specific calibration configuration layered on the shared engine math.
//!
//! The dBu bounds, [`OutputMode`] semantics, [`ModelCalibrationMeta`] snapshot
//! and [`compute_calibration_gains`] equations live in
//! `neural_amp_modeler_rs::dsp::calibration`. This module only adds the host
//! parameter/state adapter ([`CalibrationConfig`]) and keeps the historical
//! [`compute_calibration_multipliers`] tuple API used across the processor.

use serde::{Deserialize, Serialize};

pub use neural_amp_modeler_rs::dsp::calibration::{
    CalibrationGains, DEFAULT_CALIBRATION_DBU, MAX_CALIBRATION_DBU, MIN_CALIBRATION_DBU,
    ModelCalibrationMeta, OutputMode, TARGET_LOUDNESS_DB, compute_calibration_gains,
    sanitize_calibration_level_dbu,
};

/// Default input calibration reference level in dBu.
pub use neural_amp_modeler_rs::dsp::calibration::DEFAULT_CALIBRATION_DBU as INPUT_CALIBRATION_LEVEL_DEFAULT_DBU;
/// Maximum allowed input calibration reference level in dBu.
pub use neural_amp_modeler_rs::dsp::calibration::MAX_CALIBRATION_DBU as INPUT_CALIBRATION_LEVEL_MAX_DBU;
/// Minimum allowed input calibration reference level in dBu.
pub use neural_amp_modeler_rs::dsp::calibration::MIN_CALIBRATION_DBU as INPUT_CALIBRATION_LEVEL_MIN_DBU;

/// Serde adapter preserving the historical `snake_case` string form of
/// [`OutputMode`] in CLAP state (`"raw"`, `"normalized"`, `"calibrated"`).
mod output_mode_serde {
    use super::OutputMode;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(mode: &OutputMode, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match mode {
            OutputMode::Raw => "raw",
            OutputMode::Normalized => "normalized",
            OutputMode::Calibrated => "calibrated",
        })
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<OutputMode, D::Error> {
        let text = String::deserialize(deserializer)?;
        OutputMode::from_text(&text)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown output mode: {text:?}")))
    }
}

/// User-facing calibration configuration stored in CLAP state and parameters.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CalibrationConfig {
    /// Opt-in flag enabling input level calibration prior to neural inference.
    pub calibrate_input: bool,
    /// Host converter 0 dBFS reference level in dBu (`[-60.0, 60.0]`).
    pub input_calibration_level_dbu: f32,
    /// Model output compensation policy.
    #[serde(with = "output_mode_serde")]
    pub output_mode: OutputMode,
}

impl Default for CalibrationConfig {
    /// Returns explicit defaults for newly created instances (`calibrate_input = false`,
    /// `input_calibration_level_dbu = 12.0`, `output_mode = OutputMode::Normalized`).
    #[inline]
    fn default() -> Self {
        Self::new_project_default()
    }
}

impl CalibrationConfig {
    /// Defaults for newly instantiated plugins and v2 projects.
    #[inline]
    pub const fn new_project_default() -> Self {
        Self {
            calibrate_input: false,
            input_calibration_level_dbu: INPUT_CALIBRATION_LEVEL_DEFAULT_DBU,
            output_mode: OutputMode::Normalized,
        }
    }

    /// Defaults restored when loading legacy v0/v1 states that lack explicit
    /// calibration fields, preserving pre-parameterized session sound.
    #[inline]
    pub const fn legacy_restored_default() -> Self {
        Self {
            calibrate_input: true,
            input_calibration_level_dbu: INPUT_CALIBRATION_LEVEL_DEFAULT_DBU,
            output_mode: OutputMode::Normalized,
        }
    }

    /// Returns a copy with `input_calibration_level_dbu` guaranteed finite and clamped
    /// to `[INPUT_CALIBRATION_LEVEL_MIN_DBU, INPUT_CALIBRATION_LEVEL_MAX_DBU]`.
    #[inline]
    pub fn sanitized(self) -> Self {
        Self {
            calibrate_input: self.calibrate_input,
            input_calibration_level_dbu: sanitize_calibration_level_dbu(
                self.input_calibration_level_dbu,
            ),
            output_mode: self.output_mode,
        }
    }

    /// Validates serialized calibration fields strictly (rejecting non-finite or out-of-range dBu).
    pub fn validate(self) -> Result<Self, String> {
        if !self.input_calibration_level_dbu.is_finite() {
            return Err("calibration.input_calibration_level_dbu must be finite".to_string());
        }
        if !(INPUT_CALIBRATION_LEVEL_MIN_DBU..=INPUT_CALIBRATION_LEVEL_MAX_DBU)
            .contains(&self.input_calibration_level_dbu)
        {
            return Err(format!(
                "calibration.input_calibration_level_dbu ({}) out of range [{}, {}]",
                self.input_calibration_level_dbu,
                INPUT_CALIBRATION_LEVEL_MIN_DBU,
                INPUT_CALIBRATION_LEVEL_MAX_DBU
            ));
        }
        Ok(self)
    }
}

/// Computes `(input_mult, output_mult)` linear multipliers using the shared
/// engine equations.
#[inline]
pub fn compute_calibration_multipliers(
    meta: ModelCalibrationMeta,
    cfg: CalibrationConfig,
) -> (f32, f32) {
    let cfg = cfg.sanitized();
    let gains = compute_calibration_gains(
        meta,
        cfg.calibrate_input,
        cfg.input_calibration_level_dbu,
        cfg.output_mode,
    );
    (gains.input_mult, gains.output_mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lstm_fixture_meta() -> ModelCalibrationMeta {
        ModelCalibrationMeta {
            has_model: true,
            input_level_dbu: Some(18.3),
            output_level_dbu: Some(12.3),
            loudness: Some(-37.840_687),
        }
    }

    #[test]
    fn equations_match_cpp_plugin_for_lstm_fixture() {
        let meta = lstm_fixture_meta();

        let (in_def, out_def) =
            compute_calibration_multipliers(meta, CalibrationConfig::new_project_default());
        assert!((in_def - 1.0).abs() < 1e-7);
        let expected_norm = 10.0_f64.powf((-18.0 - (-37.840_687_f64)) / 20.0) as f32;
        assert!((out_def - expected_norm).abs() < 1e-5);

        let cfg_raw = CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: 12.0,
            output_mode: OutputMode::Raw,
        };
        let (in_raw, out_raw) = compute_calibration_multipliers(meta, cfg_raw);
        let expected_in = 10.0_f64.powf((-6.3_f64) / 20.0) as f32;
        assert!((in_raw - expected_in).abs() < 1e-6);
        assert!((out_raw - 1.0).abs() < 1e-7);

        let cfg_cal = CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: 12.0,
            output_mode: OutputMode::Calibrated,
        };
        let (in_cal, out_cal) = compute_calibration_multipliers(meta, cfg_cal);
        let expected_cal_out = 10.0_f64.powf(0.3_f64 / 20.0) as f32;
        assert!((in_cal - expected_in).abs() < 1e-6);
        assert!((out_cal - expected_cal_out).abs() < 1e-6);
    }

    #[test]
    fn missing_metadata_never_fabricates_dbu() {
        let empty_meta = ModelCalibrationMeta {
            has_model: true,
            input_level_dbu: None,
            output_level_dbu: None,
            loudness: None,
        };
        for mode in [
            OutputMode::Raw,
            OutputMode::Normalized,
            OutputMode::Calibrated,
        ] {
            let (in_m, out_m) = compute_calibration_multipliers(
                empty_meta,
                CalibrationConfig {
                    calibrate_input: true,
                    input_calibration_level_dbu: 24.0,
                    output_mode: mode,
                },
            );
            assert_eq!(in_m, 1.0);
            assert_eq!(out_m, 1.0);
        }
    }

    #[test]
    fn sanitization_and_extreme_bounds_stay_finite_without_30db_lut_clip() {
        let meta = ModelCalibrationMeta {
            has_model: true,
            input_level_dbu: Some(0.0),
            output_level_dbu: Some(50.0),
            loudness: None,
        };
        let cfg = CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: 45.0,
            output_mode: OutputMode::Calibrated,
        };
        let (in_m, out_m) = compute_calibration_multipliers(meta, cfg);
        let expected_in = 10.0_f64.powf(45.0 / 20.0) as f32;
        let expected_out = 10.0_f64.powf(5.0 / 20.0) as f32;
        assert!((in_m - expected_in).abs() / expected_in < 1e-6);
        assert!((out_m - expected_out).abs() < 1e-6);

        assert_eq!(sanitize_calibration_level_dbu(f32::NAN), 12.0);
        assert_eq!(sanitize_calibration_level_dbu(f32::INFINITY), 12.0);
        assert_eq!(sanitize_calibration_level_dbu(99.0), 60.0);
        assert_eq!(sanitize_calibration_level_dbu(-99.0), -60.0);
    }

    #[test]
    fn output_mode_state_roundtrip_keeps_snake_case_contract() {
        let cfg = CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: 12.0,
            output_mode: OutputMode::Calibrated,
        };
        let json = serde_json::to_string(&cfg).expect("serialize calibration config");
        assert!(json.contains("\"output_mode\":\"calibrated\""), "{json}");
        let restored: CalibrationConfig =
            serde_json::from_str(&json).expect("deserialize calibration config");
        assert_eq!(restored, cfg);
    }
}

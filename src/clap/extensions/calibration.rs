// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Amp model input calibration and output level mode semantics.
//!
//! Aligns gain equations and bounds with C++ `NeuralAmpModelerPlugin`:
//! - `Calibrate Input` (opt-in boolean toggle; new-project default `false`,
//!   legacy pre-v2 state migration `true` to preserve saved session audio)
//! - `Input Calibration Level` (`[-60.0, 60.0]` dBu reference, default `12.0` dBu)
//! - `Output Mode` (`Raw`, `Normalized` (default), `Calibrated`)
//!
//! When a loaded model omits `input_level_dbu`, `output_level_dbu`, or `loudness`,
//! the corresponding compensation contributes `0.0 dB` (`1.0x`) without fabricating
//! missing metadata, and the UI surfaces unavailability.

use neural_amp_modeler_rs::loader::LoadedModelPair;
use serde::{Deserialize, Serialize};

/// Minimum allowed input calibration reference level in dBu.
pub const INPUT_CALIBRATION_LEVEL_MIN_DBU: f32 = -60.0;
/// Maximum allowed input calibration reference level in dBu.
pub const INPUT_CALIBRATION_LEVEL_MAX_DBU: f32 = 60.0;
/// Default input calibration reference level in dBu.
pub const INPUT_CALIBRATION_LEVEL_DEFAULT_DBU: f32 = 12.0;
/// Target normalized loudness in LUFS/dB.
pub const TARGET_LOUDNESS_DB: f32 = -18.0;

/// Output gain compensation mode for neural amp models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum OutputMode {
    /// Unity model output (`0.0 dB` compensation).
    Raw = 0,
    /// Normalize output loudness to `-18.0 dB` using model `loudness` metadata.
    #[default]
    Normalized = 1,
    /// Scale output to match `input_calibration_level_dbu` using model `output_level_dbu`.
    Calibrated = 2,
}

impl OutputMode {
    /// Converts a discrete numeric index into [`OutputMode`].
    #[inline]
    pub const fn from_u32(val: u32) -> Self {
        match val {
            0 => Self::Raw,
            2 => Self::Calibrated,
            _ => Self::Normalized,
        }
    }

    /// Converts a CLAP parameter float value into [`OutputMode`].
    #[inline]
    pub fn from_f32(val: f32) -> Self {
        if !val.is_finite() {
            return Self::Normalized;
        }
        Self::from_u32(val.round().clamp(0.0, 2.0) as u32)
    }

    /// Converts [`OutputMode`] into its discrete numeric value.
    #[inline]
    pub const fn to_u32(self) -> u32 {
        self as u32
    }

    /// Converts [`OutputMode`] into its CLAP float value.
    #[inline]
    pub const fn to_f32(self) -> f32 {
        (self as u32) as f32
    }

    /// Full display label used in CLAP parameter text and tooltips.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "Raw",
            Self::Normalized => "Normalized",
            Self::Calibrated => "Calibrated",
        }
    }

    /// Parses text entered by a host or user.
    pub fn from_text(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "raw" | "0" => Some(Self::Raw),
            "normalized" | "norm" | "1" => Some(Self::Normalized),
            "calibrated" | "cal" | "2" => Some(Self::Calibrated),
            _ => None,
        }
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

/// RT-safe snapshot of amp model calibration metadata (`Copy`, zero allocations).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ModelCalibrationMeta {
    /// True only when an amp model is loaded (never set for CabSim IRs alone).
    pub has_model: bool,
    /// Model capture input reference level in dBu, when present and valid.
    pub input_level_dbu: Option<f32>,
    /// Model capture output reference level in dBu, when present and valid.
    pub output_level_dbu: Option<f32>,
    /// Measured model loudness in dB, when present and finite.
    pub loudness: Option<f32>,
}

impl ModelCalibrationMeta {
    /// Extracts validated calibration metadata from a loaded neural amp model pair.
    pub fn from_loaded_model(pair: &LoadedModelPair) -> Self {
        let input_level_dbu = pair
            .has_input_level_dbu()
            .then(|| pair.input_level_dbu())
            .flatten()
            .filter(|v| is_valid_dbu(*v));
        let output_level_dbu = pair
            .has_output_level_dbu()
            .then(|| pair.output_level_dbu())
            .flatten()
            .filter(|v| is_valid_dbu(*v));
        let loudness = pair
            .has_loudness()
            .then(|| pair.loudness())
            .flatten()
            .filter(|v| v.is_finite());
        Self {
            has_model: true,
            input_level_dbu,
            output_level_dbu,
            loudness,
        }
    }

    /// Empty metadata when no amp model is active.
    #[inline]
    pub const fn empty() -> Self {
        Self {
            has_model: false,
            input_level_dbu: None,
            output_level_dbu: None,
            loudness: None,
        }
    }
}

/// Validates and clamps an input calibration dBu level.
#[inline]
pub fn sanitize_calibration_level_dbu(val: f32) -> f32 {
    if !val.is_finite() {
        return INPUT_CALIBRATION_LEVEL_DEFAULT_DBU;
    }
    val.clamp(
        INPUT_CALIBRATION_LEVEL_MIN_DBU,
        INPUT_CALIBRATION_LEVEL_MAX_DBU,
    )
}

#[inline]
fn is_valid_dbu(val: f32) -> bool {
    val.is_finite()
        && (INPUT_CALIBRATION_LEVEL_MIN_DBU..=INPUT_CALIBRATION_LEVEL_MAX_DBU).contains(&val)
}

/// Computes `(input_mult, output_mult)` linear multipliers in dB-to-linear `f64`
/// precision without clamping valid calibration deltas at user gain LUT bounds.
#[inline]
pub fn compute_calibration_multipliers(
    meta: ModelCalibrationMeta,
    cfg: CalibrationConfig,
) -> (f32, f32) {
    if !meta.has_model {
        return (1.0, 1.0);
    }

    let cfg = cfg.sanitized();
    let input_db = if cfg.calibrate_input {
        match meta.input_level_dbu {
            Some(model_in_dbu) => cfg.input_calibration_level_dbu - model_in_dbu,
            None => 0.0,
        }
    } else {
        0.0
    };

    let output_db = match cfg.output_mode {
        OutputMode::Raw => 0.0,
        OutputMode::Normalized => match meta.loudness {
            Some(loudness_db) => TARGET_LOUDNESS_DB - loudness_db,
            None => 0.0,
        },
        OutputMode::Calibrated => match meta.output_level_dbu {
            Some(model_out_dbu) => model_out_dbu - cfg.input_calibration_level_dbu,
            None => 0.0,
        },
    };

    (db_to_linear_exact(input_db), db_to_linear_exact(output_db))
}

#[inline]
fn db_to_linear_exact(db: f32) -> f32 {
    if db == 0.0 || !db.is_finite() {
        return 1.0;
    }
    let linear = 10.0_f64.powf(f64::from(db) / 20.0) as f32;
    if linear.is_finite() && linear > 0.0 {
        linear
    } else {
        1.0
    }
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

        // Default new-project configuration: calibrate_input=false, 12.0 dBu, Normalized
        let (in_def, out_def) =
            compute_calibration_multipliers(meta, CalibrationConfig::new_project_default());
        assert!((in_def - 1.0).abs() < 1e-7);
        let expected_norm = 10.0_f64.powf((-18.0 - (-37.840_687_f64)) / 20.0) as f32;
        assert!((out_def - expected_norm).abs() < 1e-5);

        // Opt-in calibrated input at 12.0 dBu -> 12.0 - 18.3 = -6.3 dB
        // Raw output -> 0.0 dB
        let cfg_raw = CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: 12.0,
            output_mode: OutputMode::Raw,
        };
        let (in_raw, out_raw) = compute_calibration_multipliers(meta, cfg_raw);
        let expected_in = 10.0_f64.powf((-6.3_f64) / 20.0) as f32;
        assert!((in_raw - expected_in).abs() < 1e-6);
        assert!((out_raw - 1.0).abs() < 1e-7);

        // Calibrated output at 12.0 dBu -> 12.3 - 12.0 = +0.3 dB
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
}

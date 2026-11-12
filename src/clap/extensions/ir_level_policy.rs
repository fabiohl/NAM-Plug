// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Explicit Cabinet IR level policy and convolution kernel builder.
//!
//! Decouples raw WAV sample storage (`ir_raw_samples` at native `original_rate`)
//! from the level policy applied when building the UPOLS [`CabSimPair`].
//! All operations here are strictly off-RT.

use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode, SystemSnapshot};
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Level policy applied to a cabinet impulse response before convolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IrLevelPolicy {
    /// Normalize the IR kernel so its absolute peak is 1.0 (0 dBFS).
    /// Default for backwards compatibility with existing sessions.
    #[default]
    PeakNormalized,
    /// Preserve the original WAV amplitude (1:1 gain, no peak normalization).
    Preserved,
    /// Apply the legacy C++ NeuralAmpModelerPlugin level scaling:
    /// `10^(-18/20) * (48000 / target_rate)` on the unnormalized IR.
    /// Note: preserves Rust polyphase sinc resampling and full IR length.
    LegacyNamPlugin,
}

impl IrLevelPolicy {
    /// Stable kebab-case identifier used in state serialization and logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PeakNormalized => "peak-normalized",
            Self::Preserved => "preserved",
            Self::LegacyNamPlugin => "legacy-nam-plugin",
        }
    }

    /// Compact label for the GUI badge/selector.
    pub const fn display_label(self) -> &'static str {
        match self {
            Self::PeakNormalized => "Peak",
            Self::Preserved => "Preserved",
            Self::LegacyNamPlugin => "Legacy",
        }
    }

    /// Cycles to the next policy in UI order.
    pub const fn next(self) -> Self {
        match self {
            Self::PeakNormalized => Self::Preserved,
            Self::Preserved => Self::LegacyNamPlugin,
            Self::LegacyNamPlugin => Self::PeakNormalized,
        }
    }

    /// Encodes the policy as `u8` for lock-free atomic storage.
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::PeakNormalized => 0,
            Self::Preserved => 1,
            Self::LegacyNamPlugin => 2,
        }
    }

    /// Decodes an atomic `u8` back to [`IrLevelPolicy`], defaulting to `PeakNormalized`.
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Preserved,
            2 => Self::LegacyNamPlugin,
            _ => Self::PeakNormalized,
        }
    }
}

/// Applies `policy` in-place to `samples` at `target_rate` Hz.
///
/// Returns the scalar gain factor applied (1.0 when unchanged or silent).
pub(crate) fn apply_ir_level_policy(
    samples: &mut [f32],
    policy: IrLevelPolicy,
    target_rate: u32,
) -> f32 {
    match policy {
        IrLevelPolicy::PeakNormalized => {
            let peak =
                samples.iter().fold(
                    0.0f32,
                    |acc, &s| {
                        if s.is_finite() { acc.max(s.abs()) } else { acc }
                    },
                );
            if peak <= 0.0 || (peak - 1.0).abs() < f32::EPSILON {
                return 1.0;
            }
            let gain = 1.0 / peak;
            for s in samples.iter_mut() {
                *s *= gain;
            }
            gain
        }
        IrLevelPolicy::Preserved => 1.0,
        IrLevelPolicy::LegacyNamPlugin => {
            let rate = f64::from(target_rate.max(1));
            let gain = (10.0_f64.powf(-18.0 / 20.0) * (48_000.0 / rate)) as f32;
            for s in samples.iter_mut() {
                *s *= gain;
            }
            gain
        }
    }
}

/// Resamples (when `original_rate != host_rate`), applies `policy`, and builds
/// a stereo-decoupled [`CabSimPair`] off-RT.
pub(crate) fn build_cabsim_pair_from_raw(
    raw_samples: &[f32],
    original_rate: u32,
    host_rate: u32,
    partition_size: usize,
    policy: IrLevelPolicy,
    sys: &SystemSnapshot,
) -> Result<CabSimPair, Box<NamDiagnostic>> {
    let resolved: Cow<'_, [f32]> = if original_rate > 0 && original_rate != host_rate {
        let resampled = CabSimIr::resample(raw_samples, original_rate, host_rate).map_err(|e| {
            Box::new(
                NamDiagnostic::new(NamErrorCode::IrLoadFailed, sys)
                    .message(format!(
                        "Failed to resample cab-sim IR ({original_rate} Hz -> {host_rate} Hz)"
                    ))
                    .param("error", e.to_string()),
            )
        })?;
        Cow::Owned(resampled)
    } else {
        Cow::Borrowed(raw_samples)
    };

    let mut leveled = resolved.into_owned();
    let _gain = apply_ir_level_policy(&mut leveled, policy, host_rate);

    let build_adapter = || {
        ConvEngine::new(&leveled, partition_size)
            .map_err(|e| {
                Box::new(
                    NamDiagnostic::new(e, sys)
                        .message("Failed to build convolution engine for cab-sim IR")
                        .hint("The IR samples require more memory than available."),
                )
            })
            .and_then(|engine| {
                CabSimAdapter::new(Box::new(engine)).map_err(|e| {
                    Box::new(
                        NamDiagnostic::new(e, sys)
                            .message("Failed to build cab-sim convolution adapter")
                            .hint("The IR samples require more memory than available."),
                    )
                })
            })
    };

    let l = build_adapter()?;
    let r = build_adapter()?;
    Ok(CabSimPair {
        l: Box::new(l),
        r: Box::new(r),
        sample_rate: host_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_normalized_scales_half_and_clipping_irs_and_leaves_silence() {
        let mut half = vec![0.5, -0.25, 0.0];
        let g_half = apply_ir_level_policy(&mut half, IrLevelPolicy::PeakNormalized, 48_000);
        assert!((g_half - 2.0).abs() < 1e-6);
        assert!((half[0] - 1.0).abs() < 1e-6);
        assert!((half[1] + 0.5).abs() < 1e-6);

        let mut hot = vec![2.0, -1.0, 0.0];
        let g_hot = apply_ir_level_policy(&mut hot, IrLevelPolicy::PeakNormalized, 48_000);
        assert!((g_hot - 0.5).abs() < 1e-6);
        assert!((hot[0] - 1.0).abs() < 1e-6);

        let mut silent = vec![0.0; 16];
        let g_silent = apply_ir_level_policy(&mut silent, IrLevelPolicy::PeakNormalized, 48_000);
        assert_eq!(g_silent, 1.0);
        assert!(silent.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn preserved_keeps_original_amplitude() {
        let mut ir = vec![0.25, -0.125, 2.0];
        let original = ir.clone();
        let g = apply_ir_level_policy(&mut ir, IrLevelPolicy::Preserved, 48_000);
        assert_eq!(g, 1.0);
        assert_eq!(ir, original);
    }

    #[test]
    fn legacy_nam_plugin_matches_cpp_formula_across_sample_rates() {
        for &(rate, expected_gain) in &[
            (
                44_100u32,
                10.0_f64.powf(-18.0 / 20.0) * (48_000.0 / 44_100.0),
            ),
            (48_000u32, 10.0_f64.powf(-18.0 / 20.0)),
            (96_000u32, 10.0_f64.powf(-18.0 / 20.0) * 0.5),
        ] {
            let mut ir = vec![1.0, -0.5, 0.0];
            let g = apply_ir_level_policy(&mut ir, IrLevelPolicy::LegacyNamPlugin, rate);
            assert!((g as f64 - expected_gain).abs() < 1e-6);
            assert!((ir[0] as f64 - expected_gain).abs() < 1e-6);
            assert!((ir[1] as f64 + 0.5 * expected_gain).abs() < 1e-6);
        }
    }

    #[test]
    fn serde_roundtrip_and_default_compatibility() {
        let json = serde_json::to_string(&IrLevelPolicy::LegacyNamPlugin).unwrap();
        assert_eq!(json, "\"legacy-nam-plugin\"");
        let parsed: IrLevelPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, IrLevelPolicy::LegacyNamPlugin);
        assert_eq!(IrLevelPolicy::default(), IrLevelPolicy::PeakNormalized);
    }
}

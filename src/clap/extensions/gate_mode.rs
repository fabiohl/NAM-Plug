// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Noise gate operating mode selection (`Silence` legacy vs `Musical` C++-parity expander).
//!
//! - [`GateMode::Silence`] (default): Legacy Schmitt-trigger silence gate (`DynamicHysteresis`)
//!   operating post-IR with CPU-saving model bypass when closed. Existing DAW sessions and
//!   legacy v0/v1/pre-v2.1 state snapshots restore `Silence` so behavior never changes
//!   silently.
//! - [`GateMode::Musical`]: Continuous quadratic downward expander matching the C++
//!   reference (`AudioDSPTools/dsp/NoiseGate`), detecting input pre-model/pre-calibration and
//!   attenuating post-model/pre-cabsim-IR so amp noise is controlled while IR tails decay
//!   naturally.

use serde::{Deserialize, Serialize};

/// Noise gate operating mode stored in CLAP parameters and plugin state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum GateMode {
    /// Legacy block hysteresis silence gate with post-IR fade and closed-gate inference bypass.
    #[default]
    Silence = 0,
    /// Reference quadratic downward expander (detector pre-model, attenuation post-model/pre-IR).
    Musical = 1,
}

impl GateMode {
    /// Converts a discrete numeric index into [`GateMode`].
    #[inline]
    pub const fn from_u32(val: u32) -> Self {
        match val {
            1 => Self::Musical,
            _ => Self::Silence,
        }
    }

    /// Converts a CLAP parameter float value into [`GateMode`].
    #[inline]
    pub fn from_f32(val: f32) -> Self {
        if !val.is_finite() {
            return Self::Silence;
        }
        Self::from_u32(val.round().clamp(0.0, 1.0) as u32)
    }

    /// Converts [`GateMode`] into its discrete numeric index.
    #[inline]
    pub const fn to_u32(self) -> u32 {
        self as u32
    }

    /// Converts [`GateMode`] into its CLAP parameter float value.
    #[inline]
    pub const fn to_f32(self) -> f32 {
        (self as u32) as f32
    }

    /// Full display label used in CLAP parameter text and tooltips.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Silence => "Silence",
            Self::Musical => "Musical",
        }
    }

    /// Parses user/host parameter text into [`GateMode`].
    pub fn from_text(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "silence" | "sil" | "legacy" | "0" => Some(Self::Silence),
            "musical" | "mus" | "expander" | "1" => Some(Self::Musical),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_silence_for_legacy_compatibility() {
        assert_eq!(GateMode::default(), GateMode::Silence);
        assert_eq!(GateMode::from_u32(0), GateMode::Silence);
        assert_eq!(GateMode::from_u32(99), GateMode::Silence);
        assert_eq!(GateMode::from_f32(f32::NAN), GateMode::Silence);
        assert_eq!(GateMode::from_f32(f32::INFINITY), GateMode::Silence);
        assert_eq!(GateMode::from_f32(1.0), GateMode::Musical);
        assert_eq!(GateMode::from_f32(0.6), GateMode::Musical);
        assert_eq!(GateMode::from_f32(0.4), GateMode::Silence);
    }

    #[test]
    fn text_round_trip() {
        for mode in [GateMode::Silence, GateMode::Musical] {
            assert_eq!(GateMode::from_text(mode.as_str()), Some(mode));
            assert_eq!(GateMode::from_u32(mode.to_u32()), mode);
            assert_eq!(GateMode::from_f32(mode.to_f32()), mode);
        }
        assert_eq!(GateMode::from_text("expander"), Some(GateMode::Musical));
        assert_eq!(GateMode::from_text("legacy"), Some(GateMode::Silence));
        assert_eq!(GateMode::from_text("unknown"), None);
    }

    #[test]
    fn serde_round_trip() {
        let json = serde_json::to_string(&GateMode::Musical).unwrap();
        assert_eq!(json, r#""musical""#);
        let parsed: GateMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, GateMode::Musical);
    }
}

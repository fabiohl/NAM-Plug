// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the CLAP render extension.
//!
//! NAM is deterministic and causal, so it has no hard realtime requirement.
//! In offline mode (bounce/export), `AdaptiveCompute` is forced to `Off`
//! so the model always processes at maximum quality without soft-degrade.
//!
//! See: `clap/ext/render.h`

use crate::clap::plugin::{NamClapMainThread, RENDER_MODE_OFFLINE, RENDER_MODE_REALTIME};
use clack_extensions::render::{PluginRender, PluginRenderImpl, RenderMode};
use clack_plugin::prelude::*;
use std::sync::atomic::Ordering;

/// Type alias for the CLAP render extension registration.
pub type NamPluginRender = PluginRender;

impl<'a> PluginRenderImpl for NamClapMainThread<'a> {
    fn has_hard_realtime_requirement(&self) -> bool {
        false
    }

    fn set(&mut self, mode: RenderMode) -> Result<(), PluginError> {
        let old_val = self.shared.cold.render_mode.load(Ordering::Acquire);
        let val = match mode {
            RenderMode::Realtime => RENDER_MODE_REALTIME,
            RenderMode::Offline => RENDER_MODE_OFFLINE,
        };

        if old_val != val {
            let old_mode = if old_val == RENDER_MODE_OFFLINE {
                "Offline"
            } else {
                "Realtime"
            };
            let new_mode = if val == RENDER_MODE_OFFLINE {
                "Offline"
            } else {
                "Realtime"
            };
            // Report actual oversample factor from shared state, not a
            // hard-coded "max quality" claim. The audio thread does NOT
            // activate a 4x engine during offline mode (CLAP-F009).
            let os_factor = neural_amp_modeler_rs::dsp::oversample::OversampleFactor::from_f32(
                self.shared
                    .ui_to_rt
                    .param_oversample
                    .load(Ordering::Relaxed) as f32,
            );
            let os_label = match os_factor {
                neural_amp_modeler_rs::dsp::oversample::OversampleFactor::Off => "1x (Off)",
                neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X2 => "2x",
                neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X4 => "4x",
            };
            log::info!(
                "Render mode changed: {old_mode} -> {new_mode} \
                 (oversample={os_label}, adaptive_compute=Off in Offline mode)",
            );
        }

        self.shared.cold.render_mode.store(val, Ordering::Release); // pairs with Acquire load em processor/events.rs:136
        Ok(())
    }
}

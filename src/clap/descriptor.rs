// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Identity descriptor of the NAM-Plug plugin in CLAP format.

use clack_common::plugin::features;
use clack_plugin::prelude::*;

/// Returns the immutable plugin descriptor.
/// Read by the host during scan — must be deterministic and without allocations.
///
/// Feature identifiers are taken from the SDK constants exported by
/// `clack_common::plugin::features` (mirroring `include/clap/plugin-features.h`
/// from the CLAP 1.2.2 SDK / clap-sys 0.5).
/// Standard features only — non-standard features ($namespace:$feature)
/// are ignored by most hosts and should not be declared here.
pub fn nam_descriptor() -> PluginDescriptor {
    PluginDescriptor::new("br.eti.fabiolima.nam-plug", "NAM-Plug")
        .with_vendor("Fabio Lima")
        .with_url("https://github.com/fabiohl/NAM-Plug")
        .with_description("Real-time Neural Amp Modeler plugin (CLAP)")
        // `gate` uses the canonical SDK constant from clack-common 0.2.
        .with_features([
            features::AUDIO_EFFECT,
            features::DISTORTION,
            features::GATE,
            features::MONO,
        ])
}

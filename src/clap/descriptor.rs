// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Identity descriptor of the NAM-rs plugin in CLAP format.
//!
//! ## Plugin ID Stability Decision
//!
//! The CLAP plugin ID was changed from `br.eti.fabiolima.nam-rs` to
//! `br.eti.fabiolima.nam-plug` to reflect the repository and crate renaming
//! after the monorepo split. This breaks backward compatibility with DAW
//! sessions and presets that referenced the old ID — users upgrading from the
//! monorepo build will need to re-instantiate the plugin in existing projects.

use clack_plugin::prelude::*;

/// Returns the immutable plugin descriptor.
/// Read by the host during scan — must be deterministic and without allocations.
///
/// Feature strings validated against CLAP 1.2.2 (clap-sys 0.5 / clack 0.1),
/// as defined in `include/clap/plugin-features.h` from the CLAP SDK.
/// Standard features only — non-standard features ($namespace:$feature)
/// are ignored by most hosts and should not be declared here.
pub fn nam_descriptor() -> PluginDescriptor {
    PluginDescriptor::new("br.eti.fabiolima.nam-plug", "NAM-Plug")
        .with_vendor("Fabio Lima")
        .with_url("https://github.com/fabiohl/NAM-Plug")
        .with_description("Real-time Neural Amp Modeler plugin (CLAP)")
        .with_features([c"audio-effect", c"distortion", c"gate", c"mono"])
}

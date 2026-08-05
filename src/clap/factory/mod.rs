// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # CLAP Factory Implementations
//!
//! Exposes host factory interfaces for discovering plugin instances and preset collections.
//!
//! - **[`preset_discovery`]**: Factory scanning and serving `.nam` model files and presets to compatible DAW browsers.

pub mod preset_discovery;

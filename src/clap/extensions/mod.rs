// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # Standard CLAP Extensions Implementation
//!
//! This module implements the standardized CLAP protocol extensions allowing DAW hosts
//! to negotiate parameters, save/restore plugin state, render GUIs, report latency/tail, and manage audio ports.
//!
//! ## Implemented Extensions
//!
//! - **[`audio_ports`] / [`audio_ports_activation`]**: Audio I/O bus configuration (Main Input / Main Output).
//! - **[`gui`]**: Main thread GUI lifecycle binding with egui windows.
//! - **[`latency`]**: Dynamic latency reporting to the host when oversampling or resampling is active.
//! - **[`param_indication`]**: Visual parameter automation status mapping.
//! - **[`params`]**: CLAP parameter metadata, value formatting, and event dispatch.
//! - **[`preset_load`]**: Direct loading of `.nam` model files via host preset interfaces.
//! - **[`remote_controls`]**: Mapping of primary controls (Input, Output, Gate, Oversampling) to hardware controller pages.
//! - **[`render`]**: Quality mode negotiation (`Realtime` vs `Offline` HQ rendering).
//! - **[`state`] / [`state_context`] / `state_transaction`**: Transactional, thread-safe binary state serialization and restoration.

//! - **[`tail`]**: Audio tail duration reporting (0 samples for live processing).
//! - **[`track_info`]**: DAW track channel and metadata binding.

pub mod audio_ports;
pub mod audio_ports_activation;
pub mod latency;
pub mod param_indication;
pub mod params;
pub mod preset_load;
pub mod remote_controls;
pub mod render;
pub mod state;
pub mod state_context;
pub(crate) mod state_transaction;
pub mod tail;
pub mod track_info;

pub mod gui;

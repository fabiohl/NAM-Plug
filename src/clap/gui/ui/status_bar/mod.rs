// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Zone 5 (footer): Status bar with sample rate, latency, DSP load and telemetry.

mod metadata;
mod orchestrator;
mod telemetry;

pub(crate) use orchestrator::draw_zone5_status_bar;

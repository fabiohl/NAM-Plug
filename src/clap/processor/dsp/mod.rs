// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! DSP block proper: channel extraction, gate, inference,
//! resampling, output gain, peaks, and telemetry.

pub mod bypass;
pub mod channels;
pub mod gain;
pub mod orchestrator;
pub mod peaks;
pub mod telemetry;

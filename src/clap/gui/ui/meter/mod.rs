// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Vertical VU meter with tricolor gradient, analog peak hold and clipping LED.
//! Dispatches rendering to the appropriate backend (glow or CPU fallback).

pub mod cpu;
pub mod glow;
pub mod orchestrator;
pub mod readout;

pub(crate) use orchestrator::draw_vertical_meter;

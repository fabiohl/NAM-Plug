// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

pub(crate) mod drag_drop;
pub(crate) mod handler;
pub(crate) mod input_map;
pub(crate) mod lifecycle;
pub(crate) mod shaders;
pub(crate) mod state;

pub(crate) use state::NamPluginWindow;

#[cfg(test)]
#[path = "window_test.rs"]
mod window_test;

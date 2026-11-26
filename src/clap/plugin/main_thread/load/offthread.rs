// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Off-thread prepare entry point.

use super::*;

/// Off-thread entry point to the pure `prepare_model` phase.
///
/// A free function so the loader worker can call it without an instance of
/// [`NamClapMainThread`] — every input is an explicit parameter.
pub(crate) fn prepare_model_offthread(
    path: &Path,
    host_rate: u32,
    buffer_size: usize,
    sys: &SystemSnapshot,
) -> Result<PreparedModel, Box<NamDiagnostic>> {
    NamClapMainThread::prepare_model(path, host_rate, buffer_size, sys)
}

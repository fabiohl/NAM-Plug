// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

/// Shared state for the model file-picker dialog.
///
/// Owned by `ColdShared::dialog_state` (`Arc<DialogSharedState>`).
/// The dialog thread writes `pending_model` and clears `active` on completion.
/// The UI thread reads `active` to detect duplicate spawns.
/// `housekeeping()` (main thread) reads `pending_model` and propagates it.
pub(crate) struct DialogSharedState {
    /// Selected model path. Set to `Some(path)` on user selection,
    /// `Some(DIALOG_CANCELLED_SENTINEL)` on cancel, `Some(DIALOG_TIMEDOUT_SENTINEL)` on timeout.
    /// Cleared by `housekeeping()` after processing.
    pub pending_model: Mutex<Option<PathBuf>>,
    /// `true` while the dialog window is open. Set by the UI thread on spawn,
    /// cleared by the dialog thread on completion.
    /// Also cleared by `teardown_gui_resources()` on plugin destroy.
    pub active: AtomicBool,
}

/// Sentinel path used to signal cancellation to `housekeeping()`.
pub(crate) fn dialog_cancelled_sentinel() -> PathBuf {
    PathBuf::from("\x00__DIALOG_CANCELLED__")
}

/// Sentinel path used to signal timeout to `housekeeping()`.
pub(crate) fn dialog_timedout_sentinel() -> PathBuf {
    PathBuf::from("\x00__DIALOG_TIMEDOUT__")
}

/// Shared state for the IR (cab-sim) file-picker dialog.
pub(crate) struct IrDialogSharedState {
    /// Selected IR path. Same sentinel convention as `DialogSharedState`.
    pub pending_ir: Mutex<Option<PathBuf>>,
    /// `true` while the IR dialog window is open.
    pub active: AtomicBool,
}

impl DialogSharedState {
    /// Creates a new idle dialog state.
    pub fn new() -> Self {
        Self {
            pending_model: Mutex::new(None),
            active: AtomicBool::new(false),
        }
    }
}

impl IrDialogSharedState {
    /// Creates a new idle IR dialog state.
    pub fn new() -> Self {
        Self {
            pending_ir: Mutex::new(None),
            active: AtomicBool::new(false),
        }
    }
}

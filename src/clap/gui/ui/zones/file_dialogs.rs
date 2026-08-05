// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::dialog_state::{self, DialogSharedState, IrDialogSharedState};
use clack_plugin::host::HostSharedHandle;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

const DIALOG_TIMEOUT: Duration = Duration::from_secs(60);

/// Spawns a model file-picker dialog in a background thread.
///
/// All outcomes (selected, cancelled by user, timed out) set `pending_model`
/// and call `host_static.request_callback()` so `housekeeping()` always
/// processes the result — preventing stale `ui_loading` flags.
///
/// Returns the thread handle so the main thread can join it on teardown.
pub(crate) fn spawn_file_dialog(
    state: Arc<DialogSharedState>,
    host_static: HostSharedHandle<'static>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let path_opt = rfd::FileDialog::new()
                .add_filter("NAM Model", &["nam", "namb"])
                .pick_file();
            let _ = tx.send(path_opt);
        });

        let outcome = rx.recv_timeout(DIALOG_TIMEOUT);

        match outcome {
            Ok(Some(path)) => {
                if let Ok(mut guard) = state.pending_model.lock() {
                    *guard = Some(path);
                }
            }
            Ok(None) => {
                if let Ok(mut guard) = state.pending_model.lock() {
                    *guard = Some(dialog_state::dialog_cancelled_sentinel());
                }
            }
            Err(_) => {
                if let Ok(mut guard) = state.pending_model.lock() {
                    *guard = Some(dialog_state::dialog_timedout_sentinel());
                }
                log::warn!(
                    "NAM-rs: file dialog timed out after {}s",
                    DIALOG_TIMEOUT.as_secs()
                );
            }
        }

        state.active.store(false, Ordering::Release);
        host_static.request_callback();
    })
}

/// Spawns an IR file-picker dialog in a background thread.
///
/// Same outcome guarantees as `spawn_file_dialog`.
pub(crate) fn spawn_ir_file_dialog(
    state: Arc<IrDialogSharedState>,
    host_static: HostSharedHandle<'static>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let path_opt = rfd::FileDialog::new()
                .add_filter("WAV Impulse Response", &["wav"])
                .pick_file();
            let _ = tx.send(path_opt);
        });

        let outcome = rx.recv_timeout(DIALOG_TIMEOUT);

        match outcome {
            Ok(Some(path)) => {
                if let Ok(mut guard) = state.pending_ir.lock() {
                    *guard = Some(path);
                }
            }
            Ok(None) => {
                if let Ok(mut guard) = state.pending_ir.lock() {
                    *guard = Some(dialog_state::dialog_cancelled_sentinel());
                }
            }
            Err(_) => {
                if let Ok(mut guard) = state.pending_ir.lock() {
                    *guard = Some(dialog_state::dialog_timedout_sentinel());
                }
                log::warn!(
                    "NAM-rs: IR file dialog timed out after {}s",
                    DIALOG_TIMEOUT.as_secs()
                );
            }
        }

        state.active.store(false, Ordering::Release);
        host_static.request_callback();
    })
}

#[cfg(test)]
mod file_dialogs_test {
    use super::dialog_state;
    use super::dialog_state::{DialogSharedState, IrDialogSharedState};
    use std::sync::atomic::Ordering;

    #[test]
    fn test_dialog_state_sentinels_are_distinct() {
        let cancelled = dialog_state::dialog_cancelled_sentinel();
        let timedout = dialog_state::dialog_timedout_sentinel();
        assert_ne!(
            cancelled, timedout,
            "cancel and timeout sentinels must differ"
        );
        assert!(cancelled.to_string_lossy().contains("CANCELLED"));
        assert!(timedout.to_string_lossy().contains("TIMEDOUT"));
    }

    #[test]
    fn test_dialog_state_active_flag() {
        let state = DialogSharedState::new();
        assert!(!state.active.load(Ordering::Relaxed));
        state.active.store(true, Ordering::Relaxed);
        assert!(state.active.load(Ordering::Relaxed));
        state.active.store(false, Ordering::Relaxed);
        assert!(!state.active.load(Ordering::Relaxed));
    }

    #[test]
    fn test_ir_dialog_state_active_flag() {
        let state = IrDialogSharedState::new();
        assert!(!state.active.load(Ordering::Relaxed));
        state.active.store(true, Ordering::Relaxed);
        assert!(state.active.load(Ordering::Relaxed));
    }
}

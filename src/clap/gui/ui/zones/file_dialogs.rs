// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::dialog_state::{self, DialogSharedState, IrDialogSharedState};
use clack_plugin::host::HostSharedHandle;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const DIALOG_TIMEOUT: Duration = Duration::from_secs(60);

/// Spawns a model file-picker dialog in a background thread.
///
/// All outcomes (selected, cancelled by user, timed out) set `pending_model`
/// and call `host_static.request_callback()` so `housekeeping()` always
/// processes the result — preventing stale `ui_loading` flags.
///
/// R-09: if the plugin is destroyed while the picker is open (`alive_fence`
/// drops), the outcome is **discarded**: the path is never written to
/// `pending_model` and the host is never notified — `request_callback` on a
/// destroyed instance would be a use-after-free in the host event loop.
///
/// Returns the thread handle so the main thread can join it on teardown.
pub(crate) fn spawn_file_dialog(
    state: Arc<DialogSharedState>,
    host_static: HostSharedHandle<'static>,
    alive_fence: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    spawn_file_dialog_inner(
        state,
        alive_fence,
        {
            || {
                rfd::FileDialog::new()
                    .add_filter("NAM Model", &["nam", "namb"])
                    .pick_file()
            }
        },
        move || host_static.request_callback(),
    )
}

/// Testable core of [`spawn_file_dialog`]: the picker and the host
/// notification are injected so a fake picker can complete the dialog after
/// the fence is lowered (R-09 criterion: zero `request_callback` observed,
/// zero path written to `pending_model`).
fn spawn_file_dialog_inner(
    state: Arc<DialogSharedState>,
    alive_fence: Arc<AtomicBool>,
    picker: impl FnOnce() -> Option<PathBuf> + Send + 'static,
    notify_host: impl FnOnce() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let path_opt = picker();
            let _ = tx.send(path_opt);
        });

        complete_dialog(
            &state.pending_model,
            &state.active,
            &alive_fence,
            rx.recv_timeout(DIALOG_TIMEOUT),
            dialog_state::dialog_cancelled_sentinel(),
            dialog_state::dialog_timedout_sentinel(),
            notify_host,
        );
    })
}

/// Spawns an IR file-picker dialog in a background thread.
///
/// Same outcome guarantees as `spawn_file_dialog` (including the R-09 fence
/// discard: `pending_ir` is never written and the host is never notified
/// after the plugin is destroyed).
pub(crate) fn spawn_ir_file_dialog(
    state: Arc<IrDialogSharedState>,
    host_static: HostSharedHandle<'static>,
    alive_fence: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    spawn_ir_file_dialog_inner(
        state,
        alive_fence,
        {
            || {
                rfd::FileDialog::new()
                    .add_filter("WAV Impulse Response", &["wav"])
                    .pick_file()
            }
        },
        move || host_static.request_callback(),
    )
}

/// Testable core of [`spawn_ir_file_dialog`]; see [`spawn_file_dialog_inner`].
fn spawn_ir_file_dialog_inner(
    state: Arc<IrDialogSharedState>,
    alive_fence: Arc<AtomicBool>,
    picker: impl FnOnce() -> Option<PathBuf> + Send + 'static,
    notify_host: impl FnOnce() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let path_opt = picker();
            let _ = tx.send(path_opt);
        });

        complete_dialog(
            &state.pending_ir,
            &state.active,
            &alive_fence,
            rx.recv_timeout(DIALOG_TIMEOUT),
            dialog_state::dialog_cancelled_sentinel(),
            dialog_state::dialog_timedout_sentinel(),
            notify_host,
        );
    })
}

/// Single source of truth for the R-09 dialog-completion protocol, shared by
/// the model and IR pickers (one copy of the teardown-safety logic — a fix
/// here applies to both dialogs).
///
/// While the alive fence is up, the outcome is written to the pending slot;
/// with the fence down (plugin destroyed while the picker was open) the
/// outcome is discarded entirely. The host is notified only if the fence is
/// STILL up immediately before the call — the re-check closes the
/// check-then-act window (the fence may drop between the outcome handling
/// and `request_callback`, which would dispatch to a destroyed instance).
fn complete_dialog(
    pending: &std::sync::Mutex<Option<PathBuf>>,
    active: &AtomicBool,
    alive_fence: &AtomicBool,
    outcome: Result<Option<PathBuf>, std::sync::mpsc::RecvTimeoutError>,
    sentinel_cancel: PathBuf,
    sentinel_timeout: PathBuf,
    notify_host: impl FnOnce(),
) {
    // R-09: the plugin instance may have been destroyed while the picker
    // was open. Lower the fence ⇒ discard the outcome entirely: no write to
    // the pending slot and no `request_callback`.
    if !alive_fence.load(Ordering::Acquire) {
        log::debug!("NAM-rs: file dialog completed after teardown — outcome discarded (R-09)");
        active.store(false, Ordering::Release);
        return;
    }

    match outcome {
        Ok(Some(path)) => {
            if let Ok(mut guard) = pending.lock() {
                *guard = Some(path);
            }
        }
        Ok(None) => {
            if let Ok(mut guard) = pending.lock() {
                *guard = Some(sentinel_cancel);
            }
        }
        Err(_) => {
            if let Ok(mut guard) = pending.lock() {
                *guard = Some(sentinel_timeout);
            }
            log::warn!(
                "NAM-rs: file dialog timed out after {}s",
                DIALOG_TIMEOUT.as_secs()
            );
        }
    }

    active.store(false, Ordering::Release);
    // R-09 re-check immediately before the host call: the fence may have
    // dropped while the outcome was being written (TOCTOU closure).
    if alive_fence.load(Ordering::Acquire) {
        notify_host();
    }
}

#[cfg(test)]
#[path = "file_dialogs_test.rs"]
mod file_dialogs_test;

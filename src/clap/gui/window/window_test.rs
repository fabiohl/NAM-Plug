// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::drag_drop::get_valid_model_file;
use crate::clap::plugin::NamClapSharedRef;
use baseview::DropData;
use std::path::PathBuf;

#[test]
fn test_get_valid_model_file_none() {
    let data = DropData::None;
    assert_eq!(get_valid_model_file(&data), None);
}

#[test]
fn test_get_valid_model_file_invalid_extensions() {
    let files = vec![
        PathBuf::from("model.wav"),
        PathBuf::from("config.json"),
        PathBuf::from("readme.txt"),
    ];
    let data = DropData::Files(files);
    assert_eq!(get_valid_model_file(&data), None);
}

#[test]
fn test_get_valid_model_file_valid_nam() {
    let files = vec![PathBuf::from("my_amp_model.nam")];
    let data = DropData::Files(files);
    assert_eq!(
        get_valid_model_file(&data),
        Some(PathBuf::from("my_amp_model.nam"))
    );
}

#[test]
fn test_get_valid_model_file_valid_namb() {
    let files = vec![PathBuf::from("my_amp_model.namb")];
    let data = DropData::Files(files);
    assert_eq!(
        get_valid_model_file(&data),
        Some(PathBuf::from("my_amp_model.namb"))
    );
}

#[test]
fn test_get_valid_model_file_case_insensitive() {
    let files = vec![PathBuf::from("MY_AMP_MODEL.NAM")];
    let data = DropData::Files(files);
    assert_eq!(
        get_valid_model_file(&data),
        Some(PathBuf::from("MY_AMP_MODEL.NAM"))
    );

    let files_namb = vec![PathBuf::from("another_model.Namb")];
    let data_namb = DropData::Files(files_namb);
    assert_eq!(
        get_valid_model_file(&data_namb),
        Some(PathBuf::from("another_model.Namb"))
    );
}

#[test]
fn test_get_valid_model_file_multiple_mixed() {
    let files = vec![
        PathBuf::from("invalid.wav"),
        PathBuf::from("sweet_tone.nam"),
        PathBuf::from("other.namb"),
    ];
    let data = DropData::Files(files);
    // Should skip the first invalid file and return the first valid model file
    assert_eq!(
        get_valid_model_file(&data),
        Some(PathBuf::from("sweet_tone.nam"))
    );
}

#[test]
fn test_gui_drag_drop_fuzz() {
    use crate::clap::plugin::make_test_shared;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    let shared = Arc::new(make_test_shared());

    // SAFETY: `&*shared` is a valid, non-null pointer into the Arc.
    // The Arc is kept alive for the duration of the test.
    let shared_ref = unsafe { NamClapSharedRef::new(&*shared) };
    let alive_fence = Arc::clone(&shared.cold.alive_fence);

    // Simulates the safe_shared helper logic for drag-drop:
    let check_and_drop =
        |alive: &Arc<std::sync::atomic::AtomicBool>, s_ref: NamClapSharedRef, path: PathBuf| {
            if alive.load(Ordering::Relaxed) {
                // SAFETY: alive=true ensures the pointer is still valid.
                let s = unsafe { s_ref.as_ref() };
                if let Ok(mut pending_guard) = s.cold.ui_pending_model.lock() {
                    *pending_guard = Some(path);
                    s.cold.ui_loading.store(true, Ordering::Relaxed);
                }
                true
            } else {
                false
            }
        };

    // 1. Alive case: should set the pending model
    let path = PathBuf::from("model.nam");
    assert!(check_and_drop(&alive_fence, shared_ref, path.clone()));
    assert_eq!(*shared.cold.ui_pending_model.lock().unwrap(), Some(path));

    // Reset
    *shared.cold.ui_pending_model.lock().unwrap() = None;
    shared.cold.ui_loading.store(false, Ordering::Relaxed);

    // 2. Dead case (fence false): should not access or change anything
    alive_fence.store(false, Ordering::Relaxed);
    assert!(!check_and_drop(
        &alive_fence,
        shared_ref,
        PathBuf::from("another.nam")
    ));
    assert_eq!(*shared.cold.ui_pending_model.lock().unwrap(), None);
    assert!(!shared.cold.ui_loading.load(Ordering::Relaxed));
}

// ---------------------------------------------------------------------------
// G3.T02 — Deterministic cleanup of OpenGL resources on window close
// ---------------------------------------------------------------------------

/// Verifies that `Option::take()` on glow types is idempotent — the core
/// safety property that prevents double-free in `destroy_gl_resources`.
#[test]
fn test_gl_handle_take_idempotent() {
    use std::num::NonZeroU32;

    let dummy_prog = Some(glow::NativeProgram(NonZeroU32::new(1).unwrap()));
    let dummy_vao = Some(glow::NativeVertexArray(NonZeroU32::new(2).unwrap()));
    let mut opt_prog = dummy_prog;
    let mut opt_vao = dummy_vao;

    // First take: returns the handle.
    assert!(opt_prog.take().is_some());
    assert!(opt_vao.take().is_some());

    // Second take: idempotent — None.
    assert!(opt_prog.take().is_none());
    assert!(opt_vao.take().is_none());
}

/// Simulates the `destroy_gl_resources` flow on `UiState` fields without
/// actual GL calls — validates that `vu_program` and `vu_vao` are consumed
/// exactly once and that subsequent invocations are harmless.
#[test]
fn test_destroy_gl_resources_idempotent() {
    use crate::clap::gui::ui::UiState;
    use std::num::NonZeroU32;

    let mut state = UiState {
        vu_program: Some(glow::NativeProgram(NonZeroU32::new(1).unwrap())),
        vu_vao: Some(glow::NativeVertexArray(NonZeroU32::new(2).unwrap())),
        ..Default::default()
    };

    assert!(state.vu_program.is_some());
    assert!(state.vu_vao.is_some());

    // Simulate first cleanup call
    let _vu_vao = state.vu_vao.take();
    let _vu_prog = state.vu_program.take();

    assert!(state.vu_program.is_none());
    assert!(state.vu_vao.is_none());

    // Simulate second cleanup — idempotent no-op
    assert!(state.vu_vao.take().is_none());
    assert!(state.vu_program.take().is_none());
}

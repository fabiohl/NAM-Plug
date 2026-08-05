// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::clap::extensions::params::PARAM_INPUT_GAIN;
use crate::clap::plugin::make_test_shared;
use clack_host::prelude::EventBuffer;
use clack_plugin::events::event_types::{
    ParamGestureBeginEvent, ParamGestureEndEvent, ParamValueEvent,
};
use clack_plugin::prelude::InputEvents;
use std::sync::atomic::Ordering;

#[test]
fn test_gui_gestures_and_parameter_flow() {
    let shared = make_test_shared();

    // Simulates gesture begin, value change, and gesture end for input gain
    let input_idx = PARAM_INPUT_GAIN as usize;
    const BITS_PER_PARAM: u32 = 3;
    const CHANGED_SHIFT: u32 = 0;
    const BEGIN_SHIFT: u32 = 1;
    const END_SHIFT: u32 = 2;
    let offset = input_idx as u32 * BITS_PER_PARAM;
    shared
        .ui_to_rt
        .gesture_flags
        .fetch_or(1 << (offset + BEGIN_SHIFT), Ordering::Relaxed);
    shared
        .ui_to_rt
        .gesture_flags
        .fetch_or(1 << (offset + CHANGED_SHIFT), Ordering::Relaxed);
    shared
        .ui_to_rt
        .param_input_gain
        .store(1.5f32.to_bits(), Ordering::Relaxed);
    shared
        .ui_to_rt
        .gesture_flags
        .fetch_or(1 << (offset + END_SHIFT), Ordering::Relaxed);

    let mut output_events_buffer = EventBuffer::new();
    {
        let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);
        shared.write_gui_events(&mut output_events);
    }

    let input_view = InputEvents::from_buffer(&output_events_buffer);
    let mut begin_received = false;
    let mut value_received = false;
    let mut end_received = false;

    for event in &input_view {
        if let Some(begin) = event.as_event::<ParamGestureBeginEvent>()
            && begin.param_id().unwrap().get() == PARAM_INPUT_GAIN
        {
            begin_received = true;
        } else if let Some(val_ev) = event.as_event::<ParamValueEvent>() {
            if val_ev.param_id().unwrap().get() == PARAM_INPUT_GAIN {
                assert_eq!(val_ev.value(), 1.5);
                value_received = true;
            }
        } else if let Some(end) = event.as_event::<ParamGestureEndEvent>()
            && end.param_id().unwrap().get() == PARAM_INPUT_GAIN
        {
            end_received = true;
        }
    }

    assert!(begin_received, "Should receive ParamGestureBeginEvent");
    assert!(value_received, "Should receive ParamValueEvent");
    assert!(end_received, "Should receive ParamGestureEndEvent");

    // Verifies that the flags were cleared
    let flags = shared.ui_to_rt.gesture_flags.load(Ordering::Relaxed);
    assert_eq!(
        flags & (1 << (offset + BEGIN_SHIFT)),
        0,
        "begin flag should be cleared"
    );
    assert_eq!(
        flags & (1 << (offset + CHANGED_SHIFT)),
        0,
        "changed flag should be cleared"
    );
    assert_eq!(
        flags & (1 << (offset + END_SHIFT)),
        0,
        "end flag should be cleared"
    );
}

#[test]
fn test_file_picker_alive_fence_and_timeout() {
    let shared = Arc::new(make_test_shared());

    // 1. Success Case: alive_fence is true and we receive the path before timeout
    {
        shared.cold.ui_loading.store(true, Ordering::Relaxed);
        let alive_fence = Arc::clone(&shared.cold.alive_fence);
        let shared_addr = &*shared as *const NamClapShared as usize;

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Some(std::path::PathBuf::from("/tmp/model.nam")))
            .unwrap();

        // Simulates the Picker Manager Thread behavior
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(path_opt) => {
                if alive_fence.load(Ordering::Relaxed) {
                    let shared_ref = unsafe { &*(shared_addr as *const NamClapShared) };
                    if let Some(path) = path_opt
                        && let Ok(mut pending_guard) = shared_ref.cold.ui_pending_model.lock()
                    {
                        *pending_guard = Some(path);
                    }
                }
            }
            Err(_) => panic!("Should have received the path"),
        }

        assert_eq!(
            *shared.cold.ui_pending_model.lock().unwrap(),
            Some(std::path::PathBuf::from("/tmp/model.nam"))
        );
    }

    // Clears the state
    *shared.cold.ui_pending_model.lock().unwrap() = None;

    // 2. Case alive_fence is false: the plugin was destroyed before the picker returned
    {
        shared.cold.ui_loading.store(true, Ordering::Relaxed);
        let alive_fence = Arc::clone(&shared.cold.alive_fence);
        let shared_addr = &*shared as *const NamClapShared as usize;

        // Sets the alive fence to false (simulating plugin/GUI destruction)
        alive_fence.store(false, Ordering::Relaxed);

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Some(std::path::PathBuf::from("/tmp/model2.nam")))
            .unwrap();

        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(path_opt) => {
                if alive_fence.load(Ordering::Relaxed) {
                    // If it entered here, it would be an error (illegal access to freed address)
                    let shared_ref = unsafe { &*(shared_addr as *const NamClapShared) };
                    if let Some(path) = path_opt
                        && let Ok(mut pending_guard) = shared_ref.cold.ui_pending_model.lock()
                    {
                        *pending_guard = Some(path);
                    }
                }
            }
            Err(_) => panic!("Should have received the path"),
        }

        // Must remain None because alive_fence was false and prevented accessing shared_addr
        assert_eq!(*shared.cold.ui_pending_model.lock().unwrap(), None);
    }

    // 3. Timeout Case: rx.recv_timeout fails due to timeout, resetting ui_loading
    {
        // Restores alive_fence to true
        shared.cold.alive_fence.store(true, Ordering::Relaxed);
        shared.cold.ui_loading.store(true, Ordering::Relaxed);
        let alive_fence = Arc::clone(&shared.cold.alive_fence);
        let shared_addr = &*shared as *const NamClapShared as usize;

        let (_tx, rx) = std::sync::mpsc::channel::<Option<std::path::PathBuf>>();
        // We don't send anything through the channel to force the timeout

        match rx.recv_timeout(std::time::Duration::from_millis(10)) {
            Ok(_) => panic!("Should not have received anything"),
            Err(_) => {
                // Timeout occurred
                if alive_fence.load(Ordering::Relaxed) {
                    let shared_ref = unsafe { &*(shared_addr as *const NamClapShared) };
                    shared_ref.cold.ui_loading.store(false, Ordering::Relaxed);
                }
            }
        }

        // ui_loading must be reset to false
        assert!(!shared.cold.ui_loading.load(Ordering::Relaxed));
    }
}

#[test]
fn test_per_instance_hugepage_synced() {
    use crate::clap::test_util;

    let (_entry1, _host_info1, mut plugin_instance1) = test_util::make_test_plugin();
    let (_entry2, _host_info2, mut plugin_instance2) = test_util::make_test_plugin();

    let raw_ptr1 = plugin_instance1.plugin_handle().as_raw_ptr();
    let raw_ptr2 = plugin_instance2.plugin_handle().as_raw_ptr();

    let main_thread1_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr1,
            |w| Ok(w.main_thread().as_ptr()),
        )
        .unwrap()
    };
    let main_thread2_ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr2,
            |w| Ok(w.main_thread().as_ptr()),
        )
        .unwrap()
    };

    let mt1 = unsafe { &mut *main_thread1_ptr };
    let mt2 = unsafe { &mut *main_thread2_ptr };

    assert!(
        !mt1.hugepage_synced,
        "Instance 1 hugepage_synced should start false"
    );
    assert!(
        !mt2.hugepage_synced,
        "Instance 2 hugepage_synced should start false"
    );

    mt1.housekeeping();

    assert!(
        mt1.hugepage_synced,
        "Instance 1 hugepage_synced should be true after housekeeping"
    );
    assert!(
        !mt2.hugepage_synced,
        "Instance 2 must remain unsynced until its own housekeeping call"
    );

    mt2.housekeeping();
    assert!(
        mt2.hugepage_synced,
        "Instance 2 hugepage_synced should be true after housekeeping"
    );
}

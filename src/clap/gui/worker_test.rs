// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

fn mock_host_bridge() -> (clap_sys::host::clap_host, crate::clap::gui::GuiHostBridge) {
    let mut mock_host = clap_sys::host::clap_host {
        clap_version: clap_sys::version::CLAP_VERSION,
        host_data: std::ptr::null_mut(),
        name: c"Test Host".as_ptr(),
        vendor: std::ptr::null(),
        url: std::ptr::null(),
        version: std::ptr::null(),
        get_extension: None,
        request_restart: None,
        request_process: None,
        request_callback: None,
    };
    let raw = std::ptr::NonNull::new(&mut mock_host).unwrap();
    // SAFETY: `raw` points to the live mock `clap_host` owned by the caller
    // for the duration of the test.
    let shared_handle =
        unsafe { clack_plugin::host::HostAudioProcessorHandle::from_raw(raw).shared() };
    let bridge = crate::clap::gui::GuiHostBridge::new(&shared_handle);
    (mock_host, bridge)
}

#[test]
fn test_gui_worker_contains_generation_panic_and_stays_alive() {
    let (_mock_host, bridge) = mock_host_bridge();
    let worker =
        GuiWorker::spawn(GuiSharedState::new_test(), bridge).expect("test GUI worker must spawn");

    worker
        .tx
        .send(GuiWorkerJob::PanicProbe)
        .expect("panic probe must be queued");
    let (reply, rx) = mpsc::channel();
    worker
        .tx
        .send(GuiWorkerJob::LivenessProbe { reply })
        .expect("liveness probe must be queued");

    rx.recv_timeout(Duration::from_secs(10))
        .expect("GUI worker must survive a contained job panic and stay responsive");

    if let Some(handle) = worker.shutdown() {
        handle.join().expect("GUI worker must shut down cleanly");
    }
}

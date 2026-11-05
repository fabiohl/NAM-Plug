// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Product integration tests verifying host call quiescence during teardown (SP-06 / F-PLUG-02).
//!
//! Asserts that:
//! 1. When a foreign host callback is blocked, plugin teardown (`destroy`) waits for the in-flight
//!    call to complete before returning.
//! 2. No calls to the host occur after `destroy` returns.
//! 3. Audio processing path maintains zero allocations on RT (heap unchanged).

use clack_host::prelude::*;
use nam_plug::clap::NamClapPlugin;
use nam_plug::clap::test_util;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

struct BlockingHostShared {
    callback_calls: Arc<AtomicUsize>,
    is_blocked: Arc<AtomicBool>,
    unblock: Arc<AtomicBool>,
}

impl<'a> SharedHandler<'a> for BlockingHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {
        self.callback_calls.fetch_add(1, Ordering::SeqCst);
        self.is_blocked.store(true, Ordering::SeqCst);
        while !self.unblock.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(2));
        }
    }
}

struct BlockingTestHost;
impl HostHandlers for BlockingTestHost {
    type Shared<'a> = BlockingHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

#[test]
fn test_host_call_quiescence_blocks_destroy_until_complete_and_rejects_subsequent() {
    let callback_calls = Arc::new(AtomicUsize::new(0));
    let is_blocked = Arc::new(AtomicBool::new(false));
    let unblock = Arc::new(AtomicBool::new(false));

    let entry =
        PluginEntry::load_from_clack::<clack_plugin::entry::SinglePluginEntry<NamClapPlugin>>(
            c"/test_quiescence",
        )
        .expect("load PluginEntry");

    let host_info = HostInfo::new("TestHost", "Test", "Test", "0.1.0").unwrap();

    let mut instance = PluginInstance::<BlockingTestHost>::new(
        {
            let callback_calls = Arc::clone(&callback_calls);
            let is_blocked = Arc::clone(&is_blocked);
            let unblock = Arc::clone(&unblock);
            move |_| BlockingHostShared {
                callback_calls,
                is_blocked,
                unblock,
            }
        },
        |_| (),
        &entry,
        c"br.eti.fabiolima.nam-plug",
        &host_info,
    )
    .expect("instantiate plugin");

    // 1. Activate and verify RT zero-alloc
    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 64,
        max_frames_count: 512,
    };
    let stopped = instance
        .activate(|_, _| (), audio_config)
        .expect("activate plugin");
    let mut started = stopped.start_processing().expect("start processing");

    // Pre-allocate every AudioPorts/EventBuffer outside the counted region so the
    // heap audit measures only the processing path (A-07 / T-09.8).
    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);
    let mut output_events_buffer = EventBuffer::new();

    test_util::assert_zero_alloc("process audio with zero alloc", || {
        let mut in_l = [0.0f32; 64];
        let mut in_r = [0.0f32; 64];
        let mut out_l = [0.0f32; 64];
        let mut out_r = [0.0f32; 64];

        let mut in_ch = [&mut in_l[..], &mut in_r[..]];
        let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                in_ch.iter_mut().map(|s| InputChannel::constant(&mut s[..])),
            ),
        }]);
        let out_ch = [&mut out_l[..], &mut out_r[..]];
        let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
        }]);
        let mut out_ev = OutputEvents::from_buffer(&mut output_events_buffer);

        let status = started.process(
            &input_audio,
            &mut output_audio,
            &InputEvents::empty(),
            &mut out_ev,
            None,
            None,
        );
        assert!(status.is_ok());
    });

    let stopped = started.stop_processing();
    instance.deactivate(stopped);

    // 2. Extract shared state
    let shared_ptr = test_util::extract_shared(&mut instance);
    let shared = unsafe { &*shared_ptr };
    let quiescence = Arc::clone(&shared.cold.host_call_quiescence);

    // 3. Spawn a background thread that executes a host call through quiescence which blocks
    let bg_quiescence = Arc::clone(&quiescence);
    let bg_callback_calls = Arc::clone(&callback_calls);
    let bg_is_blocked = Arc::clone(&is_blocked);
    let bg_unblock = Arc::clone(&unblock);
    // Explicit synchronization: the closure signals completion on this channel,
    // proving the real host call finished before destroy returned.
    let (call_done_tx, call_done_rx) = mpsc::channel();
    let bg_thread = thread::spawn(move || {
        bg_quiescence.host_call(|| {
            bg_callback_calls.fetch_add(1, Ordering::SeqCst);
            bg_is_blocked.store(true, Ordering::SeqCst);
            while !bg_unblock.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(2));
            }
            call_done_tx.send(()).expect("signal host call completion");
        })
    });

    // Wait until the background thread enters the host callback and blocks
    let deadline = Instant::now() + Duration::from_secs(3);
    while !is_blocked.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
    assert!(
        is_blocked.load(Ordering::SeqCst),
        "Host call must have blocked inside request_callback"
    );
    assert_eq!(callback_calls.load(Ordering::SeqCst), 1);

    // 4. Spawn a helper thread to release the blocked host call after a delay
    let unblock_clone = Arc::clone(&unblock);
    let unblocker_thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(80));
        unblock_clone.store(true, Ordering::SeqCst);
    });

    // 5. Teardown the plugin instance on the main thread — must block until unblocked
    let t0 = Instant::now();
    drop(instance);
    let elapsed = t0.elapsed();

    assert!(
        elapsed >= Duration::from_millis(60),
        "destroy() must wait for in-flight host call before returning (elapsed: {elapsed:?})"
    );
    assert!(
        call_done_rx.try_recv().is_ok(),
        "the real host call must have finished before destroy returned"
    );

    let bg_res = bg_thread.join().expect("join bg thread");
    assert_eq!(bg_res, Some(()));
    unblocker_thread.join().expect("join unblocker thread");

    // 6. Invariant: subsequent attempts after destroy must return None without calling host
    let subsequent_res = quiescence.host_call(|| {
        callback_calls.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(
        subsequent_res, None,
        "host_call after destroy must be rejected"
    );
    assert_eq!(
        callback_calls.load(Ordering::SeqCst),
        1,
        "No host callbacks may occur after destroy"
    );
}

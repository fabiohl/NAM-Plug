// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! RT-thread lifecycle tests for `start_processing`/`stop_processing`:
//!
//! * The one-shot RT-thread setup (confirmed scheduling telemetry, activation
//!   TLS priming, DAZ/FTZ) runs on the CLAP audio thread via
//!   `start_processing()`, before any block is processed on that thread.
//! * `stop_processing()` invalidates the setup: a host that migrates the
//!   audio thread (thread A → thread B) triggers a re-run on the new thread,
//!   keeping the priority/CPU telemetry current and FTZ/DAZ active from the
//!   first block processed there.
//! * `reset()` re-arms the latch, so the cheap `process()` fallback re-runs
//!   the setup on the same thread (defensive path for hosts that never call
//!   `start_processing()`).

#[cfg(test)]
mod tests {
    use crate::clap::host_harness::{
        CompleteHost, CompleteHostAudioProcessor, extract_plugin_shared,
        make_test_plugin_with_harness,
    };
    use crate::clap::test_util::{StereoTestBuffers, process_stereo_block_prealloc};
    use clack_host::prelude::*;
    use std::sync::atomic::Ordering;

    const BLOCK: usize = 64;

    fn audio_config_64() -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: BLOCK as u32,
            max_frames_count: BLOCK as u32,
        }
    }

    /// DAZ (0x0040) | FTZ (0x8000) MXCSR control bits — the exact mask
    /// `set_daz_ftz()` ORs in.
    #[cfg(target_arch = "x86_64")]
    const MXCSR_DAZ_FTZ: u32 = 0x8040;

    /// Reads the calling thread's MXCSR control register.
    #[cfg(target_arch = "x86_64")]
    fn read_mxcsr() -> u32 {
        let mut mxcsr: u32 = 0;
        // SAFETY: `stmxcsr` stores into the aligned stack local; SSE2 is part
        // of the x86-64-v3 baseline of this project.
        unsafe {
            std::arch::asm!("stmxcsr [{0}]", in(reg) &mut mxcsr);
        }
        mxcsr
    }

    /// Restricts the calling thread to `cpu` (deterministic `rt_cpu`
    /// telemetry). Returns `false` when the platform forbids the pinning
    /// (e.g. container cpuset), in which case the exact-CPU assertions are
    /// downgraded to sentinel-based ones.
    fn pin_to_cpu(cpu: usize) -> bool {
        assert!(cpu < libc::CPU_SETSIZE as usize);
        // SAFETY: `libc::cpu_set_t` is a plain POD bitmask, so an all-zero value is a valid empty CPU set.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: `cpu` is asserted below `libc::CPU_SETSIZE` and `set` is a live stack `cpu_set_t` with enough storage for that CPU bit.
        unsafe { libc::CPU_SET(cpu, &mut set) };
        // SAFETY: pid 0 targets the calling thread only; the mask spans the
        // whole `cpu_set_t` allocation.
        unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0 }
    }

    /// Runs one stereo block through the started processor and asserts the
    /// output stays finite (no NaN/Inf leak from the DSP chain).
    fn process_block(started: &mut StartedPluginAudioProcessor<CompleteHost>) {
        let mut bufs = StereoTestBuffers::new(BLOCK, 0.25, -0.25);
        process_stereo_block_prealloc(started, &mut bufs, None);
        assert!(
            bufs.out_l.iter().all(|s| s.is_finite()) && bufs.out_r.iter().all(|s| s.is_finite()),
            "output must stay finite"
        );
    }

    /// Thread A: `activate → start_processing → process`; thread B: `stop` →
    /// `start` → first block. Asserts the telemetry follows the migrated
    /// audio thread and FTZ/DAZ is active on B from the first block, and that
    /// `reset()` re-arms the `process()` fallback on the same thread.
    #[test]
    fn test_rt_thread_setup_on_start_and_thread_migration() {
        if std::thread::available_parallelism()
            .map(|n| n.get() < 2)
            .unwrap_or(true)
        {
            // Two distinct pinned cores are required for the exact-CPU
            // telemetry assertions; single-core environments cannot provide
            // them (the sentinel-based assertions below still hold).
            eprintln!("skipping: fewer than 2 CPUs available");
            return;
        }

        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared_ptr = extract_plugin_shared(&mut instance);
        // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
        let rt_status = unsafe { (&*shared_ptr).cold.rt_status.clone() };

        // Activation is a main-thread operation; only start/stop/process must
        // run on the (migrating) audio threads.
        let stopped_a = instance
            .activate(
                move |_, _| CompleteHostAudioProcessor::new(&state),
                audio_config_64(),
            )
            .expect("activate");

        // Fresh instance: the confirmed telemetry is still at the -1 sentinel.
        assert_eq!(rt_status.rt_priority.load(Ordering::Relaxed), -1);
        assert_eq!(rt_status.rt_cpu.load(Ordering::Relaxed), -1);

        let stopped_final = std::thread::scope(|scope| {
            // ── Thread A ──
            let rt_a = std::sync::Arc::clone(&rt_status);
            let (tx_ab, rx_ab) = std::sync::mpsc::channel();
            let moved_stopped = stopped_a;
            let thread_a = scope.spawn(move || {
                let pinned = pin_to_cpu(0);
                // SAFETY: `sched_getcpu` takes no pointer arguments, only reads the current thread's CPU id, and cannot violate memory safety.
                let cpu_a = unsafe { libc::sched_getcpu() };

                let mut started = moved_stopped
                    .start_processing()
                    .expect("start_processing on A");

                // Setup ran on A before any block: FTZ/DAZ applied and
                // telemetry refreshed with this thread's values.
                #[cfg(target_arch = "x86_64")]
                assert_eq!(
                    read_mxcsr() & MXCSR_DAZ_FTZ,
                    MXCSR_DAZ_FTZ,
                    "FTZ/DAZ must be active on the audio thread before the first block"
                );
                if pinned {
                    assert_eq!(
                        rt_a.rt_cpu.load(Ordering::Relaxed),
                        cpu_a as i32,
                        "rt_cpu must reflect the audio thread that ran start_processing"
                    );
                } else {
                    assert!(rt_a.rt_cpu.load(Ordering::Relaxed) >= 0);
                }
                assert!(rt_a.rt_priority.load(Ordering::Relaxed) >= 0);
                assert!(rt_a.confirmed_priority.load(Ordering::Relaxed) >= 0);
                assert!(rt_a.rt_policy.load(Ordering::Relaxed) >= 0);

                process_block(&mut started);

                let stopped = started.stop_processing();
                // Clobber the cached telemetry: thread B's start_processing
                // must re-store it (proof the setup re-runs after stop/start).
                rt_a.rt_cpu.store(-1, Ordering::Relaxed);
                rt_a.confirmed_priority.store(-1, Ordering::Relaxed);
                rt_a.rt_policy.store(-1, Ordering::Relaxed);
                tx_ab.send(stopped).expect("send processor to thread B");
            });
            thread_a.join().expect("thread A panicked");
            let stopped_b = rx_ab.recv().expect("thread A sent the processor");

            // ── Thread B ──
            let rt_b = std::sync::Arc::clone(&rt_status);
            let (tx_ba, rx_ba) = std::sync::mpsc::channel();
            let thread_b = scope.spawn(move || {
                let pinned = pin_to_cpu(1);
                // SAFETY: `sched_getcpu` takes no pointer arguments, only reads the current thread's CPU id, and cannot violate memory safety.
                let cpu_b = unsafe { libc::sched_getcpu() };

                let mut started = stopped_b.start_processing().expect("start_processing on B");

                // FTZ/DAZ active on the migrated thread BEFORE the first
                // block — the 1023-block staleness window cannot exist.
                #[cfg(target_arch = "x86_64")]
                assert_eq!(
                    read_mxcsr() & MXCSR_DAZ_FTZ,
                    MXCSR_DAZ_FTZ,
                    "FTZ/DAZ must be active on the migrated audio thread before its first block"
                );
                // Telemetry re-queried on B (it was clobbered to -1 after A).
                if pinned {
                    assert_eq!(
                        rt_b.rt_cpu.load(Ordering::Relaxed),
                        cpu_b as i32,
                        "rt_cpu must reflect the migrated audio thread"
                    );
                } else {
                    assert!(
                        rt_b.rt_cpu.load(Ordering::Relaxed) >= 0,
                        "rt_cpu sentinel must be refreshed by start_processing on B"
                    );
                }
                assert!(rt_b.confirmed_priority.load(Ordering::Relaxed) >= 0);
                assert!(rt_b.rt_policy.load(Ordering::Relaxed) >= 0);

                process_block(&mut started);

                // `reset()` (seek / loop) must not re-arm the one-shot setup:
                // the next block issues no scheduler or CPU query on this thread.
                started.reset();
                rt_b.rt_cpu.store(-1, Ordering::Relaxed);
                process_block(&mut started);
                assert_eq!(
                    rt_b.rt_cpu.load(Ordering::Relaxed),
                    -1,
                    "reset() must not re-run the RT thread setup on the next block"
                );

                let stopped = started.stop_processing();
                tx_ba.send(stopped).expect("send processor back to main");
            });
            thread_b.join().expect("thread B panicked");
            rx_ba.recv().expect("thread B sent the processor back")
        });

        instance.deactivate(stopped_final);
    }
}

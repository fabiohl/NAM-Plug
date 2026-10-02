// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! `FtzDazGuard` per-block MXCSR preservation tests (T-P1.2.1).
//!
//! Validates the RAII guard that owns the steady-state FTZ/DAZ invariant on
//! every `process()` block:
//!
//! * Unit roundtrip: the guard asserts DAZ+FTZ while alive and restores the
//!   host's MXCSR verbatim on drop — including when the scope unwinds.
//! * Host preservation: `process()` never leaks FTZ/DAZ to the host, on every
//!   block (1050 consecutive blocks, past the former 1024-block reassertion
//!   window of `cycles_since_telemetry & 0x3FF`).
//! * Panic path: the MXCSR is still restored when the DSP unwinds through
//!   `catch_unwind` (`Drop` never panics, so restoration provably precedes
//!   the poisoning latch).
//! * Numerical stability: the guard changes no DSP bit — the same input
//!   processed across guard instantiations (separated by `reset()`) is
//!   bit-identical, and subnormal inputs stay finite.
//! * RT-safety: a guarded block performs zero heap allocations.

#[cfg(test)]
mod tests {
    use crate::clap::test_util::{self, StereoTestBuffers};
    use clack_host::prelude::*;

    /// DAZ (bit 6) | FTZ (bit 15) MXCSR control bits — the exact mask the
    /// guard ORs in on construction.
    #[cfg(target_arch = "x86_64")]
    const MXCSR_DAZ_FTZ: u32 = 0x8040;

    /// Reads the calling thread's MXCSR control register.
    #[cfg(target_arch = "x86_64")]
    fn read_mxcsr() -> u32 {
        let mut mxcsr: u32 = 0;
        // SAFETY: `stmxcsr` stores into the aligned stack local; SSE2 is part
        // of the x86-64-v3 baseline of this project.
        unsafe {
            core::arch::asm!("stmxcsr [{0}]", in(reg) &mut mxcsr);
        }
        mxcsr
    }

    /// Writes the calling thread's MXCSR control register.
    #[cfg(target_arch = "x86_64")]
    fn write_mxcsr(val: u32) {
        // SAFETY: `ldmxcsr` from a stack-local `u32` previously read via
        // `stmxcsr` (or derived from it by clearing valid control flags) is
        // unconditionally safe on x86-64.
        unsafe {
            core::arch::asm!("ldmxcsr [{0}]", in(reg) &val);
        }
    }

    fn audio_config_64() -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 64,
            max_frames_count: 64,
        }
    }

    /// Processes one stereo block, propagating the CLAP result so panic-path
    /// tests can observe `Err` without the harness panicking.
    fn process_one(
        started: &mut StartedPluginAudioProcessor<test_util::TestHost>,
        bufs: &mut StereoTestBuffers,
    ) -> Result<ProcessStatus, PluginInstanceError> {
        let mut input_channels = [bufs.in_l.as_mut_slice(), bufs.in_r.as_mut_slice()];
        let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input_channels.iter_mut().map(InputChannel::constant),
            ),
        }]);
        let output_channels = [bufs.out_l.as_mut_slice(), bufs.out_r.as_mut_slice()];
        let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
        }]);
        let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);
        started.process(
            &input_audio,
            &mut output_audio,
            &InputEvents::empty(),
            &mut output_events,
            None,
            None,
        )
    }

    fn ensure_isolated_crash_dir() {
        if std::env::var_os("NAM_CRASH_DIR").is_none() {
            let temp_dir = std::env::temp_dir().join("nam_plug_test_crashes");
            let _ = std::fs::create_dir_all(&temp_dir);
            // SAFETY: test-only env setup before any audio thread spawns.
            unsafe {
                std::env::set_var("NAM_CRASH_DIR", temp_dir.as_os_str());
            }
        }
    }

    /// Unit roundtrip: bits asserted while alive, host value restored verbatim.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_ftz_daz_guard_roundtrip_sets_and_restores() {
        let before = read_mxcsr();
        let cleared = before & !MXCSR_DAZ_FTZ;
        write_mxcsr(cleared);
        assert_eq!(read_mxcsr() & MXCSR_DAZ_FTZ, 0);

        {
            let _guard = super::super::FtzDazGuard::new();
            assert_eq!(
                read_mxcsr() & MXCSR_DAZ_FTZ,
                MXCSR_DAZ_FTZ,
                "guard must assert DAZ+FTZ while alive"
            );
            assert_eq!(
                read_mxcsr() & !MXCSR_DAZ_FTZ,
                cleared & !MXCSR_DAZ_FTZ,
                "guard must preserve every other MXCSR bit"
            );
        }

        assert_eq!(
            read_mxcsr(),
            cleared,
            "guard must restore the host MXCSR verbatim on drop"
        );
        write_mxcsr(before);
    }

    /// Unit unwind: `Drop` runs (and restores) when the scope panics.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_ftz_daz_guard_restores_on_unwind() {
        ensure_isolated_crash_dir();
        let before = read_mxcsr();
        let cleared = before & !MXCSR_DAZ_FTZ;
        write_mxcsr(cleared);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = super::super::FtzDazGuard::new();
            assert_eq!(read_mxcsr() & MXCSR_DAZ_FTZ, MXCSR_DAZ_FTZ);
            panic!("intentional unwind through FtzDazGuard");
        }));
        assert!(result.is_err(), "inner scope must have panicked");

        assert_eq!(
            read_mxcsr(),
            cleared,
            "guard Drop must restore MXCSR during unwinding"
        );
        write_mxcsr(before);
    }

    /// Host preservation on every block, past the old 1024-block window: no
    /// FTZ/DAZ state may ever leak to the host, whatever the host had set.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_process_preserves_host_mxcsr_every_block() {
        use std::sync::atomic::Ordering;

        let _ = Ordering::Relaxed;
        let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
        let stopped = instance
            .activate(|_, _| (), audio_config_64())
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");

        let mut bufs = StereoTestBuffers::new(64, 0.25, -0.25);
        // Warm-up: first-view port construction + one-shot thread setup settle
        // outside the measured window.
        for _ in 0..4 {
            process_one(&mut started, &mut bufs).expect("warm-up process");
        }

        // Host ships MXCSR with DAZ+FTZ clear — the guard must reassert them
        // per block for the DSP yet hand the thread back untouched.
        let host_before = read_mxcsr();
        let cleared = host_before & !MXCSR_DAZ_FTZ;
        write_mxcsr(cleared);

        for i in 0..1050 {
            process_one(&mut started, &mut bufs).expect("process");
            assert_eq!(
                read_mxcsr(),
                cleared,
                "host MXCSR must be preserved after block {i} (no FTZ/DAZ leak)"
            );
        }
        assert!(
            bufs.out_l.iter().all(|s| s.is_finite()) && bufs.out_r.iter().all(|s| s.is_finite()),
            "output must stay finite across the 1050-block run"
        );

        // Guarded blocks allocate nothing on the audio thread (stack `u32`
        // only — the static RT-alloc scan covers the rest structurally).
        test_util::assert_zero_alloc("FtzDazGuard process() block", || {
            process_one(&mut started, &mut bufs).expect("audited process");
        });
        assert_eq!(read_mxcsr(), cleared);

        write_mxcsr(host_before);
    }

    /// Panic path: MXCSR restored even when the DSP unwinds, and the poisoned
    /// fast path (no guard) likewise leaves the host MXCSR untouched.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_process_restores_host_mxcsr_on_panic_path() {
        use std::sync::atomic::Ordering;

        ensure_isolated_crash_dir();
        let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
        let stopped = instance
            .activate(|_, _| (), audio_config_64())
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");

        let mut bufs = StereoTestBuffers::new(64, 0.25, -0.25);
        process_one(&mut started, &mut bufs).expect("warm-up process");

        let host_before = read_mxcsr();
        let cleared = host_before & !MXCSR_DAZ_FTZ;
        write_mxcsr(cleared);

        super::super::TEST_PANIC_INJECTION.store(true, Ordering::Relaxed);
        let res = process_one(&mut started, &mut bufs);
        assert!(res.is_err(), "injected panic must surface as Err");
        assert_eq!(
            read_mxcsr(),
            cleared,
            "MXCSR must be restored even when the DSP panics"
        );

        let shared_ptr = test_util::extract_shared(&mut instance);
        let shared = unsafe { &*shared_ptr };
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_PROCESSOR_POISONED),
            "panic must latch the poisoned state"
        );

        // Poisoned fast path returns before the guard: MXCSR trivially kept.
        let res2 = process_one(&mut started, &mut bufs);
        assert!(res2.is_ok());
        assert_eq!(read_mxcsr(), cleared);

        write_mxcsr(host_before);
    }

    /// Numerical stability: the guard changes no DSP bit. The same input —
    /// including subnormal samples — processed across separate guard
    /// instantiations (split by `reset()`) is bit-identical and finite.
    #[test]
    fn test_ftz_guard_preserves_bit_identical_output() {
        let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
        let stopped = instance
            .activate(|_, _| (), audio_config_64())
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");

        let mut bufs = StereoTestBuffers::new(64, 0.25, -0.25);
        // Subnormal tail: exercises the FTZ path deterministically.
        bufs.in_l[0] = 1e-30;
        bufs.in_l[1] = -1e-30;
        bufs.in_r[0] = 1e-30;

        process_one(&mut started, &mut bufs).expect("first process");
        let first_l = bufs.out_l.clone();
        let first_r = bufs.out_r.clone();
        assert!(
            first_l.iter().all(|s| s.is_finite()) && first_r.iter().all(|s| s.is_finite()),
            "output with subnormal input must stay finite"
        );

        started.reset();
        process_one(&mut started, &mut bufs).expect("second process");

        assert_eq!(
            bufs.out_l, first_l,
            "output L must be bit-identical across FtzDazGuard instantiations"
        );
        assert_eq!(
            bufs.out_r, first_r,
            "output R must be bit-identical across FtzDazGuard instantiations"
        );
    }
}

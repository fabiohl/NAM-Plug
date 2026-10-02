<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Automated Test Suite & Benchmark Architecture — `NAM-Plug`

This document details the automated test suite, integration harness, property-based testing, allocation auditing, and Criterion benchmark targets for the `NAM-Plug` CLAP plugin subproject ([`../`](../)).

> [!NOTE]
> For manual QA procedures and DAW-specific human testing workflows (Bitwig, Fender Studio Pro), see [functional-tests.md](./functional-tests.md). For overall plugin architecture and internal design, see [architecture.md](./architecture.md).

---

## 1. Scope & Crate Features Taxonomy

`NAM-Plug` testing relies on Cargo feature flags defined in [`Cargo.toml`](../Cargo.toml):

| Feature Flag     | Description                                                        | Test/Bench Usage Scope                                                                                |
|:---------------- |:------------------------------------------------------------------ |:----------------------------------------------------------------------------------------------------- |
| **`testing`**    | Enables engine test utilities, generators, and fixture resolution. | Mandatory feature flag when running `NAM-Plug` integration tests and benches.                         |
| **`heap-audit`** | Intercepts memory allocations via `CountingAllocator`.             | Used by RT-safety tests to ensure zero heap allocations occur on the audio thread during `process()`. |
| **`stereo`**     | Enables dual-channel L/R processing.                               | Default feature enabled across standard builds and test runs.                                         |

---

## 2. Integration Harness & Dynamic Artifact Validation

The `NAM-Plug` test suite simulates a real CLAP host environment using [`clack-host`](https://crates.io/crates/clack-host).

### 2.1 Dynamic Artifact Validation ([`tests/clap/artifact_validator.rs`](../tests/clap/artifact_validator.rs))

Integration tests execute against the dynamically compiled `.so` plugin binary rather than in-process static links where applicable. The `ArtifactValidator` helper:

1. Locates the compiled target artifact (`target/debug/libnam_plug.so` or `target/release/libnam_plug.so`).
2. Computes the SHA-256 fingerprint of the `.so` binary to ensure test traceability.
3. Loads the plugin entrypoint dynamically via `PluginEntry::load(&artifact.path)`.

### 2.2 RT Heap Allocation Audit & Static Guards

RT memory and machine code safety is enforced via a three-layer defense-in-depth approach:

- **Dynamic Interceptor ([`tests/common/alloc_audit.rs`](../tests/common/alloc_audit.rs)):** When compiled with `--features "testing heap-audit"`, `tests/clap.rs` registers `CountingAllocator` as the `#[global_allocator]`. The test harness captures allocation counters before and after calling `started_processor.process()`, enforcing **zero heap allocations** on the audio thread.
- **Static AST-Light Scanner ([`utils/lib/verify_no_rt_alloc.sh`](../utils/lib/verify_no_rt_alloc.sh) / [`utils/lib/rt_alloc_scan.awk`](../utils/lib/rt_alloc_scan.awk)):** Runs during static analysis (`lints.sh`). Parses `src/clap/processor/` Rust sources, tracks brace depth while stripping comments and string literals, excludes whitelisted off-RT lifecycle hooks (`activate`, `deactivate`, panic handlers, test modules), and flags any illegal heap allocation or dynamic collection types (`Box::new`, `Vec::new`, `format!`, `Arc::new`, `HashMap`, etc.).
- **Static Machine-Code Codegen Guard ([`utils/verify_rt_codegen.sh`](../utils/verify_rt_codegen.sh)):** Runs during static analysis (`lints.sh` Phase 9). Disassembles the compiled `.so` with `nm` and `objdump` to verify machine code invariants in hot-path DSP routines (`process`, `process_sub_block`, `process_sub_block_chunked`, `process_crossfade_sub_block`, `process_tail_drain`, `drain_tail_into`), enforcing zero `malloc`/`free`, zero `div`/`idiv` in the core DSP loop, and zero illegal `__tls_get_addr` calls.

---

## 3. Automated Test Inventory

The automated test targets under [`../tests/`](../tests/) are structured into root test files and modular sub-suites.

### 3.1 Main Harness Entrypoint ([`tests/clap.rs`](../tests/clap.rs))

`tests/clap.rs` acts as the root harness declaring common utilities and submodules under `tests/clap/`.

### 3.2 Modular Sub-Suites ([`tests/clap/`](../tests/clap/))

The root harness declares modular sub-suites covering: dynamic artifact discovery and SHA256 integrity (`artifact_validator`), cross-machine determinism and float consistency across frame boundaries (`clap_cross_machine`), plugin lifecycle transitions and audio configuration renegotiation (`clap_lifecycle_test`), multi-instance concurrency and thread-safety (`clap_multi_instance`), CLAP × NAMCore C++ oracle parity with ESR/SNR gates across the 44.1/48/96 kHz certification rates (`clap_parity_multi_sr`), state persistence and version migration (`clap_state_migration`), and CLAP tail extension semantics (`tail_semantics`). Individual thresholds are defined at the top of each module and summarized in section 6, rather than duplicated in an inventory table.

#### 3.2.1 Multi-Rate Resampling Reference Oracle ([`tests/clap/clap_parity_multi_sr.rs`](../tests/clap/clap_parity_multi_sr.rs))

`test_clap_parity_multi_rate` exercises the plugin at **44.1 kHz**, **48 kHz** (native) and **96 kHz** with irregular buffers against the C++ NAMCore oracle. For `host_sr ≠ model_sr` the expected host-rate curve is produced by the reference-resampling oracle:

1. The stress signal is generated at the model's native rate (48 kHz) and rendered by the C++ oracle at that rate.
2. The plugin input is the reference resample of the native stress (`model_sr → host_sr`).
3. The oracle model input is the reference resample of that host input back to `model_sr` — the exact round-trip signal the plugin's input resampler presents to the model — so the input-stage group delay is embedded in the oracle input and cancels.
4. The expected host-rate curve is the reference resample of the oracle output (`model_sr → host_sr`) using the `NeuralAmpModeler-rs` `NamResampler` (minimum-phase polyphase sinc FIR, the same filter family the plugin's `StreamingResampleBuffer` embeds).
5. Group-delay compensation: both curves are content-aligned at equal host indices after the plugin's declared resampler latency (`latency_samples()`, zero-primed by the streaming adapter) is skipped; ESR/SNR are computed on the steady-state window.

Re-rendering the oracle over the round-trip model input cancels the sinc interpolation error, so the resampled rates sit on the same cross-implementation float floor as native 48 kHz (measured 2026-09-03: 44.1 kHz ESR ≈ 9.01e-12 / SNR ≈ 110.5 dB; 96 kHz ESR ≈ 8.13e-12 / SNR ≈ 110.9 dB).

### 3.3 Regression Containment Suite ([`tests/clap_e0_containment_test.rs`](../tests/clap_e0_containment_test.rs))

TDD red/green containment suite guarding against specific architectural regression cases:

- **CabSim Participation:** Ensures loaded impulse responses participate directly in audio processing and latency calculations.
- **Parameter State Fidelity:** Asserts parameter changes remain faithful through save/restore cycles.
- **Reset Semantics:** Validates DSP internal state clear during `reset()`.
- **Sample Rate Negotiation:** Confirms DSP pipeline re-initializes upon host sample rate changes.
- **State Deserialization:** Guarantees corrupt or partial state payloads recover gracefully.

### 3.4 Property-Based Testing ([`tests/clap_e2_proptest.rs`](../tests/clap_e2_proptest.rs))

Uses `proptest` to generate random audio buffer lengths, parameter value sequences, and event queues to stress test CLAP event handling and boundary condition handling.

### 3.5 Processor Bypass Test ([`tests/processor_bypass_test.rs`](../tests/processor_bypass_test.rs))

Tests plugin bypass processing, verifying bit-transparent phase cancellation (< -120 dBFS) when bypassed and smooth crossfade transitions during bypass state toggles.

### 3.6 Real-Time Fault Containment & Poisoning Suite ([`src/clap/processor_poisoning_test.rs`](../src/clap/processor_poisoning_test.rs))

Guards against real-time panic cascades and memory corruption (`F-NP-02`, `F-NP-09`):

- **Single Panic per Activation:** Injected panics in `process()` or `reset()` are safely caught and latch `poisoned = true` without crashing the host or causing panic loops.
- **Zero-Alloc O(1) Containment:** Once poisoned, subsequent `process()` calls execute an early O(1) branch that immediately silences output audio ports and returns `Ok(ProcessStatus::Continue)` with zero allocations and zero I/O.
- **Static Error Catalog & Zero Leaks:** Catches convert to `PluginError::Message(errors::processor::AUDIO_CALLBACK_PANICKED)` without `Box::leak`.
- **Atomic Telemetry & Off-RT Restart:** Panics raise `RT_STATUS_PROCESSOR_POISONED` on the atomic bitmask; the main thread polls this in `housekeeping()` and triggers a single `host.request_restart()`.
- **Corrupt State Discard & Clean Resumption:** Deactivating a poisoned processor discards `deactivated_dsp`, and reactivation clears the latch and status bit.

### 3.7 Common Test Utilities ([`tests/common/`](../tests/common/))

- **[`alloc_audit.rs`](../tests/common/alloc_audit.rs):** Global memory allocation counting interceptor.
- **[`metrics.rs`](../tests/common/metrics.rs):** Off-RT audio fidelity metrics: Peak, RMS, SNR (Signal-to-Noise Ratio), and ESR (Error-to-Signal Ratio).
- **[`perceptual.rs`](../tests/common/perceptual.rs):** Spectral and perceptual comparison helpers.
- **[`wav.rs`](../tests/common/wav.rs):** WAV file reading/writing helpers for fixture comparison.

---

## 4. Benchmark Suite Architecture — `benches/clap_bench.rs`

The benchmark suite under [`../benches/`](../benches/) uses [Criterion.rs](https://bheisler.github.io/criterion.rs/book/index.html) to measure host process block throughput, parameter modulation overhead, neural model inference across topologies, sample rate transitions, quality modes, and CabSim IR convolution.

### 4.1 Measured Execution Groups

1. **`CLAP_Infrastructure` (Zero-Inference Base Overheads):**

   - **`Passthrough`**: Measures baseline CLAP `process()` execution duration with empty event queues across buffer sizes:
     - **32, 64 samples** (ultra-low latency mode)
     - **128 samples** (standard Live mode)
     - **256, 512, 1024 samples** (DAW mixing/mastering buffers)
   - **`ParamModulation`**: Measures `process()` execution duration when handling continuous CLAP parameter automation events (`ParamValueEvent`) queued at sub-buffer intervals across block sizes (32..1024).
   - **`Bypass`**: Measures latency-compensated bit-transparent dry-path processing time at block size 64.

2. **`CLAP_Inference` (Real Neural Model Processing Matrix):**

   - **Neural Architecture Sweeps (Block Sizes 32..1024)**:
     - **`WaveNet_A1_Standard`**: Deep dilated convolution network (`wavenet_a1_standard.nam`).
     - **`WaveNet_A2_Slimmable`**: Slimmable dilated convolution container (`a2_example.nam`).
     - **`LSTM`**: Recurrent neural network topology (`lstm.nam`).
   - **Sample Rate Conversions**: Compares throughput at **44.1 kHz** (44.1→48k polyphase resampling), **48.0 kHz** (native rate), and **96.0 kHz** (96→48k downsampling).
   - **Quality Modes & Oversampling Factors**: Measures execution across **`Oversample_Off`** (Live default), **`Oversample_2x`**, **`Oversample_4x`**, and **`RenderMode_Offline_HQ`** (HQ offline mastering mode).
   - **CabSim IR Convolution**: Measures incremental cost of real-time time-domain / partitioned IR convolution (**`CabSim_Off`** vs **`CabSim_On`** with 512-sample IR).

3. **`CLAP_WorstCase` (Worst-Case Coverage, SP-P0.2 — T-P0.2.1–T-P0.2.4):**

   - **`SmallBlocks/{Passthrough,Bypass,Wet_LSTM}` (T-P0.2.1)**: 16/32-sample worst-case fixed cost (+ 64/128 as the anchor to the existing matrix), ns/sample via `Throughput::Elements`, p50/p99/max from the Criterion distribution.
   - **`DryDelay/{Mono,Stereo}` (T-P0.2.2)**: Pure engine `DelayLine<f32>` push/pop path (same per-sample pattern as `DryDelayLine::process_block`, no model), sweeping delay 0/12/256/max over 32/64/128/512 blocks — the official F-NPPERF-01 baseline.
   - **`EventFlood` (T-P0.2.3)**: Dense `ParamValueEvent`/`ParamModEvent` floods (16/64/256/1024 events over 64/128/512 blocks, plus the degenerate 1-event-per-sample case) — the mandatory F-NPPERF-05 baseline; ns/sample × event-density curves in the Criterion summary.
   - **`MonoStereo` / `Crossfade` / `RingOut` / `ThreadHop` (T-P0.2.4)**: Real mono (L==R) vs. stereo (L!=R) content (F-NPPERF-06 baseline); bypass crossfade in flight (`PARAM_BYPASS` at offset 0, 64-sample ramp active); gate-closed CabSim ring-out drain; 2-worker `process` alternation harness measuring TLS/MXCSR re-priming cost (feeds T-P1.1.1 — harness, not a fix).

### 4.2 Benchmark Fixtures & Real-Time Isolation ([`benches/common.rs`](../benches/common.rs))

- **Deterministic Fixtures**: All neural models and impulse responses are resolved and validated via cryptographic SHA-256 hashes prior to benchmark execution. Missing fixtures fail-closed immediately.
- **Off-Measurement Pre-Warming**: Models are instantiated, state-loaded, activated, and pre-warmed for 2048 samples *prior* to `b.iter(|| ...)` to eliminate cold cache and off-RT initialization bias.
- **Zero Audio-Thread Heap Allocation**: Inner iteration closures strictly operate on pre-allocated, 64-byte aligned buffers (`AlignedVec<f32>`).
- **Deterministic Execution Environment**: Inner loops and Criterion measurements require isolated CPU core execution (via `isolcpus` and `taskset`) to avoid scheduler jitter; see [Section 5.5](#55-deterministic-benchmark-execution--cpu-core-isolation-isolcpus--taskset).

---

## 5. Execution Commands & Developer Workflow

All test and benchmark execution commands **must be executed inside `./NAM-Plug/`**:

### 5.1 Verification Scripts (`utils/` & `utils/lib/`)

Top-level workflow entrypoints reside in `utils/`, while shared libraries and modular guard utilities reside in `utils/lib/` (`_lib.sh`, `rt_alloc_scan.awk`, `verify_no_rt_alloc.sh`):

```bash
# 1. Static analysis quality gate (formatting, SPDX headers, cargo check, cargo clippy, static RT scan, AppStream metadata sync)
./utils/lints.sh

# 2. Agile first line of defense QA suite
./utils/tests-quick.sh
```

`utils/lints.sh` executes a 9-phase static and quality audit matrix:

- **Fmt & Matrix Compilation:** `cargo fmt`, multi-target `cargo check` and strict `cargo clippy -D warnings` across feature combinations (`--all-features`, `--no-default-features`).
- **SPDX & Code Style Policies:** SPDX license header validation, anti-pattern checks, and documented `#[allow(clippy::)]` verification.
- **Static RT Allocation Guard:** Invokes `utils/lib/verify_no_rt_alloc.sh` (backed by `utils/lib/rt_alloc_scan.awk`) to statically verify zero heap allocations in `src/clap/processor/`.
- **AppStream Metadata Sync:** Verifies the AppStream metainfo release version stays synchronized with `Cargo.toml`.
- **Static RT Codegen Guard:** Invokes `utils/verify_rt_codegen.sh` to statically verify machine code invariants (zero heap calls, zero illegal divs, zero illegal TLS accesses) in compiled hot-path DSP routines.

The x86-64-v3 (AVX2/FMA) engine baseline is contractual: `NAM-Plug` links against `NeuralAmpModeler-rs` without enabling opt-in EVEX features, so default and release builds contain no EVEX machine code by construction and the feature matrix above proves all configurations compile cleanly.

`utils/tests-quick.sh` runs three phases, each persisting its output to `target/logs/quick-phaseN.log`, and closes with a typed receipt (`target/logs/quick-receipt.txt`). The artifact under test is selected by `ensure_clap_artifact` — honoring the authoritative `CLAP_PLUGIN_UNDER_TEST` (or `CLAP_PLUGIN_PATH`) override first — and the chosen path is exported as `CLAP_PLUGIN_UNDER_TEST` so every `dlopen`-based integration test and the release gates run against the exact same `.so` whose SHA256 is logged:

1. **Structural (debug)** — unit + integration tests with debug assertions ON. `ensure_clap_artifact debug` validates the `.so` artifact (fail-closed: missing artifact aborts with `FATAL:`) and logs its SHA256 before any test that `dlopen`s it. Under `NAM_QUICK_STRICT=1` the artifact is additionally freshness-gated: a `.so` older than any source input (`Cargo.toml`/`Cargo.lock`/`.cargo/config.toml`/`src/**`, including the patched sibling `NeuralAmpModeler-rs` tree) aborts the suite instead of being silently validated.
2. **Release verification (release)** — the release-only surface: `ensure_clap_artifact release` builds the `.so` under release codegen, then:
   - **CLAP × NAMCore parity oracle** — `test_clap_parity_multi_rate` (ESR < 1e-8, SNR > 80 dB at 44.1 kHz, 48 kHz native, and 96 kHz) compares the release `.so` against the C++ render binary (`NAM_CORE_RENDER_BIN` or `build/namcore_render`) through the multi-rate resampling reference oracle (see §3.2.1), executing when the render binary, the release `.so` and the model fixture are all present. The Phase 1 targets are not re-run under `--release` — debug assertions ON already validate that logic, and release codegen of the `.so` is exactly what the oracle measures. Missing prerequisites are never masked — they are recorded as `GAPS+=("clap_parity_multi_rate:missing_render_or_fixtures")` and reported as a `WARN GAP`.
   - **CabSim IR artifact test** — `test_cabsim_ir_changes_audio_release_artifact` `dlopen`s the release `.so` to prove a loaded IR changes the audio output.
3. **RT-Safety heap-audit (debug)** — zero-allocation `process()` gate via `--features testing,heap-audit` (`processor_heap_audit_test`).

The run closes with `OVERALL: PASSED` or `OVERALL: PASSED_WITH_GAPS` (with `NAM_QUICK_STRICT=1`, any GAP turns the run into a failure, and stale `.so` artifacts are rejected rather than rebuilt).

### 5.2 Direct Cargo Commands

```bash
# 1. Quick compilation and lint check for tests and benches
cargo check --tests --benches --features testing

# 2. Run standard automated unit and integration tests
cargo test --features testing

# 3. Run allocation audit RT-safety tests (also Phase 3 of tests-quick.sh)
cargo test --features testing,heap-audit --lib processor_heap_audit_test

# 4. Run property-based tests
cargo test --features testing --test clap_e2_proptest

# 5. Run Criterion benchmarks
cargo bench --features testing --bench clap_bench

# 6. Run the CLAP × NAMCore parity oracle (requires the C++ render binary;
#    also Phase 2 of tests-quick.sh when prerequisites are present).
#    Exercises 44.1 kHz, 48 kHz native and 96 kHz via the multi-rate
#    resampling reference oracle (ESR < 1e-8, SNR > 80 dB per rate).
NAM_REQUIRE_CPP_ORACLE=1 cargo test --features testing --release --test clap \
    test_clap_parity_multi_rate -- --ignored --nocapture
```

### 5.3 Isolated CI/CD Execution of the CLAP × NAMCore Parity Oracle

`NAM-Plug` is a self-contained subproject: in a clone without a sibling `../NeuralAmpModeler-rs` checkout, the parity oracle resolves the NAMCore C++ render binary through the following order (mirrored by both [`clap_parity_multi_sr.rs`](../tests/clap/clap_parity_multi_sr.rs) and [`tests-quick.sh`](../utils/tests-quick.sh)):

1. `NAM_CORE_RENDER_BIN` — formal environment contract (authoritative for isolated environments).
2. This repo's own `build/namcore_render` (built via `golden_gen_build.sh`).
3. `neural_amp_modeler_rs::testing::fixtures::render_bin_path()` — only resolves when the dependency is linked as a local path.
4. `../NeuralAmpModeler-rs/build/namcore_render` — development convenience for the co-located monorepo workspace only; silently skipped when absent.

For an isolated CI/CD job, provide the prebuilt oracle binary and model fixture explicitly:

```bash
# Authoritative binary contract — no sibling layout assumptions.
export NAM_CORE_RENDER_BIN=/opt/namcore/build/namcore_render/tools/render
# Optional: point the fixture resolver at the model directory.
export NAM_FIXTURES_DIR=/opt/namcore/models

# Fail loud on discovery mismatch instead of a masked SKIP-pass.
NAM_REQUIRE_CPP_ORACLE=1 cargo test --features testing --release --test clap \
    test_clap_parity_multi_rate -- --ignored --nocapture
```

When the oracle is unavailable, `tests-quick.sh` reports an actionable `WARN GAP: clap_parity_multi_rate:missing_render_or_fixtures` (instructing the operator to set `NAM_CORE_RENDER_BIN` or build under local `build/namcore_render`) rather than failing the suite; set `NAM_QUICK_STRICT=1` to promote any GAP to a hard failure.

### 5.4 Release Receipt & Dist-Pipeline Provenance (`build-release.sh`)

The distribution build (`utils/build-release.sh`) compiles the **`dist`**
profile (inherits `release`, PGO + optional BOLT reordering, `strip`, `panic =
"unwind"`) into `~/.clap/nam_plug.clap` and packages tarball/Flatpak
deliverables. Its provenance is certified by a **cryptographic build receipt**
at `target/release-receipt.json`, written **atomically** (temp file + `mv`
rename in Phase 8) so an interrupted build (SIGINT/SIGTERM) can never leave a
partial receipt that looks valid.

The receipt's mandatory fields are:

| Field                  | Meaning                                                                                                                                                                                            |
|:---------------------- |:-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `status`               | `CERTIFIED` **only** if every gate below actually ran (`skipped_gates` empty) **and** `git_dirty=false`; any skip, missing oracle/fixture/validator, or dirty tree ⇒ `INCOMPLETE` (not certified). |
| `skipped_gates`        | List of gates skipped in non-strict mode (`NAM_STRICT_RELEASE=0`), e.g. `clap_validator:unavailable`, `namcore_parity:missing_oracle_bin`.                                                         |
| `package`              | `name` + `version` (`nam-plug` v0.8.0).                                                                                                                                                            |
| `provenance`           | `git_commit`, `git_dirty`, `cargo_lock_sha256`, `rustc_version`, `rustflags` (the sanitized `CONFIG_RUSTFLAGS` actually used).                                                                     |
| `optimizations`        | `pgo_applied`, `bolt_applied`.                                                                                                                                                                     |
| `artifacts`            | `clap_installed_path` + `clap_installed_sha256`, plus tarball/Flatpak paths and SHA-256 when built.                                                                                                |
| `oracles_and_fixtures` | `oracle_render_bin` + SHA-256, `fixture_model_path` + SHA-256.                                                                                                                                     |

The five release gates run **against the distributed artifact** — not against
`target/release/libnam_plug.so`, which the quick QA validates and which is a
different, non-optimized binary. `CLAP_PLUGIN_UNDER_TEST="$CLAP_TARGET"` points
the gates at the exact installed `.so`:

1. **Symbol & SONAME validation** of the distributed artifact.
2. **External `clap-validator`** against the distributed artifact (skipped ⇒ `skipped_gates` entry; fail-closed in strict mode).
3. **NAMCore float parity** — `NAM_REQUIRE_CPP_ORACLE=1 CLAP_PLUGIN_UNDER_TEST="$CLAP_TARGET" cargo test ... test_clap_parity_multi_rate` (all three certification rates — see §3.2.1).
4. **CabSim IR artifact test** — `CLAP_PLUGIN_UNDER_TEST="$CLAP_TARGET" cargo test ... test_cabsim_ir_changes_audio_release_artifact`.
5. **Performance certification** — `cargo run --locked --profile dist --features testing --bin nam_perf_guard -- certify --clap "$CLAP_TARGET" --out target/perf-certification-report.json` (real-time deadline margins against the distributed artifact). A scenario is certified only when p99 stays below **85% of the block budget** — the same threshold the audio callback telemetry uses to flag a DSP overload — so the gate fails strictly before the runtime would report an overload; the per-scenario summary prints the gate verdict with the p99 share of the budget and the remaining headroom.

In strict mode (`NAM_STRICT_RELEASE=1`, default) the tree must be clean
(`check_git_clean_strict()` runs before the build and again right before the
receipt is written), any external `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` is
rejected, and a gate skip or missing prerequisite aborts with exit ≠ 0 — so a
`CERTIFIED` receipt can only ever exist for a clean-tree, all-gates-green
build of the exact artifact that is distributed.

### 5.5 Deterministic Benchmark Execution & CPU Core Isolation (`isolcpus` + `taskset`)

Micro-benchmarking real-time DSP inference and sub-microsecond CLAP processing requires rigorous environmental determinism. Without kernel-level CPU core isolation, performance measurements are subject to Linux kernel scheduler preemption, cross-core cache invalidation, frequency governor scaling, timer interrupts, and workstation background workload jitter.

> [!WARNING]
> **Environmental Noise & Baseline Invalidation**:
> Benchmark and performance guard measurements obtained without dedicated CPU core isolation reflect non-deterministic environmental noise. Such unisolated results **cannot serve as authoritative baselines** and are strictly invalid for certifying real-time performance optimizations or merging performance-sensitive changes.

To achieve reproducible micro-benchmarks and deterministic baseline capture, the host environment must be configured with kernel CPU isolation and pinned execution:

#### 1. GRUB Configuration (`isolcpus`, `nohz_full`, and `clocksource=tsc`)
Isolate dedicated physical CPU cores from the OS scheduler and tick interrupts, and guarantee sub-20ns timing primitives by configuring GRUB:

1. Edit `/etc/default/grub` (as `root` or via `sudo`) and append `isolcpus=8,9 nohz_full=8,9 clocksource=tsc tsc=reliable` to `GRUB_CMDLINE_LINUX_DEFAULT`:
   ```bash
   GRUB_CMDLINE_LINUX_DEFAULT="quiet splash ... isolcpus=8,9 nohz_full=8,9 clocksource=tsc tsc=reliable"
   ```
   *(Note: Adapt core indices `8,9` to the target machine's CPU topology, selecting dedicated performance cores free of hyperthreading siblings running host OS tasks).*
2. Update the bootloader configuration:
   ```bash
   sudo update-grub
   ```
3. Reboot the machine:
   ```bash
   sudo reboot
   ```
4. Confirm after boot that the kernel command line parameters and clocksource are active:
   ```bash
   cat /proc/cmdline | grep -E "isolcpus|nohz_full|clocksource"
   cat /sys/devices/system/clocksource/clocksource0/current_clocksource
   ```

> [!NOTE]
> **Clocksource Hygiene & Telemetry Overhead (T-P4.2.3)**:
> Systems operating with `current_clocksource = hpet` incur MMIO register bus overhead on system clock reads. Empirical profiling on an isolated performance core (AMD Ryzen 7 5700U, core 8) measured:
> - **Uncalibrated fallback (`Instant::now()` via HPET)**: **1134.66 ns/call** (~1.13 µs per checkpoint, ~2.27 µs per audio block).
> - **Calibrated RDTSC (`rdtsc_nanos()` mult/shift)**: **17.16 ns/call** (~40–50 cycles at 2.5 GHz, ~34 ns per audio block).
>
> That represents a **66× reduction** in hot-path telemetry overhead. To eliminate host activation freezes while guaranteeing sub-20ns telemetry, `NAM-Plug` initializes `calibrate_tsc()` asynchronously on a dedicated helper thread during off-RT `new_shared()`. It incurs **0ms blocking time** on host initialization or audio activation (`activate()`). During the ~60ms measurement window, initial audio blocks fall back safely to `Instant::now()`, after which all telemetry probes seamlessly switch to native 17.16 ns RDTSC execution. Configuring `clocksource=tsc` further optimizes vDSO for any off-RT callers.

#### 2. CPU Pinning via `taskset -c`
Execute benchmark suites and performance guards exclusively pinned to an isolated core (e.g. core `8`):
```bash
# Pin Criterion benchmarks to isolated core 8
taskset -c 8 cargo bench --features testing --bench clap_bench

# Pin performance certification guard to isolated core 8
taskset -c 8 cargo run --locked --profile dist --features testing --bin nam_perf_guard -- \
    certify --clap target/release/libnam_plug.so --out target/perf-certification-report.json
```

#### 3. Canonical Capture Workflow
When establishing or validating official performance baselines, execute the canonical 3-stage validation and capture sequence inside `./NAM-Plug/`:

```bash
# Step 1: Static analysis and code health gates
./utils/lints.sh

# Step 2: Full functional QA test suite, parity oracle, and RT zero-alloc verification
./utils/tests-quick.sh

# Step 3: Pinned deterministic benchmark capture on isolated core
taskset -c 8 cargo bench --features testing --bench clap_bench
```

### 5.6 GUI Testing Environment & Headless Verification (`xvfb-run`)

`NAM-Plug` includes comprehensive integration tests for the CLAP GUI extension (`PluginGui`), including embedded X11 window hierarchy negotiation and XEmbed parent/child tree binding (`gui_lifecycle_x11_embedded_set_parent_honest`).

Depending on the execution environment:

1. **Interactive Desktop (with active X11 / Xwayland display)**:
   Tests run directly against the active X11 display (`DISPLAY=:0`), creating an embedded test host window and asserting strict XEmbed parentage.
2. **Headless CI / Containerized Environments (without display)**:
   - When `DISPLAY` is unset (`env -u DISPLAY`), the GUI lifecycle tests verify the fail-closed negotiation contract: `create(X11 embedded)` is rejected honestly at creation time without leaking or mutating FSM state.
   - To run the full embedded X11 GUI lifecycle test in a headless runner, provide a virtual framebuffer using `xvfb-run`:
     ```bash
     xvfb-run -a cargo test --features testing --lib gui_lifecycle_x11_embedded_set_parent_honest
     ```
   - If `DISPLAY` is exported but points to an unreachable X11 server, the harness emits an explicit `[WARN GAP]` and skips the physical embedding assertions cleanly rather than producing an unhandled test panic.

---

## 6. Quality Gates & Baseline Standards

| Metric / Test Gate         | Threshold / Constraint                        | Enforced In                                                        |
|:-------------------------- |:--------------------------------------------- |:------------------------------------------------------------------ |
| **CLAP vs NAMCore Parity** | ESR < 1e-8, SNR > 80 dB @ 44.1/48/96 kHz      | `clap_parity_multi_sr.rs` (Phase 2 of `tests-quick.sh`, release Gate 3) |
| **Bypass Transparency**    | Phase cancellation < -120 dBFS                | `processor_bypass_test.rs`                                    |
| **RT Allocation Budget**   | Exactly 0 heap allocations during `process()` | `verify_no_rt_alloc.sh` (static) & `alloc_audit.rs` (dynamic) |
| **RT Codegen Invariants** | 0 heap allocs, 0 core-loop divs, 0 core-loop TLS | `verify_rt_codegen.sh` (static machine-code inspection) |
| **CLAP Event Handling**    | 0 panics / unhandled boundary conditions      | `clap_e2_proptest.rs`                                         |

Measured CLAP × NAMCore parity floor (2026-09-03, `wavenet_a1_standard.nam`, release artifact SHA256 `f07a7941…`):

| Host rate  | Resampler latency | ESR          | SNR      |
|:---------- |:----------------- |:------------ |:-------- |
| 44.1 kHz   | 11 samples        | 9.01e-12     | 110.5 dB |
| 48.0 kHz   | 0 (bypass)        | 7.98e-12     | 111.0 dB |
| 96.0 kHz   | 19 samples        | 8.13e-12     | 110.9 dB |

The resampled rates sit on the same cross-implementation float floor as native: the multi-rate reference oracle re-renders the C++ model over the exact round-trip input the plugin's resampler produces, cancelling the sinc interpolation error (the gate is therefore uniform across the rate matrix).

### 6.1 Reference Performance Baseline (Criterion Benchmark Matrix)

The following baseline metrics were measured using `taskset -c 8 cargo bench --features testing --bench clap_bench` under the `x86-64-v3` AVX2/FMA baseline on an isolated CPU core (see §5.5):

#### Neural Architecture Inference (Live Mode, Native 48 kHz, Stereo)

| Topology Family          | Model Fixture             | Block 32 | Block 64 | Block 128 | Block 256 | Block 512 | Block 1024 | Steady ns/sample   |
|:------------------------ |:------------------------- |:-------- |:-------- |:--------- |:--------- |:--------- |:---------- |:------------------ |
| **WaveNet A1 Standard**  | `wavenet_a1_standard.nam` | 21.8 µs  | 42.3 µs  | 83.9 µs   | 167.8 µs  | 335.7 µs  | 671.6 µs   | **~655 ns/sample** |
| **WaveNet A2 Slimmable** | `a2_example.nam`          | 18.4 µs  | 34.4 µs  | 70.2 µs   | 138.6 µs  | 276.7 µs  | 553.4 µs   | **~540 ns/sample** |
| **LSTM 1×3**             | `lstm.nam`                | 2.6 µs   | 4.8 µs   | 9.2 µs    | 17.9 µs   | 35.4 µs   | 70.1 µs    | **~69 ns/sample**  |

#### CLAP Infrastructure & Processing Overhead

| Execution Group                         | Block 32 | Block 64 | Block 128 | Block 256 | Block 512 | Block 1024 | Unit Cost      |
|:--------------------------------------- |:-------- |:-------- |:--------- |:--------- |:--------- |:---------- |:-------------- |
| **Passthrough (Zero-Inference)**        | 474 ns   | 576 ns   | 841 ns    | 1.21 µs   | 2.01 µs   | 3.83 µs    | ~3.7 ns/sample |
| **ParamModulation (Active Automation)** | 690 ns   | 890 ns   | 1.23 µs   | 1.90 µs   | 3.17 µs   | 3.85 µs    | ~3.8 ns/sample |
| **Bypass (Latency-Compensated)**        | —        | 463 ns   | —         | —         | —         | —          | ~7.2 ns/sample |

#### Quality Modes, Sample Rates & CabSim Convolution (WaveNet A1, Block Size 64)

| Configuration / Mode                   | Mean Latency / Duration | Incremental Cost vs Live Native 48k | Notes                                          |
|:-------------------------------------- |:----------------------- |:----------------------------------- |:---------------------------------------------- |
| **Native 48 kHz (Live, OS Off)**       | 41.9 µs                 | Baseline (1.00×)                    | Zero added latency                             |
| **Resample 44.1 kHz (Live, OS Off)**   | 52.5 µs                 | +10.6 µs (+25.3%)                   | Polyphase minimum-phase bandlimited FIR        |
| **Downsample 96.0 kHz (Live, OS Off)** | 23.7 µs                 | -18.2 µs (-43.4%)                   | 64 input samples = 32 internal DSP samples     |
| **Oversample 2× (Live)**               | 84.7 µs                 | +42.8 µs (+102%)                    | 2× internal neural iterations                  |
| **Oversample 4× (Live)**               | 167.3 µs                | +125.4 µs (+299%)                   | 4× internal neural iterations                  |
| **RenderMode Offline HQ (4×)**         | 168.0 µs                | +126.1 µs (+301%)                   | Deterministic HQ mastering mode                |
| **CabSim IR Convolution (512-sample)** | 42.6 µs                 | +1.2 µs (+2.8%)                     | Partitioned time-domain / SIMD FIR convolution |

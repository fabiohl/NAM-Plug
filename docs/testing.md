<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Automated Test Suite & Benchmark Architecture - `NAM-Plug`

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
- **Static Machine-Code Codegen Guard ([`utils/verify_rt_codegen.sh`](../utils/verify_rt_codegen.sh)):** Runs during release verification (`tests-quick.sh` Phase 2) and release packaging (`build-release.sh` Phase 4.5). Disassembles the compiled `.so` with `objdump` and runs the unified NAM codegen filter ([`utils/lib/dsp-hotpath-filter.py`](../utils/lib/dsp-hotpath-filter.py)) with the per-repo symbol profile ([`utils/codegen-profile.json`](../utils/codegen-profile.json)) to verify machine code invariants in hot-path DSP routines (`process`, `process_sub_block`, `process_sub_block_chunked`, `process_crossfade_sub_block`, `process_tail_drain`, `drain_tail_into`, `process_dry_contained_block`), enforcing zero `malloc`/`free`, zero `div`/`idiv` in the core DSP loop, and zero illegal `__tls_get_addr` calls. The filter/profile pair is byte-identical (filter) or per-repo (profile) with the engine's `verify-kernel-codegen.sh` and the Pipe's `build-release.sh` Phase 4.5.

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

### 3.7 Fixtures Freshness Gate ([`tests/fixtures_freshness.rs`](../tests/fixtures_freshness.rs))

Validates schema version, model configurations, and file integrity of test audio fixtures, neural models (`.nam`), and impulse response WAV files before test execution.

### 3.8 Receipt Semantics Test ([`tests/receipt_semantics_test.rs`](../tests/receipt_semantics_test.rs))

Validates parsed structure, strict pre-release validation invariants, and JSON/receipt parsing semantics for both quick and long QA receipt schemas (`quick-receipt.txt`, `long-audit-receipt.jsonl`).

### 3.9 Runtime Heap Audit Harness ([`tests/heap_audit.rs`](../tests/heap_audit.rs))

Exercises the plugin and processor under the allocation interceptor, confirming that zero heap allocation occurs during audio processing and state query hot paths.

### 3.10 Slint View Model Lifecycle & Bindings ([`tests/slint_view_model_test.rs`](../tests/slint_view_model_test.rs))

Tests declarative Slint UI view model updates, parameter reflection, VU meter levels, and reactive state synchronization with the plugin backend.

### 3.11 Slint Widget Component Tests ([`tests/slint_widgets_test.rs`](../tests/slint_widgets_test.rs))

Tests declarative UI component properties, slider ranges, layout bounds, and rendering callbacks under headless Slint test backends.

### 3.12 Common Test Utilities ([`tests/common/`](../tests/common/))

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

3. **`CLAP_WorstCase` (Worst-Case Coverage):**

   - **`SmallBlocks_{Passthrough,Bypass,Wet_LSTM}`**: 16/32-sample worst-case fixed cost (+ 64/128 as the anchor to the existing matrix), ns/sample via `Throughput::Elements`, p50/p99/max from the Criterion distribution.
   - **`DryDelay_{Mono,Stereo}`**: Pure engine `DelayLine<f32>` push/pop path (same per-sample pattern as `DryDelayLine::process_block`, no model), sweeping delay 0/12/256/max over 32/64/128/512 blocks.
   - **`EventFlood`**: Dense `ParamValueEvent`/`ParamModEvent` floods (16/64/256/1024 events over 64/128/512 blocks, plus the degenerate 1-event-per-sample case); ns/sample × event-density curves in the Criterion summary.
   - **`EventFloodModel`**: Same flood densities/blocks as `EventFlood` but with a loaded **WaveNet A1 / LSTM** model (`CLAP_WorstCase_EventFloodModel`), exposing the real sub-block fragmentation cost — an event per sample forces a 1-sample inference call, losing the per-call vectorization the empty pipeline never pays (F-UX-03, D1). The Sprint S7 decision criterion is cost **> 2×** the same block without events (gate met; see §5.10).
   - **`MonoStereo` / `Crossfade` / `RingOut` / `ThreadHop`**: Real mono (L==R) vs. stereo (L!=R) content; bypass crossfade in flight (`PARAM_BYPASS` at offset 0, 64-sample ramp active); gate-closed CabSim ring-out drain; 2-worker `process` alternation harness measuring TLS/MXCSR re-priming cost.

4. **`CLAP_Evidence_InferenceE2E` (End-to-End Regression Matrix):**

   - End-to-end inference matrix model × block (LSTM / WaveNet-A1 / WaveNet-A2 × 64/128/512 host samples), ns/sample via `Throughput::Elements` serving as the block-size evidence anchor for regression baselines.

### 4.2 Benchmark Fixtures & Real-Time Isolation ([`benches/common.rs`](../benches/common.rs))

- **Deterministic Fixtures**: All neural models and impulse responses are resolved and validated via cryptographic SHA-256 hashes prior to benchmark execution. Missing fixtures fail-closed immediately.
- **Off-Measurement Pre-Warming**: Models are instantiated, state-loaded, activated, and pre-warmed for 2048 samples *prior* to `b.iter(|| ...)` to eliminate cold cache and off-RT initialization bias.
- **Zero Audio-Thread Heap Allocation**: Inner iteration closures strictly operate on pre-allocated, 64-byte aligned buffers (`AlignedVec<f32>`).
- **Deterministic Execution Environment**: Inner loops and Criterion measurements require isolated CPU core execution (via `isolcpus` and `taskset`) to avoid scheduler jitter; see [Section 5.5](#55-deterministic-benchmark-execution--cpu-core-isolation-isolcpus--taskset).

### 4.3 Performance Regression Gate & Baseline Discipline

[`utils/tests-performance-regression.sh`](../utils/tests-performance-regression.sh) is the unified performance gate. It runs the Criterion `clap_bench` suite pinned to a dedicated core and delegates every interpretation to the engine binary `nam_perf_gate` — the single statistical authority shared with `NeuralAmpModeler-rs` and `NAM-Audio-Pipe` — for the environment fingerprint, baseline persistence, per-benchmark coverage and the machine regression verdict over Criterion `change/estimates.json`.

- **Expected coverage:** [`benches/clap_bench.manifest`](../benches/clap_bench.manifest) declares the mandatory Criterion groups (trailing-`*` patterns). A group that did not run, or an executed benchmark without a saved baseline series, fails closed (`BASELINE_COVERAGE_GAP`).
- **Modes:** `--check` (default, read-only) and `--bootstrap-baseline` (**human operator only**, on a calibrated host). The legacy rollback gate has been fully retired in favor of `nam_perf_gate`.
- **Gate resolution:** `NAM_PERF_GATE` (prebuilt binary) → `nam_perf_gate` on `PATH` (`cargo install NeuralAmpModeler-rs --features testing --bin nam_perf_gate`) → a sibling engine checkout (`NAM_ENGINE_DIR`, default `../NeuralAmpModeler-rs`). A gate that cannot be resolved fails closed.
- **Environment:** `NAM_BENCH_CORE`, `NAM_BASELINE_NAME` (default `ci-baseline`), `NAM_BENCH_MANIFEST`, `NAM_THERMAL_COOLDOWN_S` (default 180 s).
- **Governor guard:** before delegating to the shared gate, the wrapper reads `/sys/devices/system/cpu/cpu${NAM_BENCH_CORE}/cpufreq/scaling_governor` (core resolved by the canonical `pick_bench_core`). A governor other than `performance`, or an unreadable governor file, aborts both `--check` and `--bootstrap-baseline` with a fail-closed error. A log captured under a governor other than `performance` is **invalid** as evidence and must not be compared or persisted.

Baselines live in `.performance-baselines/` (gitignored). A missing baseline is `MISSING_BASELINE`; a fingerprint drift (CPU model, ISA, toolchain, governor, pinned core, producing commit) is `INCOMPARABLE_ENVIRONMENT`. Both fail closed — baselines are **never** silently re-created; renewal is an explicit human operation under a calibrated environment (see the engine's `docs/benchmarks.md` §4).

> [!IMPORTANT]
> **Baseline durability gap:** the shared `lib/_lib_core.sh` hardcodes `baseline_dir=".performance-baselines"` inside the working tree; there is **no** `NAM_BASELINE_DIR` override today. Consequently `git clean -fdx` (or a fresh clone) destroys the persisted series and fingerprint. Until the shared core gains a configurable directory, operators must archive the directory after `--bootstrap-baseline` and restore it before `--check`, e.g. `cp -a .performance-baselines ~/.cache/nam-plug/perf-baselines/` and `cp -a ~/.cache/nam-plug/perf-baselines/. .performance-baselines/`. This is a documented gap for the operator to resolve in the shared core; this repository deliberately does not fork the drift-pinned body.

### 4.4 Engine DSP vs. CLAP Plugin Boundary Differentiation

A fundamental question when auditing performance is whether `NAM-Plug`'s benchmark suite is simply a redundant extension of [`NeuralAmpModeler-rs`](../NeuralAmpModeler-rs)'s engine benchmarks. The two suites measure entirely distinct architectural boundaries:

| Architectural Layer | Core Engine (`NeuralAmpModeler-rs`) | CLAP Plugin (`NAM-Plug`) |
| :------------------- | :----------------------------------- | :------------------------ |
| **Measurement Target** | Raw DSP math kernels (GEMM, Conv1D, WaveNet dilations, fast math activations like Pwl/FastTanh). | End-to-end CLAP plugin boundary, C ABI dispatch (`clack`), buffer slicing, and audio/event routing. |
| **Memory Topology** | Pre-allocated, contiguous, SIMD-aligned Rust slices. | Raw host pointer arrays (`*const *const f32`), non-contiguous channel buffers, and dynamic slice wrapping via `AudioPorts`. |
| **Automation & Events** | Static parameters passed directly as scalar values to DSP kernels. | Sample-accurate event demultiplexing from `InputEvents`, sub-buffer slicing, parameter smoothing interpolation, and MIDI CC mapping. |
| **State & Lifecycle** | Pure stateless or stateful mathematical models executed sequentially. | State transitions, dynamic plugin delay compensation (PDC), bypass crossfade ramping, and CabSim ring-out drain. |
| **Thread Topology** | Deterministic single-thread execution in a controlled benchmark harness. | Multi-thread worker migration (`ThreadHop`), evaluating L1/L2 cache invalidation, TLS access, and SIMD MXCSR register state restoration. |

The core engine benchmark proves that the neural network math is optimal. The `NAM-Plug` benchmark proves that the host integration layer does not squander that efficiency through wrapper overhead, memory fragmentation, or ABI translation costs.

### 4.5 Insights for CLAP Hosts & DAW Integrators

The `clap_bench` suite provides concrete telemetry and design constraints for DAW developers, host integrators, and live audio engineers:

1. **Ultra-Low Buffer Viability (`CLAP_WorstCase_SmallBlocks`):**
   - Evaluates performance down to 16 and 32 samples (at 96 kHz, 16 samples represents a strict ~166 µs total time budget per block).
   - Isolates the fixed invocation overhead of entering/exiting the CLAP ABI from the linear per-sample DSP cost, revealing the minimum practical buffer size before per-block overhead dominates CPU consumption.
2. **Parameter Automation Scalability (`CLAP_WorstCase_EventFlood`):**
   - Sweeps automation density from 16 to 1024 events per block, concluding with the pathological worst-case scenario of 1 event per sample.
   - Measures whether the plugin's event scheduling (`schedule_input_events`) and sub-block partitioning (`process_sub_block`) introduce nonlinear or quadratic CPU scaling when a host streams continuous modulation curves (e.g., LFOs or envelope followers).
3. **Work-Stealing Thread Pool Migration (`CLAP_WorstCase_ThreadHop`):**
   - Alternates `process()` calls across two distinct worker threads, simulating DAWs that utilize dynamic work-stealing thread pools (e.g., Bitwig Studio, Reaper).
   - Quantifies the cache line bouncing, TLS access overhead, and CPU pipeline flush penalty incurred when an audio task shifts between CPU cores across successive audio cycles.
4. **Resampling & Oversampling Trade-offs (`CLAP_Inference`):**
   - Quantifies the CPU budget differential between native processing (48 kHz) and streaming sample rate conversion (44.1 kHz, 96 kHz) via `StreamingResampleBuffer`.
   - Empirically contrasts `Oversample_Off` with `2x`, `4x`, and `RenderMode_Offline_HQ`, allowing hosts to provide accurate DSP load estimations to users.

### 4.6 Evaluation Methodology: Statistical Rigor & Real-Time Boundaries

[`utils/tests-performance-regression.sh`](../utils/tests-performance-regression.sh) executes a hardened statistical pipeline, but must be understood within the boundary of statistical throughput vs. hard real-time execution:

#### 1. Methodological Rigor & Guard Rails

- **Governor Gating (`guard_governor`):** Fails closed if the benchmark core's scaling governor is not set to `performance` (preventing DVFS transition jitter from polluting baseline comparisons).
- **Core Affinity (`pick_bench_core` & `taskset`):** Enforces execution on an isolated physical core to minimize OS scheduler preemption.
- **Thermal Stabilization (`PERF_GATE_COOLDOWN_SECS`):** Applies a mandatory 180s cooldown timer prior to measurement runs to prevent dynamic thermal throttling from distorting tails.
- **Fail-Closed Coverage (`clap_bench.manifest`):** Validates that all four required group patterns (`CLAP_Infrastructure_*`, `CLAP_Inference_*`, `CLAP_WorstCase_*`, `CLAP_Evidence_*`) executed and produced valid data before running comparison logic.
- **Null-Hypothesis Machine Verdict (`nam_perf_gate`):** Evaluates regression significance using Criterion's bootstrapping distributions (`change/estimates.json`) rather than simplistic wall-clock averages, discarding measurement noise.

#### 2. Real-Time Boundaries & Blind Spots

- **Statistical Throughput vs. Hard-RT Determinism:**
  Criterion benchmarks execute in user-space under standard Linux scheduling (`SCHED_OTHER`). They measure asymptotic throughput, median execution times, and statistical tail percentiles (p50/p95/p99). They **do not** prove hard real-time deadline guarantees under `SCHED_FIFO` priority, nor do they detect hardware interrupt (IRQ) jitter or kernel preemption spikes.
- **Synthetic Host Environment:**
  `clack-host` provides a clean, synthetic execution harness with perfectly pre-allocated buffers. In a production DAW session, dozens of audio tracks, virtual instruments, and GPU GUI rendering pipelines compete for shared L3 cache and memory bus bandwidth.
- **Complementary Verification:**
  To bridge this gap, performance regression gating must be complemented by:
  1. `utils/tests-quick.sh` Phase 3 (`heap-audit`): Verifying zero audio-thread dynamic memory allocations.
  2. `utils/verify_rt_codegen.sh`: Verifying disassembly invariants (zero `malloc`, zero `div`/`idiv`, bounded TLS).
  3. `utils/tests-long.sh` Phase 3: Auditing real-time execution under elevated thread priorities and real-time scheduling.

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

`utils/lints.sh` executes an 8-phase static and quality audit matrix:

- **Fmt & Matrix Compilation:** `cargo fmt`, multi-target `cargo check` and strict `cargo clippy -D warnings` across feature combinations (`--all-features`, `--no-default-features`).
- **SPDX & Code Style Policies:** SPDX license header validation, anti-pattern checks, and documented `#[allow(clippy::)]` verification.
- **Static RT Allocation Guard:** Invokes `utils/lib/verify_no_rt_alloc.sh` (backed by `utils/lib/rt_alloc_scan.awk`) to statically verify zero heap allocations in `src/clap/processor/` in ~0.2s without binary compilation.
- **AppStream Metadata Sync:** Verifies the AppStream metainfo release version stays synchronized with `Cargo.toml`.

| Utility Script                              | Scope & Operation                                                                                   | Usage Context                                                               |
|:------------------------------------------- |:--------------------------------------------------------------------------------------------------- |:--------------------------------------------------------------------------- |
| **`utils/lints.sh`**                        | Static analysis (fmt, check, clippy, SPDX, static RT alloc, AppStream)                              | Core continuous verification gate                                           |
| **`utils/tests-quick.sh`**                  | 3-phase agile gate (debug structural, release parity + RT codegen, heap audit)                      | First line of defense (local & pre-commit)                                  |
| **`utils/tests-long.sh`**                   | 4-phase heavy stress suite (GC, teardown, RT priority/latency, Xvfb GUI)                            | Nightly & release certification (operator)                                  |
| **`utils/tests-gui.sh`**                    | Headless Xvfb lifecycle, XEmbed, clipboard testing                                                  | Invoked by Phase 4 of `tests-long.sh` / on demand                           |
| **`utils/verify_rt_codegen.sh`**            | Disassembly inspection of `.so` (0 `malloc`, 0 `div`/`idiv`, bounded TLS)                           | Integrated in `tests-quick.sh` & `build-release.sh`                         |
| **`utils/tests-performance-regression.sh`** | Criterion `clap_bench` regression gate over the engine `nam_perf_gate` (coverage + machine verdict) | Operator baseline renewal; on-demand regression check (`--legacy` rollback) |
| **`utils/build-release.sh`**                | Multi-stage release builder with PGO, BOLT, RT codegen gate and packaging                           | Official release pipeline                                                   |
| **`utils/mod-update.sh`**                   | Supply chain update pipeline (rustup toolchain, cargo upgrade, Cargo.lock)                          | Periodic supply chain maintenance                                           |
| **`utils/test-lib-drift.sh`**               | Drift guard for shared QA library assets (`_lib_core.sh`, `dsp-hotpath-filter.py`, sysfs core tests)| Integrated in library QA & continuous verification                          |
| **`utils/test-pick-bench-core.sh`**         | Unit tests for CPU benchmark core selection against simulated sysfs trees                           | Invoked by `test-lib-drift.sh`                                              |

The x86-64-v3 (AVX2/FMA) engine baseline is contractual: `NAM-Plug` links against `NeuralAmpModeler-rs` without enabling opt-in EVEX features, so default and release builds contain no EVEX machine code by construction and the feature matrix above proves all configurations compile cleanly.

`utils/tests-quick.sh` runs three phases, each persisting its output to `target/logs/quick-phaseN.log`, and closes with a typed receipt (`target/logs/quick-receipt.txt`). The artifact under test is selected by `ensure_clap_artifact` — honoring the authoritative `CLAP_PLUGIN_UNDER_TEST` (or `CLAP_PLUGIN_PATH`) override first — and the chosen path is exported as `CLAP_PLUGIN_UNDER_TEST` so every `dlopen`-based integration test and the release gates run against the exact same `.so` whose SHA256 is logged:

1. **Structural (debug)** — unit + integration tests with debug assertions ON. Runs all unit and integration test targets (`cargo test --features testing --lib --tests`), actively enforcing execution of mandatory targets (`unittests src/lib.rs`, `tests/clap.rs`, `tests/fixtures_freshness.rs`, `tests/receipt_semantics_test.rs`, `tests/clap_e0_containment_test.rs`, `tests/clap_e2_proptest.rs`, `tests/processor_bypass_test.rs`, `tests/heap_audit.rs`, `tests/slint_view_model_test.rs`, `tests/slint_widgets_test.rs`) through fail-closed `assert_ran_target` assertions (minimum 350 executed tests). `ensure_clap_artifact debug` validates the `.so` artifact (fail-closed: missing artifact aborts with `FATAL:`) and logs its SHA256 before any test that `dlopen`s it. Under `NAM_QUICK_STRICT=1` the artifact is additionally freshness-gated: a `.so` older than any source input (`Cargo.toml`/`Cargo.lock`/`.cargo/config.toml`/`src/**`, including the patched sibling `NeuralAmpModeler-rs` tree) aborts the suite instead of being silently validated.
2. **Release verification (release)** — the release-only surface: `ensure_clap_artifact release` builds the `.so` under release codegen, then:
   - **CLAP × NAMCore parity oracle** — `test_clap_parity_multi_rate` (ESR < 1e-8, SNR > 80 dB at 44.1 kHz, 48 kHz native, and 96 kHz) compares the release `.so` against the C++ render binary (`NAM_CORE_RENDER_BIN` or `build/namcore_render`) through the multi-rate resampling reference oracle (see §3.2.1), executing when the render binary, the release `.so` and the model fixture are all present. The Phase 1 targets are not re-run under `--release` — debug assertions ON already validate that logic, and release codegen of the `.so` is exactly what the oracle measures. Missing prerequisites are never masked — they are recorded as `GAPS+=("clap_parity_multi_rate:missing_render_or_fixtures")` and reported as a `WARN GAP`.
   - **CabSim IR artifact test** — `test_cabsim_ir_changes_audio_release_artifact` `dlopen`s the release `.so` to prove a loaded IR changes the audio output.
   - **Static RT Codegen Guard** — invokes `utils/verify_rt_codegen.sh` on the newly verified release `.so` to verify machine-code invariants (0 `malloc`/`free`, 0 inner-loop divisions, bounded TLS) with zero additional compilation overhead.
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

#### 5.4.1 BOLT Win-Proof Ceremony — Opt-Out Decision (T5.3)

Unlike `NAM-Audio-Pipe`, `NAM-Plug` ships **no `ab-opt-ceremony` producer**:
there is no `utils/ab-opt-ceremony.sh` harness and no dedicated `ab_opt_bench`
binary. This is a recorded, deliberate decision (the cost of maintaining a
PMU-driven A/B ceremony inside the CLAP plugin outweighs its benefit):
**LLVM-BOLT is opt-out in `NAM-Plug`.**

Consequently:

- The canonical `target/logs/ab-opt-ceremony-receipt.json` is **not required**
  and is not generated by this repository.
- The only accepted independent proof that a BOLT transform actually won is
  this build's own **measured performance gate** (`PERF_GATE_STATUS=PASSED`),
  which promotes the claim to `perf-gate:<sha>` in the receipt.
- When BOLT is applied without any proof, the build records the typed gap
  `bolt_proof:missing_ceremony` in **every mode** (not only `--strict`), and the
  receipt status becomes `INCOMPLETE`. A `bolt_proof: MISSING` sentinel sitting
  next to a `CERTIFIED` status is therefore impossible, and a missing proof is
  never a silent, acceptable state.

### 5.5 Deterministic Benchmark Execution & CPU Core Isolation (`isolcpus` + `taskset`)

Micro-benchmarking real-time DSP inference and sub-microsecond CLAP processing requires rigorous environmental determinism. Without kernel-level CPU core isolation, performance measurements are subject to Linux kernel scheduler preemption, cross-core cache invalidation, frequency governor scaling, timer interrupts, and workstation background workload jitter.

> [!WARNING]
> **Environmental Noise & Baseline Invalidation**:
> Benchmark and performance guard measurements obtained without dedicated CPU core isolation reflect non-deterministic environmental noise. Such unisolated results **cannot serve as authoritative baselines** and are strictly invalid for certifying real-time performance optimizations or merging performance-sensitive changes.

To achieve reproducible micro-benchmarks and deterministic baseline capture, the host environment must be configured with kernel CPU isolation and pinned execution.

#### 1. Architectural Context: Host Delegation vs. Test/Benchmark Harness

As established in the architectural evaluation ([`docs/rt-hardening-evaluation.md`](./rt-hardening-evaluation.md)), `NAM-Plug` operates as a CLAP plugin (`cdylib`) and deliberately avoids process-wide RT hardening (`mlockall`, `SCHED_FIFO`, `sched_setaffinity`) during normal DAW operation, delegating thread management to the host DAW or PipeWire host.

However, during **automated testing, micro-benchmarking (`clap_bench`), statistical performance certification (`nam_perf_guard`), soak tests, and regression gating (`tests-performance-regression.sh`)**, the plugin code executes in standalone harness environments. In these scenarios, kernel-level core isolation via GRUB combined with explicit pinning (`taskset -c <core>`) is mandatory to eliminate operating system jitter, cross-core cache invalidation, frequency scaling transitions, and scheduler preemption. Furthermore, DAWs running on an isolated host can dedicate audio worker threads to these isolated cores for maximum real-time stability.

#### 2. Recommended Kernel Boot Command Line (`/etc/default/grub`)

Configure the Linux kernel boot line with maximum core isolation and low-latency timing primitives. A representative production boot configuration for a dedicated core pair (audio/benchmark core 8, SMT sibling 9 kept offline) on a 16-thread CPU (e.g. 8 cores / 16 threads) is:

```text
isolcpus=managed_irq,domain,8,9 nohz_full=8,9 rcu_nocbs=8,9 irqaffinity=0-7,10-15 nmi_watchdog=0 skew_tick=1 clocksource=tsc tsc=reliable
```

##### Parameter Breakdown

- `isolcpus=managed_irq,domain,8,9`: Removes cores 8 and 9 from general Linux scheduler load-balancing domains (`domain`) and excludes managed interrupt request distribution (`managed_irq`) from assigning device IRQs to these cores. *(Note: Legacy bare `isolcpus=8,9` without sub-flags leaves managed kernel IRQs attached to isolated cores, inducing sporadic preemption).*
- `nohz_full=8,9`: Enables full tickless kernel execution on cores 8 and 9. When only one runnable task executes on an isolated core (such as `taskset -c 8 cargo bench`), the periodic timer tick interrupt is suspended, eliminating scheduling jitter and timing noise.
- `rcu_nocbs=8,9`: Offloads Read-Copy-Update (RCU) callback invocations from cores 8 and 9 to housekeeping cores (0–7, 10–15), eliminating kernel RCU softirq preemption spikes during audio block processing and telemetry updates.
- `irqaffinity=0-7,10-15`: Specifies the default hardware interrupt mask, directing all hardware device IRQs (timers, storage controllers, network, GPUs, audio interfaces) away from the benchmark cores to housekeeping cores.
- `nmi_watchdog=0`: Disables Non-Maskable Interrupt (NMI) watchdog periodic ticks (~1 Hz) across all cores, eliminating recurring hardware interruptions that spoil sub-microsecond latency measurements or trigger false p99 tail spikes.
- `skew_tick=1`: Skews timer tick alignment across housekeeping cores to prevent simultaneous tick handling bursts across shared cache levels.
- `clocksource=tsc tsc=reliable`: Enforces invariant hardware RDTSC as the system clocksource, eliminating MMIO register bus overhead on clock reads and optimizing kernel vDSO timings. This directly pairs with `NAM-Plug`'s asynchronous TSC calibration (`calibrate_tsc()`), ensuring hot-path telemetry executes in native ~17 ns rather than ~1134 ns HPET fallback (see note below).

#### 3. GRUB Configuration & Verification Procedure

1. Edit `/etc/default/grub` (as `root` or via `sudo`) and update `GRUB_CMDLINE_LINUX_DEFAULT`:

   ```bash
   GRUB_CMDLINE_LINUX_DEFAULT="quiet splash ... isolcpus=managed_irq,domain,8,9 nohz_full=8,9 rcu_nocbs=8,9 irqaffinity=0-7,10-15 nmi_watchdog=0 skew_tick=1 clocksource=tsc tsc=reliable"
   ```

   *(Note: Adapt core indices `8,9` and housekeeping mask `0-7,10-15` to the target machine's CPU topology, selecting dedicated performance cores free of hyperthreading siblings running host OS tasks).*

2. Update the bootloader configuration:

   ```bash
   sudo update-grub
   ```

3. Reboot the machine:

   ```bash
   sudo reboot
   ```

4. Confirm after boot that kernel isolation parameters, tickless domains, and the TSC clocksource are active:

   ```bash
   cat /proc/cmdline | grep -E "isolcpus|nohz_full|rcu_nocbs|irqaffinity|clocksource"
   cat /sys/devices/system/clocksource/clocksource0/current_clocksource
   cat /sys/devices/system/cpu/isolated
   cat /sys/devices/system/cpu/nohz_full
   ```

> [!NOTE]
> **Clocksource Hygiene & Telemetry Overhead (T-P4.2.3)**:
> Systems operating with `current_clocksource = hpet` incur MMIO register bus overhead on system clock reads. Empirical profiling on an isolated performance core (AMD Ryzen 7 5700U, core 8) measured:
>
> - **Uncalibrated fallback (`Instant::now()` via HPET)**: **1134.66 ns/call** (~1.13 µs per checkpoint, ~2.27 µs per audio block).
> - **Calibrated RDTSC (`rdtsc_nanos()` mult/shift)**: **17.16 ns/call** (~40–50 cycles at 2.5 GHz, ~34 ns per audio block).
>
> That represents a **66× reduction** in hot-path telemetry overhead. To eliminate host activation freezes while guaranteeing sub-20ns telemetry, `NAM-Plug` initializes `calibrate_tsc()` asynchronously on a dedicated helper thread during off-RT `new_shared()`. It incurs **0ms blocking time** on host initialization or audio activation (`activate()`). During the ~60ms measurement window, initial audio blocks fall back safely to `Instant::now()`, after which all telemetry probes seamlessly switch to native 17.16 ns RDTSC execution. Configuring `clocksource=tsc` further optimizes vDSO for any off-RT callers.

#### 4. Runtime Host Calibration Commands

Beyond boot parameters, execute the following runtime configuration before running real-time benchmarks or performance certification guards:

```bash
# 1. Take the SMT sibling thread offline to eliminate hyperthreading contention:
echo 0 | sudo tee /sys/devices/system/cpu/cpu9/online

# 2. Lock CPU frequency governor to 'performance' (prevents DVFS transition latency):
echo performance | sudo tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor

# 3. Request the 'performance' energy-performance preference on amd-pstate-epp
#    (skipped on hosts that do not expose energy_performance_preference):
for epp in /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference; do
    [ -w "$epp" ] && echo performance | sudo tee "$epp" >/dev/null
done

# 4. Disable CPU frequency boost/turbo (ensures repeatable clocks without thermal throttling):
echo 0 | sudo tee /sys/devices/system/cpu/cpufreq/boost

# 5. Route hardware IRQs to housekeeping cores (fcff masks cores 8 and 9 on a 16-thread topology):
echo fcff | sudo tee /proc/irq/default_smp_affinity
for irq in /proc/irq/[0-9]*/smp_affinity_list; do
    [ -f "$irq" ] && echo 0-7,10-15 | sudo tee "$irq" >/dev/null 2>&1 || true
done

# 6. Allow PMU counter access for IPC and telemetry profiling without root:
echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid

# 7. Ensure real-time priority and locked memory limits are configured:
ulimit -r 95
ulimit -l unlimited
```

> [!IMPORTANT]
> The prerequisites are cumulative: isolated cores (`isolcpus=8,9 nohz_full=8,9` or the stricter sub-flag form above), `scaling_governor=performance`, `energy_performance_preference=performance` on `amd-pstate-epp`, and `boost=0`. A log captured with a governor other than `performance` is **invalid** and must not be used as a baseline or regression comparison; `utils/tests-performance-regression.sh` enforces this fail-closed before running.

#### 5. CPU Pinning via `taskset -c`

Execute benchmark suites and performance guards exclusively pinned to an isolated core (e.g. core `8`):

```bash
# Pin Criterion benchmarks to isolated core 8
taskset -c 8 cargo bench --features testing --bench clap_bench

# Pin performance certification guard to isolated core 8
taskset -c 8 cargo run --locked --profile dist --features testing --bin nam_perf_guard -- \
    certify --clap target/release/libnam_plug.so --out target/perf-certification-report.json
```

#### 6. Canonical Capture Workflow

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

### 5.7 GUI Open Latency (F-UX-05 / T5.1–T5.2)

The persistent GUI worker (`src/clap/gui/worker.rs`) pre-warms the Slint/winit
platform on its own thread from `gui.create()` (`GuiWorker::warm_up`), so the
host main thread no longer pays winit/GL backend startup inside the first
`set_parent`/`set_transient`/`show`. Every `open_window` measures the
host-main-thread block time and logs it at `info` level:

```text
NAM-Plug: GUI open_window completed in <N> ms
NAM-Plug: Slint platform pre-warmed in <N> ms (force_x11=<bool>)
```

**Criterion.** With the platform pre-warmed, the **p95 of the main-thread
block time** (the `open_window` line) must stay **< 50 ms** on both X11 and
Wayland sessions. This measurement is a **manual operator step** ([OPERADOR]):
open the GUI repeatedly under a real host, collect the log lines, and compute
the p95. `OPEN_WINDOW_TIMEOUT` stays at its 2 s safety value; it is only
lowered to ~500 ms once this criterion is observed to hold.

### 5.8 Host Memory Guards (OOM Containment)

This repository targets a 16 GiB development host. A release build of all test
targets (fat LTO) running alongside `rust-analyzer` exhausted RAM and triggered
a **global OOM**; because Electron's main process carries `oom_score_adj=300`,
the kernel killed the IDE while sparing the actual `rustc` memory hogs. The
following guards bound that failure mode:

- **Parallelism:** `.cargo/config.toml` sets `[build] jobs = 4` (override per
  invocation with `CARGO_BUILD_JOBS` on larger hosts).
- **Containment:** `utils/tests-quick.sh`, `utils/tests-gui.sh`,
  `utils/tests-long.sh` and `utils/build-release.sh` re-exec themselves through
  `utils/mem-guard.sh`, a transient systemd user scope with `MemoryHigh=5G`,
  `MemoryMax=6G`, `MemorySwapMax=6G` and `oom_score_adj=500` for the build tree.
  It fails open (with a warning) when the user manager or cgroup delegation is
  unavailable. `utils/tests-performance-regression.sh` is intentionally **not**
  auto-guarded (a memory cap could distort benchmark timings); wrap it manually
  when RAM is tight. Opt out of containment with `NAM_NO_MEM_GUARD=1`. Tune any
  run with `NAM_MEM_GUARD_MAX`.

- **GUI suite profile:** `utils/tests-gui.sh` builds only `--lib` in release
  (every GUI/X11 test lives in `src/`) and exports
  `CARGO_PROFILE_RELEASE_LTO=false` + `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`.
  GUI lifecycle semantics are unaffected; fat LTO was the direct OOM trigger.
- **Bench profile:** `[profile.bench]` uses thin LTO and 16 codegen units.
  **Baseline impact:** codegen now differs from the shipped fat-LTO
  `release`/`dist` binaries. The operator must re-bootstrap the Criterion
  baseline (`utils/tests-performance-regression.sh --bootstrap-baseline`)
  before `--check` is a reliable regression signal.
- **IDE protection:** Lower the running IDE's (Antigravity / VS Code family)
  `oom_score_adj` (default `-500`; negative values need `sudo`) so a runaway build is targeted first. Run it after
  starting/restarting the IDE.
- **Manual heavy commands:** wrap them, e.g.
  `utils/mem-guard.sh cargo bench --no-run`.

Host-side recommendations (operated outside the repository): enable swap/zram,
move `/tmp` off tmpfs, and tune `rust-analyzer` (`checkOnSave`, build scripts,
thread count).

### 5.9 Adaptive Rebuild Offload & Two-Phase NaN Containment (S6 / F-UX-02, F-UX-04)

- **Origin.** The two allocation-heavy structural refreshes (WaveNet slimmable
  slice + oversampling engine pair) and the non-finite containment used to run
  on the CLAP main thread / audio callback synchronously. S6 moved them to the
  `nam-model-loader` worker (`main_thread/model_loader.rs`, polled with a 10 ms
  `park_timeout` + `request_callback` handshake) and turned the containment into
  a two-phase amortized window (see `docs/architecture.md` §6.5 / §7.4).
- **Unit coverage.**
  `main_thread::model_loader_test::test_adaptive_rebuild_worker_services_slimmable_flag`
  and `…_oversample_flag` exercise the worker→slot→main-tick handshake and the
  single flag clear.
  `…::test_watchdog_fast_cadence_while_rebuild_pending` pins the S6-T2 fast
  watchdog cadence and its restoration to 250 ms.
  `processor::processor_reset_test::tests::test_non_finite_containment_two_phase_flag_cleared_and_recovers`
  pins the NaN containment: one flag clear per poisoning, held dry block, finite
  output throughout, wet resumption. The pre-existing
  `processor_stress_test::tests::test_non_finite_input_containment_and_recovery`
  and `…::test_non_finite_reset_uses_effective_model_rate` remain the recovery
  oracles.
- **Rollback switches (read once at instance creation).**
  `NAM_SYNC_MODEL_LOAD=1` (synchronous load) and
  `NAM_SYNC_ADAPTIVE_REBUILD=1` (inline structural rebuilds) revert to the legacy
  paths for A/B comparison.
- **Manual operator step [OPERADOR]:** the flag→swap latency target (< 30 ms p99
  under load) and the T6.4 decision (slice+prewarm > 5 ms ⇒ implement the
  drive-only exit) require a real host measurement on the reference machine; they
  are not enforced by the automated suites.

### 5.10 Dense Automation Mitigation (S7 / Épico D, F-UX-03)

**T5.3 decision gate.** The `CLAP_WorstCase_EventFloodModel` group (a loaded
WaveNet A1 / LSTM under the same 16/64/256/1024-event floods plus the degenerate
1-event-per-sample case) was measured on the reference machine under the
`performance` governor, against the no-event `CLAP_Inference_*` baseline for the
same block. Dense floods exceed the **> 2×** decision threshold, so sprint S7
was executed rather than closed as measurement-only.

**Implementation — exact piecewise gain fusion (T7.2).**
- Smoothed gain parameters (`PARAM_INPUT_GAIN`/`PARAM_OUTPUT_GAIN`, value or
  modulation) no longer end the inference sub-block. Only events that must stay
  sample-accurate (gate threshold, bypass, oversample, slim override, adaptive
  compute, activation precision) create a sub-block boundary.
- Fused gain targets are replayed at their exact sample offsets through the same
  analytic IIR ramp (`apply_gain_ramp_with_steps`), so the **gain stage is
  bit-identical** to the legacy per-event split
  (`prop_fused_ramp_bit_identical_to_legacy_split`, ESR = 0 ≪ −120 dB), and the
  neural model is block-invariant (`clap_e2_proptest`), so the fused inference
  call is numerically transparent. The equivalence is scoped to the gain stage
  and the model: the gate/mono hysteresis FSMs aggregate energy/max-diff over
  the (now larger) fused sub-block, so their transition timing near threshold
  can differ from the legacy split — the gate/bypass *event* boundaries
  themselves stay sample-accurate.
- `MIN_SPLIT_SAMPLES` (default 16) enables the fusion; `1`, an out-of-order event
  block (host-contract violation), an active bypass/crossfade, or a block larger
  than `MAX_RESAMP_BUF` all fall back to the legacy path verbatim.
  `MAX_GAIN_STEPS` (64) bounds the on-stack step buffer.
- T7.3: the PGO/BOLT workload (`pgo_profiling_workload.rs`, dense automation
  burst) now emits one smoothed-gain event per sample so the fused ramp path is
  profiled.

**Post-mitigation measurement (reference machine, `performance` governor).**
The degenerate 1-event-per-sample case collapses toward the no-event floor:

| Model      | Block | Density   | Pre-S7 ratio | Post-S7 ratio |
|:---------- |:----- |:--------- |:------------ |:------------- |
| WaveNet A1 | 64    | 64 ev     | 2.57×        | 1.57×         |
| WaveNet A1 | 64    | 1/sample  | 2.52×        | 1.55×         |
| LSTM       | 64    | 64 ev     | 3.20×        | 2.07×         |
| LSTM       | 512   | 1/sample  | 3.22×        | 2.00×         |

Inference-only (`CLAP_Inference_*`) baselines stay within run-to-run noise, so
the mitigation adds no regression to the no-event path. Gate-dense floods (a
boundary every few samples, e.g. 1024 events over a 64-sample block) remain
above 2× **by design**: the noise-gate threshold stays sample-accurate.

---

## 6. Quality Gates & Baseline Standards

| Metric / Test Gate         | Threshold / Constraint                           | Enforced In                                                             |
|:-------------------------- |:------------------------------------------------ |:----------------------------------------------------------------------- |
| **CLAP vs NAMCore Parity** | ESR < 1e-8, SNR > 80 dB @ 44.1/48/96 kHz         | `clap_parity_multi_sr.rs` (Phase 2 of `tests-quick.sh`, release Gate 3) |
| **Bypass Transparency**    | Phase cancellation < -120 dBFS                   | `processor_bypass_test.rs`                                              |
| **RT Allocation Budget**   | Exactly 0 heap allocations during `process()`    | `verify_no_rt_alloc.sh` (static) & `alloc_audit.rs` (dynamic)           |
| **RT Codegen Invariants**  | 0 heap allocs, 0 core-loop divs, 0 core-loop TLS | `verify_rt_codegen.sh` (static machine-code inspection)                 |
| **CLAP Event Handling**    | 0 panics / unhandled boundary conditions         | `clap_e2_proptest.rs`                                                   |

Measured CLAP × NAMCore parity floor (2026-09-03, `wavenet_a1_standard.nam`, release artifact SHA256 `f07a7941…`):

| Host rate | Resampler latency | ESR      | SNR      |
|:--------- |:----------------- |:-------- |:-------- |
| 44.1 kHz  | 11 samples        | 9.01e-12 | 110.5 dB |
| 48.0 kHz  | 0 (bypass)        | 7.98e-12 | 111.0 dB |
| 96.0 kHz  | 19 samples        | 8.13e-12 | 110.9 dB |

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

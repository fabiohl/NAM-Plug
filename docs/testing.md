<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Automated Test Suite & Benchmark Architecture - `NAM-Plug`

This document outlines the architecture, execution workflows, memory invariants, and benchmark methodologies of the automated test suite for the `NAM-Plug` CLAP plugin.

> [!NOTE]
> For manual QA procedures and host-specific functional workflows (Bitwig Studio, Reaper / Fender Studio Pro), see [functional-tests.md](./functional-tests.md). For overall plugin architecture and internal component design, see [architecture.md](./architecture.md).

---

## 1. Scope & Crate Features Taxonomy

`NAM-Plug` testing relies on Cargo feature flags defined in [`Cargo.toml`](../Cargo.toml):

| Feature Flag     | Description                                                        | Test / Bench Usage Scope                                                                              |
|:---------------- |:------------------------------------------------------------------ |:----------------------------------------------------------------------------------------------------- |
| **`testing`**    | Enables engine test utilities, generators, and fixture resolution. | Mandatory feature flag when running `NAM-Plug` integration tests and Criterion benchmarks.            |
| **`heap-audit`** | Intercepts memory allocations via `CountingAllocator`.             | Used by RT-safety tests to ensure zero heap allocations occur on the audio thread during `process()`. |
| **`stereo`**     | Enables dual-channel L/R processing.                               | Default feature enabled across standard builds and test runs.                                         |

---

## 2. Integration Harness & Dynamic Artifact Validation

The `NAM-Plug` test suite simulates real CLAP host environments using [`clack-host`](https://crates.io/crates/clack-host).

### 2.1 Dynamic Artifact Validation

*Source: [`tests/clap/artifact_validator.rs`](../tests/clap/artifact_validator.rs)*

Integration tests execute against the dynamically compiled `.so` plugin binary rather than in-process static links where applicable. The `ArtifactValidator` helper:

1. **Locates the compiled artifact:** Resolves `target/debug/libnam_plug.so` or `target/release/libnam_plug.so` (honoring `CLAP_PLUGIN_UNDER_TEST`).
2. **Fingerprints the binary:** Computes the cryptographic SHA-256 hash of the `.so` to ensure complete traceability.
3. **Loads via CLAP entrypoint:** Dynamically binds and executes plugin symbols through `PluginEntry::load(&artifact.path)`.

### 2.2 RT Heap Allocation Audit & Static Guards

Real-time audio processing strictly prohibits dynamic memory allocations, blocking I/O, and non-deterministic locks. Safety is enforced across three defensive layers:

- **Dynamic Interceptor ([`tests/common/alloc_audit.rs`](../tests/common/alloc_audit.rs)):**
  When compiled with `--features "testing,heap-audit"`, `tests/clap.rs` registers `CountingAllocator` as the global memory allocator. The test harness samples allocation counters before and after `process()`, asserting **zero heap allocations** on the audio thread.
- **Static AST-Light Scanner ([`utils/lib/verify_no_rt_alloc.sh`](../utils/lib/verify_no_rt_alloc.sh) / [`utils/lib/rt_alloc_scan.awk`](../utils/lib/rt_alloc_scan.awk)):**
  Executes during static analysis (`lints.sh`). Scans `src/clap/processor/` sources, tracks brace scoping while ignoring comments and string literals, excludes authorized off-RT lifecycle hooks (`activate`, `deactivate`, panic handlers), and flags forbidden heap patterns (`Box::new`, `Vec::new`, `format!`, `Arc::new`, `HashMap`).
- **Static Machine-Code Codegen Guard ([`utils/verify_rt_codegen.sh`](../utils/verify_rt_codegen.sh)):**
  Executes during release verification (`tests-quick.sh` Phase 2) and packaging (`build-release.sh`). Disassembles the compiled `.so` using `objdump` and runs the unified filter ([`utils/lib/dsp-hotpath-filter.py`](../utils/lib/dsp-hotpath-filter.py)) with the repository symbol profile ([`utils/codegen-profile.json`](../utils/codegen-profile.json)) to verify:
  - **Zero heap calls:** No `malloc`, `free`, or `realloc` instructions.
  - **Zero integer divisions:** No `div` or `idiv` in the core DSP loop.
  - **Bounded TLS access:** No illegal `__tls_get_addr` calls.
  - **Post-BOLT resilience:** Disassembles symbol-by-symbol and normalizes `.bolt.org.text*` sections to prevent false positives from dead bytes left by binary reordering.

---

## 3. Automated Test Inventory

Test targets reside in [`tests/`](../tests/) and test modules within `src/`.

### 3.1 Main Harness Entrypoint

*Source: [`tests/clap.rs`](../tests/clap.rs)*

`tests/clap.rs` acts as the primary harness, binding test configuration, shared allocators, and submodules under `tests/clap/`.

### 3.2 Modular Sub-Suites

*Directory: [`tests/clap/`](../tests/clap/)*

| Submodule / Test Target                                             | Scope & Invariants Verified                                                                                        |
|:------------------------------------------------------------------- |:------------------------------------------------------------------------------------------------------------------ |
| **[`artifact_validator`](../tests/clap/artifact_validator.rs)**     | Dynamic artifact discovery, `dlopen` loading, and SHA-256 fingerprint verification.                                |
| **[`clap_cross_machine`](../tests/clap/clap_cross_machine.rs)**     | Cross-machine determinism, buffer chunk boundary invariance, and numerical float consistency.                      |
| **[`clap_lifecycle_test`](../tests/clap/clap_lifecycle_test.rs)**   | Lifecycle transitions (`init`, `activate`, `deactivate`, `reset`) and dynamic audio configuration changes.         |
| **[`clap_multi_instance`](../tests/clap/clap_multi_instance.rs)**   | Multi-instance concurrency, thread safety, and resource isolation across parallel instances.                       |
| **[`clap_parity_multi_sr`](../tests/clap/clap_parity_multi_sr.rs)** | CLAP × NAMCore C++ oracle parity across topologies and rates (44.1, 48, 96 kHz), oversampling, and CabSim IR path. |
| **[`clap_state_migration`](../tests/clap/clap_state_migration.rs)** | State serialization, parameter persistence, backward compatibility, and corrupt payload recovery.                  |
| **[`tail_semantics`](../tests/clap/tail_semantics.rs)**             | CLAP tail extension semantics, CabSim ring-out decay, and zero-allocation tail drain.                              |

#### 3.2.1 Multi-Rate Resampling Reference Oracle

*Source: [`tests/clap/clap_parity_multi_sr.rs`](../tests/clap/clap_parity_multi_sr.rs)*

`test_clap_parity_multi_rate` exercises the plugin at **44.1 kHz**, **48 kHz** (native), and **96 kHz** with irregular buffers against the C++ NAMCore oracle. For `host_sr ≠ model_sr`, the expected host-rate curve is generated by a 5-step reference-resampling pipeline:

```text
Stress Signal (48 kHz) ───► [C++ Oracle @ 48k] ───► [Reference Resampler 48k→host_sr] ───► Expected Curve
         │                                                                                       ▲
         ▼                                                                                       │
[Reference Resampler 48k→host_sr]                                                          ESR / SNR Gate
         │                                                                                       │
         ▼                                                                                       ▼
   Plugin Input ──────────► [NAM-Plug (StreamingResampler + DSP)] ────────────────────────► Actual Curve
```

1. **Native Stress Generation:** Input is generated at the model's native rate (48 kHz) and rendered by the C++ oracle.
2. **Host Input Formulation:** Native stress is reference-resampled (`model_sr → host_sr`) to form the plugin input.
3. **Roundtrip Oracle Input:** The host input is resampled back (`host_sr → model_sr`), presenting the C++ oracle with the identical sinc group delay the plugin's internal resampler presents to the model.
4. **Expected Output Resampling:** The oracle's output is reference-resampled (`model_sr → host_sr`) using `NamResampler` (minimum-phase polyphase sinc FIR).
5. **Group Delay Alignment:** Both output curves are aligned after skipping the declared resampler latency (`latency_samples()`), calculating ESR and SNR over steady state.

##### Certified Baseline Parity Matrix

The matrix evaluates three topologies (`wavenet_a1_standard.nam`, `a2_example.nam`, `lstm.nam`) across all three rates:

| Fixture                  | 44.1 kHz ESR / SNR  | 48 kHz ESR / SNR    | 96 kHz ESR / SNR    | Gate Standard        |
|:------------------------ |:------------------- |:------------------- |:------------------- |:-------------------- |
| `wavenet_a1_standard`    | 9.01e-12 / 110.5 dB | 7.98e-12 / 111.0 dB | 8.13e-12 / 110.9 dB | ESR < 1e-8, SNR > 80 |
| `a2_example` (Slimmable) | 2.77e-12 / 115.6 dB | 2.75e-12 / 115.6 dB | 2.76e-12 / 115.6 dB | ESR < 1e-8, SNR > 80 |
| `lstm`                   | 1.91e-11 / 107.2 dB | 2.05e-11 / 106.9 dB | 2.03e-11 / 106.9 dB | ESR < 1e-8, SNR > 80 |

All configurations pass the uniform gate `ESR < 1e-8, SNR > 80 dB`. The definitive certified artifact hash is recorded in `target/release-receipt.json`.

#### 3.2.2 Artifact-Level Oversampling and CabSim Parity

- **Oversampling Parity (`test_clap_parity_oversample`):**
  Tests the release `.so` at 48 kHz with `OversampleFactor::X2` and `X4`. The reference feeds the native C++ oracle the identical half-band upsampled sequence produced by the plugin, then decimates the oracle output using the shared `OversampleEngine`.
  *Baseline:* X2 ESR ≈ 1.55e-11 / SNR ≈ 108.1 dB; X4 ESR ≈ 3.42e-11 / SNR ≈ 104.7 dB.
- **CabSim Convolution Parity (`test_clap_parity_cabsim`):**
  Convolves the artifact's dry model output with an impulse response using an independent direct time-domain FIR, comparing it against the artifact's internal convolution output (`max_frames_count = 1024`).
  *Baseline:* ESR ≈ 8.41e-14 / SNR ≈ 130.8 dB.

---

### 3.3 Additional Automated Test Suites

- **Regression Containment ([`tests/clap_e0_containment_test.rs`](../tests/clap_e0_containment_test.rs)):** Guards against architectural regressions (CabSim participation, parameter fidelity, reset semantics, sample rate negotiation, corrupt state recovery).
- **Property-Based Testing ([`tests/clap_e2_proptest.rs`](../tests/clap_e2_proptest.rs)):** Uses `proptest` to fuzz buffer lengths, parameter sequences, and event queues.
- **Processor Bypass Test ([`tests/processor_bypass_test.rs`](../tests/processor_bypass_test.rs)):** Verifies bit-transparent phase cancellation (< −120 dBFS) and smooth 64-sample crossfades.
- **Fault Containment & Poisoning ([`src/clap/processor_poisoning_test.rs`](../src/clap/processor_poisoning_test.rs)):** Ensures audio-thread panics latch `poisoned = true`, silence output via an $O(1)$ zero-alloc branch, raise `RT_STATUS_PROCESSOR_POISONED`, and safely trigger host restarts without crashing the DAW.
- **Fixtures Freshness Gate ([`tests/fixtures_freshness.rs`](../tests/fixtures_freshness.rs)):** Validates schema versions and cryptographic hashes of test models and audio fixtures.
- **Receipt Semantics ([`tests/receipt_semantics_test.rs`](../tests/receipt_semantics_test.rs)):** Validates JSON schema rules for `quick-receipt.txt` and `long-audit-receipt.jsonl`.
- **Runtime Heap Audit ([`tests/heap_audit.rs`](../tests/heap_audit.rs)):** Enforces zero dynamic memory allocations during active processing.
- **Slint UI View Model & Widgets ([`tests/slint_view_model_test.rs`](../tests/slint_view_model_test.rs), [`tests/slint_widgets_test.rs`](../tests/slint_widgets_test.rs)):** Verifies declarative Slint view models, VU meter synchronization, and headless widget rendering.

---

## 4. Benchmark Suite Architecture — `benches/clap_bench.rs`

Benchmarks measure throughput, parameter modulation overhead, neural inference across topologies, sample rate conversion, and CabSim convolution using [Criterion.rs](https://bheisler.github.io/criterion.rs/book/index.html).

### 4.1 Measured Execution Groups

1. **`CLAP_Infrastructure` (Zero-Inference Base Overheads):**
   - `Passthrough`: Baseline execution across block sizes (32, 64, 128, 256, 512, 1024 samples).
   - `ParamModulation`: Cost of handling continuous automation events queued at sub-buffer intervals.
   - `Bypass`: Latency-compensated bit-transparent dry path throughput.
2. **`CLAP_Inference` (Neural Model Processing):**
   - Architectural sweeps across `WaveNet_A1_Standard`, `WaveNet_A2_Slimmable`, and `LSTM`.
   - Sample rate conversions: 44.1 kHz (polyphase upsampling), 48.0 kHz (native), and 96.0 kHz (downsampling).
   - Quality modes: `Oversample_Off`, `2×`, `4×`, and `RenderMode_Offline_HQ`.
   - CabSim convolution: Partitioned time-domain SIMD FIR convolution (512-sample IR).
3. **`CLAP_WorstCase` (Pathological & Boundary Scenarios):**
   - `SmallBlocks`: Fixed overhead analysis on 16/32-sample buffers.
   - `DryDelay`: Pure delay line push/pop performance.
   - `EventFlood` / `EventFloodModel`: Dense parameter modulation up to 1 event per sample, evaluating sub-block splitting vs. vectorization.
   - `MonoDualMono`, `Crossfade`, `RingOut`, `ThreadHop`: Stereo divergence, crossfading ramps, ring-out tail drainage, and worker thread migration penalties.
4. **`CLAP_Evidence_InferenceE2E`:** End-to-end regression anchor matrix for continuous baseline tracking.

### 4.2 Benchmark Fixtures & Real-Time Isolation

*Source: [`benches/common.rs`](../benches/common.rs)*

- **Deterministic Fixtures:** Pre-validated via SHA-256 hashes prior to benchmark execution.
- **Pre-Warming:** Models are pre-warmed for 2048 samples prior to `b.iter()` to avoid cache-cold bias.
- **Zero Heap Allocations:** Iteration loops operate strictly on pre-allocated, 64-byte aligned buffers (`AlignedVec<f32>`).
- **CPU Core Isolation:** Benchmarks are pinned to isolated physical cores via `taskset` (see [§5.5](#55-deterministic-benchmark-execution--cpu-core-isolation-isolcpus--taskset)).

### 4.3 Performance Regression Gate & Baseline Discipline

[`utils/tests-performance-regression.sh`](../utils/tests-performance-regression.sh) executes the automated performance gate, delegating statistical interpretation to the unified `nam_perf_gate` binary:

- **Coverage Validation:** Asserts that all mandatory Criterion groups defined in [`benches/clap_bench.manifest`](../benches/clap_bench.manifest) are present.
- **Operating Modes:** `--check` (read-only verification) and `--bootstrap-baseline` (human operator baseline capture on calibrated hardware).
- **Governor Gating:** Fails closed if the benchmark CPU core governor is not locked to `performance`.
- **Baseline Durability:** Stored under `.performance-baselines/` and backed up to `~/.cache/nam-plug/perf-baselines/` during workspace bootstrap cleanups.

### 4.4 Engine DSP vs. CLAP Plugin Boundary Differentiation

The benchmark suite differentiates between core neural math and plugin host integration overhead:

| Architectural Layer     | Core Engine (`NeuralAmpModeler-rs`)                                   | CLAP Plugin (`NAM-Plug`)                                                                                 |
|:----------------------- |:--------------------------------------------------------------------- |:-------------------------------------------------------------------------------------------------------- |
| **Measurement Target**  | Raw DSP kernels (GEMM, Conv1D, WaveNet dilations, fast activations).  | Full CLAP plugin boundary, C ABI dispatch, buffer slicing, and audio/event routing.                      |
| **Memory Topology**     | Pre-allocated, contiguous, SIMD-aligned Rust slices.                  | Raw host pointer arrays (`*const *const f32`), non-contiguous channel buffers, dynamic `AudioPorts`.     |
| **Automation & Events** | Static parameters passed directly as scalar values to DSP kernels.    | Sample-accurate event demultiplexing, sub-block slicing, parameter smoothing ramps, and MIDI CC mapping. |
| **State & Lifecycle**   | Pure stateless or stateful mathematical models executed sequentially. | Lifecycle state transitions, dynamic delay compensation (PDC), bypass crossfading, CabSim ring-out.      |
| **Thread Topology**     | Deterministic single-thread execution in controlled test harnesses.   | Multi-thread worker migration (`ThreadHop`), evaluating L1/L2 cache invalidation and MXCSR restoration.  |

---

## 5. Execution Commands & Developer Workflow

> [!IMPORTANT]
> **Host Memory Protection (`CARGO_BUILD_JOBS=4`):**
> Always export `export CARGO_BUILD_JOBS=4` before running Cargo commands or test scripts to prevent compiler processes from saturating host RAM and causing OOM crashes.

All commands must be executed from inside `./NAM-Plug/`:

### 5.1 Verification Scripts (`utils/`)

| Script                                      | Scope & Operation                                                                                | Usage Context                              |
|:------------------------------------------- |:------------------------------------------------------------------------------------------------ |:------------------------------------------ |
| **`utils/lints.sh`**                        | Static analysis (`fmt`, `check`, `clippy -D warnings`, SPDX headers, static RT allocation scan). | Core continuous verification gate          |
| **`utils/test-scripts.sh`**                 | Aggregate runner for internal test scripts under `tests/scripts/`.                               | Invoked by `utils/lints.sh`                |
| **`utils/tests-quick.sh`**                  | 3-phase fast QA gate (debug structural, release parity + RT codegen, heap audit).                | First line of defense (local & pre-commit) |
| **`utils/tests-long.sh`**                   | 4-phase heavy stress suite (GC, teardown, RT priority/latency, Xvfb GUI).                        | Nightly & release certification (operator) |
| **`utils/tests-gui.sh`**                    | Headless Xvfb lifecycle, XEmbed window negotiation, and clipboard export testing.                | Invoked by Phase 4 of `tests-long.sh`      |
| **`utils/verify_rt_codegen.sh`**            | Disassembly inspection of `.so` (0 `malloc`, 0 `div`/`idiv`, bounded TLS).                       | Integrated in `tests-quick.sh` and release |
| **`utils/tests-performance-regression.sh`** | Criterion `clap_bench` regression gate over `nam_perf_gate`.                                     | Performance baseline verification          |
| **`utils/build-release.sh`**                | Multi-stage release builder with PGO, optional BOLT, RT codegen verification, and packaging.     | Official release pipeline                  |
| **`utils/finalize-release-receipt.sh`**     | Reconciles `release-receipt.json` with current long-audit JSONL data atomically.                 | Release packaging finalization             |

#### 5.1.1 Quick Suite Lifecycle (`utils/tests-quick.sh`)

`utils/tests-quick.sh` executes three distinct phases, generating `target/logs/quick-receipt.txt`:

1. **Phase 1: Structural (Debug):** Executes all unit and integration test targets with debug assertions enabled (enforcing execution of ≥350 tests).
2. **Phase 2: Release Verification (Release):** Builds the release `.so` and executes the CLAP × NAMCore parity oracle (`test_clap_parity_multi_rate`), oversampling parity, CabSim IR validation, and static RT codegen disassembly inspection.
3. **Phase 3: RT-Safety Heap Audit (Debug):** Validates zero audio-thread memory allocations via `--features testing,heap-audit`.

### 5.2 Direct Cargo Commands

```bash
# Set host concurrency guard
export CARGO_BUILD_JOBS=4

# 1. Type check tests and benchmarks
cargo check --tests --benches --features testing

# 2. Run standard unit and integration tests
cargo test --features testing

# 3. Run allocation audit RT-safety tests
cargo test --features testing,heap-audit --lib processor_heap_audit_test

# 4. Run property-based tests
cargo test --features testing --test clap_e2_proptest

# 5. Run Criterion benchmarks
cargo bench --features testing --bench clap_bench

# 6. Run CLAP × NAMCore parity oracle directly
NAM_REQUIRE_CPP_ORACLE=1 cargo test --features testing --release --test clap \
    test_clap_parity_multi_rate -- --ignored --nocapture
```

### 5.3 Isolated CI/CD Parity Oracle Resolution

When running in an isolated environment without a sibling checkout of `NeuralAmpModeler-rs`, the parity oracle resolves the NAMCore C++ render binary in the following priority order:

1. `NAM_CORE_RENDER_BIN` environment variable (authoritative contract).
2. Local build directory: `build/namcore_render`.
3. Linked library path: `neural_amp_modeler_rs::testing::fixtures::render_bin_path()`.
4. Workspace sibling checkout: `../NeuralAmpModeler-rs/build/namcore_render`.

For standalone CI/CD:

```bash
export CARGO_BUILD_JOBS=4
export NAM_CORE_RENDER_BIN=/opt/namcore/build/namcore_render/tools/render
export NAM_FIXTURES_DIR=/opt/namcore/models
NAM_REQUIRE_CPP_ORACLE=1 cargo test --features testing --release --test clap \
    test_clap_parity_multi_rate -- --ignored --nocapture
```

### 5.4 Release Receipt & Verification Gates (`build-release.sh`)

Production builds compile the **`dist`** profile (inherits `release`, PGO + optional BOLT, stripped, `panic = "unwind"`) into `~/.clap/nam_plug.clap`. Provenance is certified via `target/release-receipt.json`, written atomically via temporary file and rename.

#### Five Mandatory Release Gates

1. **Symbol & SONAME Validation:** Verifies export table hygiene.
2. **External `clap-validator`:** Validates protocol conformance against the distributed binary.
3. **NAMCore Float Parity:** Multi-rate parity, oversampling, and CabSim convolution against the C++ oracle.
4. **CabSim IR Artifact Test:** Confirms impulse responses alter output audio as expected.
5. **Performance Certification:** Invokes `nam_perf_guard certify` pinned to the benchmark core, verifying p99 execution stays below **85% of the audio block budget**.

#### 5.4.1 BOLT Win-Proof Ceremony — Opt-Out Architectural Decision

Unlike standalone host binaries, `NAM-Plug` does not include an in-tree `ab-opt-ceremony.sh` runner. Maintaining a dedicated PMU-driven A/B harness inside the plugin library introduces significant overhead relative to its gain. Therefore:

- **LLVM-BOLT is opt-out in `NAM-Plug`.** This is an intentional decision, not a regression: a missing `ab-opt-ceremony-receipt.json` must not be triaged as a failed ceremony.
- Passing the deadline envelope gate (`nam_perf_guard`) records `bolt_proof: "gate-pass-only"`, certifying real-time viability without claiming an unverified A/B win.
- An applied BOLT with neither an `ab-opt-ceremony:<id>` proof nor a passing gate stays a typed gap (`bolt_proof:missing_ceremony`) in every mode.

The receipt states the absence of proof instead of leaving it implicit:

- `optimizations.unproven_claims` is `["bolt_win"]` whenever `bolt_proof == "gate-pass-only"`, and `[]` when a real A/B receipt exists (or BOLT was not applied). It is a claim inventory, not a gate: `CERTIFIED` still means "the shipped bytes passed every gate", never "BOLT made them faster".
- The pipeline prints a `[UNPROVEN]` banner before exiting so the operator cannot read a green run as an A/B win.
- A missing `ab-opt-ceremony.sh` is therefore not a defect to "fix" by fabricating a proof; implementing the runner is an opt-in future enhancement.

#### 5.4.1.1 Measurement Host vs. Measurement Noise

`nam_perf_guard` reports two independent properties of a run:

| Field | Meaning | Effect on the gate |
| --- | --- | --- |
| `environment_stable` | Coefficient of variation ≤ 1.2 across the sampled series (noise/thermal). | `false` ⇒ `INCONCLUSIVE` (exit `2`). |
| `environment_degraded` + `environment_degraded_reasons` | The host was not the isolated-core policy in §5.4 (no `isolcpus`/`nohz_full`, shared IRQ affinity, non-RT scheduling, non-`performance` governor, boost enabled, or unpinned affinity). | Recorded only; the p99 evidence remains valid for *that* host. |

The receipt mirrors both fields under `performance_certification`, and the pipeline prints `[DEGRADED HOST]` with the typed reasons. Reasons are stable tokens such as `bench_core_not_isolated:8`, `irq_isolation_shared:8:35`, `sched_policy_not_realtime:SCHED_OTHER`, `governor_not_performance:powersave`, `governor_unknown`, `cpu_boost_enabled`, `boost_unknown`, `affinity_not_single_core:0-15`. A degraded host never promotes a `FAIL` to `PASS`; it only prevents a `PASS` from being read as a fully isolated measurement.

`perf-certification-report.json` uses `schema_version: "1.3"` and an ISO-8601 `timestamp_utc` (`YYYY-MM-DDTHH:MM:SSZ`), like the release receipt, so reports sort chronologically as text.

#### 5.4.1.2 External `clap-validator` Skips

Gate 2 records the validator outcome under `clap_validator` (`status`, `tests_run`, `passed`, `failed`, `skipped`, `skip_reasons`, `log_sha256`) and writes the full output to `target/logs/clap-validator.log`. Intentional skips are explicit evidence, not silence: the current artifact reports two `clap.note-ports` skips because the plugin does not implement that extension. A green run with skips is never presented as a zero-skip run.

#### 5.4.1.3 Long-Audit Artifact Exercise Coverage

`utils/tests-long.sh` stamps every phase with `artifact_exercised`. Phases that only run library-level (`--lib`) tests declare the artifact hash without `dlopen`-ing it, so phases 1, 2 and 4 are legitimately `false`; phase 3 (`RT Priority & Micro-Latency`) loads the shipped `.clap`. The finalizer summarizes this in the release receipt:

```json
"artifacts": {
  "long_audit_artifact_exercised": true,
  "long_audit_phases_exercising_artifact": ["phase3"],
  "long_audit_phases_not_exercising_artifact": ["phase1", "phase2", "phase4"]
}
```

This prevents "15 tests passed" from being read as "15 tests ran against the shipped shared object". Expanding dlopen coverage is a future epic, not a certificate requirement.

#### 5.4.1.4 Hermetic Targets vs. Local Iteration

`build-release.sh` wipes `target/pgo-build` and `target/pgo-clap` before every run so no stale object can be certified. For local iteration only, `NAM_PGO_REUSE_TARGET=1` reuses the previous target; the flag is ignored (with a warning) in strict or ceremony mode. The official `release-ceremony.sh` additionally runs `git clean -fdx`, so a certified run always compiles from scratch.

#### 5.4.2 Exit Codes: Preliminary Run × Release Ceremony

| Invocation                                        | Receipt status                           | Exit code |
| ------------------------------------------------- | ---------------------------------------- | --------- |
| `build-release.sh --strict-release` (preliminary) | any (`INCOMPLETE` is reported)           | `0`       |
| `build-release.sh --release-ceremony`             | `CERTIFIED`                              | `0`       |
| `build-release.sh --release-ceremony`             | `INCOMPLETE` or `FUNCTIONALLY_CERTIFIED` | `3`       |
| Fatal error (`die`)                               | —                                        | `1`       |

- In ceremony mode, exit `0` holds if and only if the receipt is `CERTIFIED`. Exit `3` is a dedicated code, distinct from `die` (`1`), and is printed with a `[CEREMONY-FAIL]` banner listing the skipped gates.
- The receipt and artifacts are written before the gate runs and remain in `target/` for diagnosis.
- The rule lives in `ceremony_exit_code` (`utils/lib/_lib.sh`) and is covered by `tests/scripts/test_ceremony_fail_closed.sh`.

#### 5.4.3 Certified Artifact Identity (F-CER-01)

The ceremony never recompiles after the long audit. The preliminary `--strict-release --install` run captures the installed bytes in `target/certified/nam_plug.clap` (plus `nam_plug.clap.sha256`). The `--release-ceremony` run then requires:

```text
sha256(long-audit receipt) == sha256(~/.clap/nam_plug.clap) == sha256(target/certified/nam_plug.clap)
```

Any divergence aborts with a `F-CER-01` diagnostic instead of rebuilding silently. Phases 2–4 are skipped, and the reused codegen/PGO evidence must match the preliminary `target/release-receipt.json`: the PGO log and workload receipt are bound by SHA, and the preliminary source must match the certified checkout (`provenance.git_commit == HEAD`, `git_dirty == false`, plus `Cargo.lock`, `rustc` and engine provenance), so an artifact built from commit A or a dirty tree cannot be certified as commit B. Because the installed artifact is the post-strip form of the BOLT output, the receipt records the canonical installed SHA plus `artifacts.pre_strip_artifact_sha256` (the codegen `.bolt.so` hash, populated in both preliminary and ceremony receipts); only the post-strip identity is compared with the long receipt. On success `artifacts.reused_certified_binary` is `true` and `certified_copy_sha256` matches `clap_installed_sha256`. If a ceremony cannot certify, Phases 6–7 are skipped so the last-known-good files in `$HOME` are not overwritten. Contract tests: `tests/scripts/test_ceremony_reuse_binary.sh` and `test_receipt_artifact_consistency.sh`.

#### 5.4.4 Ceremony Sequence, Receipt Roles & Invariants

The official ceremony is driven by `release-ceremony.sh` (orchestrating repository release scripts, without internal receipt logic). Phases run in this order from the repository root:

```text
[1] build-release.sh --strict-release --install           preliminary build (PGO + BOLT)
        -> target/release-receipt.json (preliminary), target/certified/nam_plug.clap(.sha256)
[2] tests-long.sh --strict-pre-release                     long audit (operator only)
        -> target/logs/long-audit-receipt.jsonl
[3] ab-opt-ceremony.sh --runs 3                            optional; not shipped in NAM-Plug (§5.4.1)
        -> target/logs/ab-opt-ceremony-receipt.json
[4] build-release.sh --strict-release --release-ceremony --install   final certification
        -> target/release-receipt.json (final); exit 0 only if CERTIFIED, otherwise exit 3
```

| Receipt / artifact                           | Written by        | Role                                                                                                                |
| -------------------------------------------- | ----------------- | ------------------------------------------------------------------------------------------------------------------- |
| `target/release-receipt.json` (preliminary)  | step 1            | Build evidence (PGO workload, codegen pre/post, source provenance). Input validated by step 4; may be `INCOMPLETE`. |
| `target/certified/nam_plug.clap` + `.sha256` | step 1            | Exact installed bytes captured for reuse. Never regenerated by step 4.                                              |
| `target/logs/long-audit-receipt.jsonl`       | step 2            | Binds the artifact SHA exercised by the long suite.                                                                 |
| `target/logs/ab-opt-ceremony-receipt.json`   | step 3 (optional) | BOLT win proof (`bolt_proof = ab-opt-ceremony:<id>`).                                                               |
| `target/release-receipt.json` (final)        | step 4            | Authoritative status, gates, and artifact identities of the shipped binary.                                         |

Invariants:

- **The binary is never rebuilt after certification.** Step 4 reuses `target/certified/nam_plug.clap`; a missing or divergent artifact aborts with `F-CER-01` (never a silent rebuild). The reuse path is explicit in the receipt: `artifacts.reused_certified_binary` is `true` only when the reuse guard passed, and `artifacts.certified_copy_sha256` equals `artifacts.clap_installed_sha256`.
- **Single identity chain:** `sha256(long receipt) == sha256(installed) == sha256(certified copy)`. The codegen `.bolt.so` hash is recorded separately in `artifacts.pre_strip_artifact_sha256`.
- **Exit-3 rule (§5.4.2):** in ceremony mode only `CERTIFIED` exits `0`; any other status exits `3`. Packaging (Phases 6–7) runs only for `CERTIFIED`, so the last-known-good files in `$HOME` survive a failed ceremony.
- **Provenance of the ceremony:** `provenance.ceremony` records `script_sha256` (SHA-256 of `release-ceremony.sh`) and the ordered `phases` it ran (for example `["build", "long", "final"]`). Manual `build-release.sh` runs, or a missing or malformed SHA, record `null`. The field is optional and additive: `schema_version` stays `1.1`.

---

### 5.5 Deterministic Benchmark Execution & CPU Core Isolation (`isolcpus` + `taskset`)

Micro-benchmarking real-time DSP requires strict environmental determinism. Without kernel-level core isolation, measurements are distorted by scheduler preemption, cross-core cache invalidation, frequency transitions, and interrupts.

#### Recommended Kernel Boot Parameters (`/etc/default/grub`)

```text
isolcpus=managed_irq,domain,8,9 nohz_full=8,9 rcu_nocbs=8,9 irqaffinity=0-7,10-15 nmi_watchdog=0 skew_tick=1 clocksource=tsc tsc=reliable
```

- `isolcpus=managed_irq,domain,8,9`: Removes isolated cores from scheduler load-balancing domains and managed interrupt distribution.
- `nohz_full=8,9`: Suspends periodic timer tick interrupts when a single pinned task executes.
- `rcu_nocbs=8,9`: Offloads RCU callback processing to housekeeping cores.
- `irqaffinity=0-7,10-15`: Directs hardware device interrupts to housekeeping cores.
- `nmi_watchdog=0`: Disables recurring hardware interruptions.
- `clocksource=tsc tsc=reliable`: Enforces invariant hardware RDTSC as the system clocksource.

> [!NOTE]
> **Clocksource Hygiene & Telemetry Overhead:**
> Systems falling back to HPET incur ~1.13 µs per checkpoint (~2.27 µs per audio block). Calibrated RDTSC executes in **17.16 ns/call** (~34 ns per audio block) — a **66× overhead reduction**. `NAM-Plug` calibrates RDTSC asynchronously on a helper thread during startup, incurring 0 ms blocking time on host initialization while guaranteeing sub-20ns telemetry.

#### Host Calibration Commands

```bash
# Take SMT sibling thread offline
echo 0 | sudo tee /sys/devices/system/cpu/cpu9/online

# Lock frequency governor to performance
echo performance | sudo tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor

# Disable CPU boost to ensure thermal repeatability
echo 0 | sudo tee /sys/devices/system/cpu/cpufreq/boost

# Pin benchmarks to isolated core 8
export CARGO_BUILD_JOBS=4
taskset -c 8 cargo bench --features testing --bench clap_bench
```

---

### 5.6 GUI Testing & Headless Verification (`xvfb-run`)

`NAM-Plug` tests the CLAP GUI extension (`PluginGui`), including embedded X11 window negotiation:

- **Interactive Desktop:** Tests run directly against the active X11 display (`DISPLAY=:0`), asserting strict XEmbed parentage.

- **Headless CI:** When `DISPLAY` is unset, GUI tests verify that `create(X11 embedded)` fails closed honestly without leaking resources.

- **Virtual Framebuffer:** To run embedded GUI tests in headless runners:

  ```bash
  export CARGO_BUILD_JOBS=4
  xvfb-run -a cargo test --features testing --lib gui_lifecycle_x11_embedded_set_parent_honest
  ```

### 5.7 GUI Open Latency & Platform Pre-Warming

The persistent GUI worker (`src/clap/gui/worker.rs`) pre-warms the Slint/winit platform on its own thread upon `gui.create()` (`GuiWorker::warm_up`). This ensures the host main thread avoids winit and OpenGL initialization latency during the first `set_parent`/`show` call.

- **Criterion:** With pre-warming active, the p95 of host-main-thread block time during `open_window` must stay **< 50 ms**.
- **Verification:** Run the plugin under a real host, collect `NAM-Plug: GUI open_window completed in <N> ms` logs, and verify p95 latency.

### 5.8 Host Memory Guards (OOM Containment)

Development systems with 16 GiB RAM can experience compiler-induced OOM crashes when running fat-LTO release test suites alongside IDE language servers (`rust-analyzer`). Protection is enforced via:

1. **Parallelism Cap:** `.cargo/config.toml` sets `jobs = 4` (override per run with `CARGO_BUILD_JOBS=4`).
2. **Cgroup Scope Containment:** Scripts re-exec via `utils/mem-guard.sh` within a transient systemd user scope (`MemoryHigh=5G`, `MemoryMax=6G`, `oom_score_adj=500`).
3. **Bench Profile Tuning:** `[profile.bench]` uses thin LTO and 16 codegen units to prevent memory spikes.
4. **IDE Process Protection:** Lowers the IDE's `oom_score_adj` to protect development environments.

### 5.9 Adaptive Rebuild Offload & Two-Phase NaN Containment

- **Offloaded Structural Refreshes:** Memory-intensive model reloading and oversampling filter reallocation run on the background `nam-model-loader` worker thread (`main_thread/model_loader.rs`), communicating with the main thread via lock-free atomic slots.
- **Two-Phase NaN Containment:** If non-finite audio samples are detected, the processor latches containment, emits a held dry block to prevent loudspeaker damage, resets state safely, and resumes clean wet processing.
- **Rollback Switches:** Synchronous legacy behavior can be forced via `NAM_SYNC_MODEL_LOAD=1` and `NAM_SYNC_ADAPTIVE_REBUILD=1` for A/B debugging.

### 5.10 Dense Automation Mitigation (Piecewise Gain Fusion)

When a host streams dense parameter automation (up to 1 event per sample), splitting the audio buffer at every event boundary destroys SIMD vectorization and increases CPU usage by >2×.

#### Solution: Piecewise Gain Fusion

- Smoothed gain parameters (`PARAM_INPUT_GAIN`, `PARAM_OUTPUT_GAIN`) no longer fragment inference sub-blocks. Only state-discontinuous events (noise gate threshold, bypass, oversampling, model swaps) create sub-block splits.
- Gain targets are replayed at their exact sample offsets through an analytic IIR ramp (`apply_gain_ramp_with_steps`). The gain stage is bit-identical to legacy splitting (ESR = 0), while keeping neural model inference fully vectorized.

#### Empirical Benchmark Comparison

| Model Fixture | Block Size | Event Density    | Baseline Ratio (Unmitigated) | Optimized Ratio (Fused Ramps) |
|:------------- |:---------- |:---------------- |:---------------------------- |:----------------------------- |
| WaveNet A1    | 64 samples | 64 events        | 2.57×                        | **1.57×**                     |
| WaveNet A1    | 64 samples | 1 event / sample | 2.52×                        | **1.55×**                     |
| LSTM          | 64 samples | 64 events        | 3.20×                        | **2.07×**                     |
| LSTM          | 512 sample | 1 event / sample | 3.22×                        | **2.00×**                     |

---

## 6. Quality Gates & Baseline Standards

| Metric / Test Gate            | Threshold / Constraint                               | Enforced In                                                             |
|:----------------------------- |:---------------------------------------------------- |:----------------------------------------------------------------------- |
| **CLAP vs. NAMCore Parity**   | ESR < 1e-8, SNR > 80 dB; 3 fixtures × 44.1/48/96 kHz | `clap_parity_multi_sr.rs` (Phase 2 of `tests-quick.sh`, release Gate 3) |
| **Artifact OS/CabSim Parity** | ESR < 1e-8, SNR > 80 dB @ 2×/4× and CabSim           | `test_clap_parity_oversample`, `test_clap_parity_cabsim` (Phase 2)      |
| **Bypass Transparency**       | Phase cancellation < −120 dBFS                       | `processor_bypass_test.rs`                                              |
| **RT Allocation Budget**      | Exactly 0 heap allocations during `process()`        | `verify_no_rt_alloc.sh` (static) & `alloc_audit.rs` (dynamic)           |
| **RT Codegen Invariants**     | 0 heap allocs, 0 core-loop divs, 0 core-loop TLS     | `verify_rt_codegen.sh` (static disassembly analysis)                    |
| **CLAP Event Handling**       | 0 panics or unhandled boundary conditions            | `clap_e2_proptest.rs`                                                   |

### 6.1 Reference Performance Baseline (Criterion Benchmark Matrix)

Measured on an isolated CPU core (`x86-64-v3` AVX2/FMA baseline, `performance` governor, core pinned via `taskset`):

#### Neural Architecture Inference (Live Mode, Native 48 kHz, Stereo)

| Topology Family          | Model Fixture             | Block 32 | Block 64 | Block 128 | Block 256 | Block 512 | Block 1024 | Steady ns/sample   |
|:------------------------ |:------------------------- |:-------- |:-------- |:--------- |:--------- |:--------- |:---------- |:------------------ |
| **WaveNet A1 Standard**  | `wavenet_a1_standard.nam` | 21.8 µs  | 42.3 µs  | 83.9 µs   | 167.8 µs  | 335.7 µs  | 671.6 µs   | **~655 ns/sample** |
| **WaveNet A2 Slimmable** | `a2_example.nam`          | 18.4 µs  | 34.4 µs  | 70.2 µs   | 138.6 µs  | 276.7 µs  | 553.4 µs   | **~540 ns/sample** |
| **LSTM 1×3**             | `lstm.nam`                | 2.6 µs   | 4.8 µs   | 9.2 µs    | 17.9 µs   | 35.4 µs   | 70.1 µs    | **~69 ns/sample**  |

#### CLAP Infrastructure & Host Processing Overhead

| Execution Group                         | Block 32 | Block 64 | Block 128 | Block 256 | Block 512 | Block 1024 | Unit Cost      |
|:--------------------------------------- |:-------- |:-------- |:--------- |:--------- |:--------- |:---------- |:-------------- |
| **Passthrough (Zero-Inference)**        | 474 ns   | 576 ns   | 841 ns    | 1.21 µs   | 2.01 µs   | 3.83 µs    | ~3.7 ns/sample |
| **ParamModulation (Active Automation)** | 690 ns   | 890 ns   | 1.23 µs   | 1.90 µs   | 3.17 µs   | 3.85 µs    | ~3.8 ns/sample |
| **Bypass (Latency-Compensated)**        | —        | 463 ns   | —         | —         | —         | —          | ~7.2 ns/sample |

#### Quality Modes, Sample Rates & CabSim Convolution (WaveNet A1, Block Size 64)

| Configuration / Mode                   | Mean Latency / Duration | Incremental Cost vs. Live Native 48k | Notes                                          |
|:-------------------------------------- |:----------------------- |:------------------------------------ |:---------------------------------------------- |
| **Native 48 kHz (Live, OS Off)**       | 41.9 µs                 | Baseline (1.00×)                     | Zero added latency                             |
| **Resample 44.1 kHz (Live, OS Off)**   | 52.5 µs                 | +10.6 µs (+25.3%)                    | Polyphase minimum-phase bandlimited FIR        |
| **Downsample 96.0 kHz (Live, OS Off)** | 23.7 µs                 | -18.2 µs (-43.4%)                    | 64 input samples = 32 internal DSP samples     |
| **Oversample 2× (Live)**               | 84.7 µs                 | +42.8 µs (+102%)                     | 2× internal neural iterations                  |
| **Oversample 4× (Live)**               | 167.3 µs                | +125.4 µs (+299%)                    | 4× internal neural iterations                  |
| **RenderMode Offline HQ (4×)**         | 168.0 µs                | +126.1 µs (+301%)                    | Deterministic HQ mastering mode                |
| **CabSim IR Convolution (512-sample)** | 42.6 µs                 | +1.2 µs (+2.8%)                      | Partitioned time-domain / SIMD FIR convolution |

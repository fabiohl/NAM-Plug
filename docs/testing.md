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
- **Static AST-Light Scanner ([`utils/verify_no_rt_alloc.sh`](../utils/verify_no_rt_alloc.sh) / [`utils/rt_alloc_scan.awk`](../utils/rt_alloc_scan.awk)):**
  Executes during static analysis (`lints.sh`). Scans `src/clap/processor/` sources, tracks brace scoping while ignoring comments and string literals, excludes authorized off-RT lifecycle hooks (`activate`, `deactivate`, panic handlers), and flags forbidden heap patterns (`Box::new`, `Vec::new`, `format!`, `Arc::new`, `HashMap`).
- **Static Machine-Code Codegen Guard ([`utils/verify_rt_codegen.sh`](../utils/verify_rt_codegen.sh)):**
  Executes during release verification (`tests-quick.sh` Phase 2) and packaging (`build-release.sh`). Disassembles the compiled `.so` using `objdump` and runs the unified filter ([`utils/dsp-hotpath-filter.py`](../utils/dsp-hotpath-filter.py)) with the repository symbol profile ([`utils/codegen-profile.json`](../utils/codegen-profile.json)) to verify:
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

### 4.3 Performance Certification Gate

Performance is certified against the canonical artifact by `nam_perf_guard certify --clap target/dist/nam_plug.clap`, invoked by the `perf` phase of `utils/tests-long.sh --artifact` (receipt `phase_id=phase4`; without `--artifact` the phase is recorded as the typed GAP `perf:requires_artifact`):

- **Benchmark core:** resolved by the precedence in [§7](#7-script-contract--automation-disciplines): explicit `NAM_BENCH_CORE` → isolated online core → first allowed CPU → unpinned (`GAP`).
- **Budget:** p99 block execution must stay below 85% of the audio block budget.
- **Coverage:** mandatory Criterion groups are declared in [`benches/clap_bench.manifest`](../benches/clap_bench.manifest).

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

> [!NOTE]
> **Host Memory Management:**
> On systems with constrained RAM, configure `CARGO_BUILD_JOBS` in your host shell profile (e.g. `~/.bashrc`) to prevent memory exhaustion during parallel codegen.

All commands must be executed from inside `./NAM-Plug/`:

### 5.1 Verification Scripts (`utils/`)

| Script                                      | Scope & Operation                                                                                | Usage Context                              |
|:------------------------------------------- |:------------------------------------------------------------------------------------------------ |:------------------------------------------ |
| **`utils/lints.sh`**                        | Static analysis (`fmt`, `check` matrix, `clippy -D warnings`, SPDX headers, static RT allocations; ~6s). | Core continuous verification gate          |
| **`utils/tests-quick.sh`**                  | Agile QA suite (≤ 5 min budget; `cargo test --features testing` + heap-audit unit gate).                 | First line of defense (local & pre-commit) |
| **`utils/tests-long.sh`**                   | Pre-release & nightly audit suite (GC, teardown, RT priority/latency, Xvfb GUI). Supports `--strict`, `--artifact`, and `--only`. | Nightly & release audit (operator)         |
| **`utils/verify_rt_codegen.sh`**            | Disassembly inspection of `.so` (0 `malloc`, 0 `div`/`idiv`, bounded TLS).                       | Invoked by `build-release.sh` (pre/post BOLT) |
| **`utils/build-release.sh`**                | Canonical release builder and receipt certifier (`--strict`, `--package`, `--install`, `--certify`). | Official release pipeline                  |
| **`utils/mod-update.sh`**                   | Manual toolchain and Cargo dependency upgrade utility (Class D).                                 | Dependency maintenance                     |

#### 5.1.1 Quick Suite Lifecycle (`utils/tests-quick.sh`)

`utils/tests-quick.sh` executes two distinct phases with a runtime budget of $\le 5$ minutes, writing detailed phase logs to `target/logs/quick-<phase>.log` and concluding with a canonical `RESULT: PASS` line:

1. **Phase 1: Tests (`tests`):** Executes all non-ignored unit and integration test targets under debug assertions (`cargo test --locked --features testing --no-fail-fast`, enforcing execution of ≥350 tests).
2. **Phase 2: RT-Safety Heap Audit (`heap-audit`):** Validates zero audio-thread memory allocations via `--features testing,heap-audit --lib processor_heap_audit_test`. Measured cost is 12.4% of total quick execution (below the 20% decision threshold), retaining this critical zero-allocation gate in the first-line discipline.

#### 5.1.2 Long Audit Lifecycle (`utils/tests-long.sh`)

The long suite uses the canonical phase names below; `utils/tests-long.sh --help` prints the same table. Each phase writes `target/logs/long-<name>.log`, and the typed receipt is `target/logs/long-audit-receipt.jsonl` (with `phase_ids` `phase1`–`phase5` kept as the receipt contract). Timeouts per command are calibrated to $\approx 2\times$ observed $p_{100}$ (overridable via `NAM_PHASE_TIMEOUT_S`):

| Receipt `phase_id` | Canonical phase | Command, Invariant & Timeout | Log |
|:------------------ |:--------------- |:---------------------------- |:--- |
| `phase1` | `catch-all` | `cargo test --release --features testing --no-fail-fast -- --include-ignored "${skips[@]}"` (timeout 500s; worst command $p_{100} \approx 230$s); runs all non-ignored and ignored tests except dedicated suites. | `target/logs/long-catch-all.log` |
| `phase2` | `gui` | `cargo test --features testing --release --lib "$gt" -- --ignored < /dev/null` per test under `xvfb-run` (timeout 300s; worst command $p_{100} \approx 120$s cold / 1.2s warm); emits `gui:xvfb_not_found` when `xvfb-run` is absent. | `target/logs/long-gui.log` |
| `phase3` | `artifact` | Parity and CabSim tests against a neutral release `.so`, or against `target/dist/nam_plug.clap` with `--artifact` (timeout 400s; worst command $p_{100} \approx 180$s); optional `clap-validator` gate. | `target/logs/long-artifact.log` |
| `phase4` | `perf` | `nam_perf_guard certify --clap target/dist/nam_plug.clap` (timeout 120s; worst command $p_{100} \approx 30$s); requires `--artifact`, otherwise `SKIPPED` with `perf:requires_artifact`. | `target/logs/long-perf.log` |
| `phase5` | `heap-audit` | GC swap/destroy, off-RT teardown, RT priority/latency, burst deadline and TLS heap-stability soak tests (timeout 500s; worst command $p_{100} \approx 215$s under release heap-audit LTO). | `target/logs/long-heap-audit.log` |

The script defines the skip list `SKIP=(gui_x11_ processor_drain_latency_test test_reset_wall_clock_time_within_contract test_structural_burst_p99_within_contract)` at the top. The `catch-all` phase passes these to Cargo via `--skip`, while the dedicated phases execute them with isolated environments: `gui` executes `gui_x11_` under `xvfb-run` (`< /dev/null`), and `heap-audit` executes the micro-latency and timing tests with `--test-threads=1`. The five GUI tests are `gui_x11_embedded_open_close_cycles_no_leak`, `gui_x11_lifecycle_embedded_set_parent_honest`, `gui_x11_lifecycle_hide_after_show_with_real_window`, `gui_x11_headless_clipboard_works` and `gui_x11_headless_floating_window_lifecycle`.

`--only <name>[,<name>]` accepts only canonical phase names. Unselected phases are still appended to the receipt as `SKIPPED`, and `target/long` is preserved for iterative reruns (it is removed only when `--only` is absent).

### 5.2 Direct Cargo Commands

```bash
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
export NAM_CORE_RENDER_BIN=/opt/namcore/build/namcore_render/tools/render
export NAM_FIXTURES_DIR=/opt/namcore/models
NAM_REQUIRE_CPP_ORACLE=1 cargo test --features testing --release --test clap \
    test_clap_parity_multi_rate -- --ignored --nocapture
```

### 5.4 Release Architecture & Verification Gates (`build-release.sh` & `plug_long_receipt certify`)

Production builds compile the **`dist`** profile (inherits `release`, PGO + optional BOLT, stripped, `panic = "unwind"`) into the canonical artifact directory `target/dist/nam_plug.clap`. Certification is decoupled from compilation and enforced via strongly typed receipts verified by the `plug_long_receipt certify` Rust binary.

#### The Three-Step Decoupled Release Chain

The canonical release workflow strictly decouples artifact compilation, long-audit stress testing, and final certification:

```text
[Step 1: Build]    utils/build-release.sh --strict [--package] [--install]
                   -> target/dist/nam_plug.clap (.sha256)
                   -> target/dist/build-receipt.json (preliminary build provenance)

[Step 2: Audit]    utils/tests-long.sh --strict --artifact
                   -> target/logs/long-audit-receipt.jsonl (stamps target/dist/nam_plug.clap SHA)

[Step 3: Certify]  utils/build-release.sh --certify
                   -> Invokes plug_long_receipt certify via cargo run (no artifact recompilation)
                   -> target/dist/release-receipt.json (authoritative CERTIFIED status)
```

| Step / Action | Script & Command | Role & Guarantees |
|:------------- |:---------------- |:----------------- |
| **1. Canonical Build** | `utils/build-release.sh --strict [--package] [--install]` | Compiles PGO + BOLT artifact into `target/dist/nam_plug.clap`. Packages distribution tarball and Flatpak extension if requested. Emits `target/dist/build-receipt.json`. |
| **2. Artifact Stress Audit** | `utils/tests-long.sh --strict --artifact` | Executes the full pre-release stress suite directly against the canonical artifact `target/dist/nam_plug.clap` via `dlopen`. Records results in `target/logs/long-audit-receipt.jsonl`. |
| **3. Final Certification** | `utils/build-release.sh --certify` | Validates cryptographic identity and release gates across receipts without recompilation. Emits final `target/dist/release-receipt.json`. |

#### Five Mandatory Release Gates

1. **Symbol & SONAME Validation:** Verifies export table hygiene.
2. **External `clap-validator`:** Validates protocol conformance against the distributed binary.
3. **NAMCore Float Parity:** Multi-rate parity, oversampling, and CabSim convolution against the C++ oracle.
4. **CabSim IR Artifact Test:** Confirms impulse responses alter output audio as expected.
5. **Performance Certification:** Invokes `nam_perf_guard certify` pinned to the benchmark core, verifying p99 execution stays below **85% of the audio block budget**.

#### 5.4.1 Certified Artifact Identity (F-CER-01)

The certification process (`build-release.sh --certify`) never recompiles after the long audit. The typed Rust certifier (`plug_long_receipt certify`) strictly enforces cryptographic parity across the artifact chain:

```text
sha256(long-audit receipt) == sha256(build receipt) == sha256(target/dist/nam_plug.clap)
```

Any divergence aborts immediately with a `F-CER-01` diagnostic error instead of rebuilding silently. The reused codegen/PGO evidence must match the preliminary `target/dist/build-receipt.json`: the PGO log and workload receipt are bound by SHA, and the source must match the certified checkout (`provenance.git_commit == HEAD`, `git_dirty == false`, plus `Cargo.lock`, `rustc` and engine provenance), ensuring that an artifact built from commit A or a dirty tree cannot be certified as commit B.

#### 5.4.2 LLVM-BOLT Win-Proof Policy — Opt-Out Architectural Decision

Unlike standalone host binaries, `NAM-Plug` does not include an in-tree `ab-opt-ceremony.sh` runner. Maintaining a dedicated PMU-driven A/B harness inside the plugin library introduces significant overhead relative to its gain. Therefore:

- **LLVM-BOLT is opt-out in `NAM-Plug`.** This is an intentional decision, not a regression: a missing A/B receipt must not be triaged as a failed ceremony.
- Passing the deadline envelope gate (`nam_perf_guard`) records `bolt_proof: "gate-pass-only"`, certifying real-time viability without claiming an unverified A/B win.
- An applied BOLT with neither an `ab-opt-ceremony:<id>` proof nor a passing gate stays a typed gap (`bolt_proof:missing_ceremony`) in every mode.
- The receipt states the absence of proof instead of leaving it implicit: `optimizations.unproven_claims` is `["bolt_win"]` whenever `bolt_proof == "gate-pass-only"`, and `[]` when a real A/B receipt exists (or BOLT was not applied). `CERTIFIED` certifies that the shipped bytes passed every functional and real-time gate.

#### 5.4.3 Measurement Host vs. Measurement Noise

`nam_perf_guard` reports two independent properties of a run:

| Field | Meaning | Effect on the gate |
|:----- |:------- |:------------------ |
| `environment_stable` | Coefficient of variation ≤ 1.2 across the sampled series (noise/thermal). | `false` ⇒ `INCONCLUSIVE` (exit `2`). |
| `environment_degraded` + `environment_degraded_reasons` | The host was not the isolated-core policy (no `isolcpus`/`nohz_full`, shared IRQ affinity, non-RT scheduling, non-`performance` governor, boost enabled, or unpinned affinity). | Recorded only; the p99 evidence remains valid for *that* host. |

The receipt mirrors both fields under `performance_certification`, and the pipeline prints `[DEGRADED HOST]` with the typed reasons. Reasons are stable tokens such as `bench_core_not_isolated:<N>`, `irq_isolation_shared:<N>`, `sched_policy_not_realtime:SCHED_OTHER`, `governor_not_performance:powersave`, `governor_unknown`, `cpu_boost_enabled`, `boost_unknown`, `affinity_not_single_core:0-15`. A degraded host never promotes a `FAIL` to `PASS`; it only prevents a `PASS` from being read as a fully isolated measurement.

`perf-certification-report.json` uses `schema_version: "1.3"` and an ISO-8601 `timestamp_utc` (`YYYY-MM-DDTHH:MM:SSZ`), like the release receipt, so reports sort chronologically as text.

#### 5.4.4 External `clap-validator` Skips

Gate 2 records the validator outcome under `clap_validator` (`status`, `tests_run`, `passed`, `failed`, `skipped`, `skip_reasons`, `log_sha256`) and writes the full output to `target/logs/clap-validator.log`. Intentional skips are explicit evidence, not silence: the current artifact reports two `clap.note-ports` skips because the plugin does not implement that extension. A green run with skips is never presented as a zero-skip run.

#### 5.4.5 Long-Audit Artifact Exercise Coverage

`utils/tests-long.sh` stamps every receipt line with `artifact_exercised`. With `--artifact`, the `artifact` phase (`phase3`) loads `target/dist/nam_plug.clap` via `dlopen` and the `perf` phase (`phase4`) certifies that same file, so both record `true`; `catch-all` (`phase1`), `gui` (`phase2`) and `heap-audit` (`phase5`) run library/harness tests and record `false`. Without `--artifact`, the artifact phase uses a neutral release `.so` and records `false`, while `perf` is `SKIPPED` with `perf:requires_artifact`. The certifier summarizes this in the release receipt:

```json
"artifacts": {
  "long_audit_artifact_exercised": true,
  "long_audit_phases_exercising_artifact": ["phase3", "phase4"],
  "long_audit_phases_not_exercising_artifact": ["phase1", "phase2", "phase5"]
}
```

This prevents "15 tests passed" from being read as "15 tests ran against the shipped shared object". Expanding dlopen coverage is a future epic, not a certificate requirement.

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
taskset -c 8 cargo bench --features testing --bench clap_bench
```

---

### 5.6 GUI Testing & Headless Verification (`xvfb-run`)

`NAM-Plug` tests the CLAP GUI extension (`PluginGui`), including embedded X11 window negotiation:

- **Interactive Desktop:** Tests run directly against the active X11 display (`DISPLAY=:0`), asserting strict XEmbed parentage.

- **Headless CI:** When `DISPLAY` is unset, GUI tests verify that `create(X11 embedded)` fails closed honestly without leaking resources.

- **Virtual Framebuffer:** The five GUI tests share the `gui_x11_` prefix, so the long suite's `gui` phase runs all of them in one filtered invocation:

  ```bash
  xvfb-run -a cargo test --features testing --release --lib gui_x11_ -- --ignored --test-threads=1
  ```

  The tests are `gui_x11_embedded_open_close_cycles_no_leak`, `gui_x11_lifecycle_embedded_set_parent_honest`, `gui_x11_lifecycle_hide_after_show_with_real_window`, `gui_x11_headless_clipboard_works` and `gui_x11_headless_floating_window_lifecycle`.

### 5.7 GUI Open Latency & Platform Pre-Warming

The persistent GUI worker (`src/clap/gui/worker.rs`) pre-warms the Slint/winit platform on its own thread upon `gui.create()` (`GuiWorker::warm_up`). This ensures the host main thread avoids winit and OpenGL initialization latency during the first `set_parent`/`show` call.

- **Criterion:** With pre-warming active, the p95 of host-main-thread block time during `open_window` must stay **< 50 ms**.
- **Verification:** Run the plugin under a real host, collect `NAM-Plug: GUI open_window completed in <N> ms` logs, and verify p95 latency.

### 5.8 Host Memory Guards (OOM Containment)

Development systems with 16 GiB RAM can experience compiler-induced OOM crashes when running fat-LTO release test suites alongside IDE language servers (`rust-analyzer`). Protection is enforced via:

1. **Parallelism Cap:** Controlled via host environment (e.g., `CARGO_BUILD_JOBS` in `~/.bashrc` on memory-constrained systems).
2. **Cgroup Scope Containment:** Heavy out-of-band builds can be wrapped with host-level cgroup containment provided by the operator host tooling (transient systemd user scope).
3. **Bench Profile Tuning:** `[profile.bench]` uses thin LTO and 16 codegen units to prevent memory spikes.
4. **IDE Process Protection:** Lowers the IDE's `oom_score_adj` to protect development environments.

### 5.9 Adaptive Rebuild Offload & Two-Phase NaN Containment

- **Offloaded Structural Refreshes:** Memory-intensive model reloading and oversampling filter reallocation run on the background `nam-model-loader` worker thread (`main_thread/model_loader.rs`), communicating with the main thread via lock-free atomic slots.
- **Two-Phase NaN Containment:** If non-finite audio samples are detected, the processor latches containment, emits a held dry block to prevent loudspeaker damage, resets state safely, and resumes clean wet processing.

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

---

## 7. Script Contract & Automation Disciplines

All automation scripts in `utils/` adhere to a lean, predictable execution contract designed for deterministic automation, reproducible continuous integration, and transparent developer workflows.

### 7.1 Script Classes & Error Handling Policies

Each script strictly adheres to one of four architectural classes:

| Class | Typical Scripts | Error Handling Policy | Architectural Rationale |
|:----- |:--------------- |:--------------------- |:----------------------- |
| **Class A — Interactive** | `lints.sh`, `tests-quick.sh` | `set -euo pipefail`; aborts on first failure; `ERR` trap emits `RESULT: FAIL` | Real-time developer loops. Immediate failure surfaces bugs fast and conserves developer cycles. |
| **Class B — Decoupled Audit** | `tests-long.sh` | `set -o pipefail` (no `-e`, no `-u`); each phase records status independently; collects all failures; exit `1` at end if any phase failed | Nightly and pre-release audits. Collects all phase evidence without aborting prematurely. |
| **Class C — Build** | `build-release.sh` | `set -euo pipefail`; aborts on first failure | Build pipelines where intermediate failure invalidates downstream artifacts. |
| **Class D — Manual Tool** | `mod-update.sh`, `verify_rt_codegen.sh` | `set -euo pipefail` | Targeted utilities executed under direct operator supervision. |

### 7.2 Output Protocol & Exit Codes

Script output follows a structured stream suitable for automated parsing:

1. **Phase Announcement:**
   ```text
   PHASE: <phase_name>
   ```
2. **Phase Completion:**
   ```text
   PHASE: <phase_name> PASS log=<path>
   PHASE: <phase_name> FAIL rc=<exit_code> log=<path>
   PHASE: <phase_name> GAP <technical_reason>
   ```
3. **Unified Final Result Line:**
   Every script terminates by emitting exactly one final summary line:
   ```text
   RESULT: PASS|PASS_WITH_GAPS|FAIL (<seconds>s) fails=<list|none> gaps=<list|none>
   ```
4. **Exit Codes:**
   - `0`: Complete success (`PASS`), or success with documented gaps (`PASS_WITH_GAPS`) when not in strict mode.
   - `1`: Execution failure (`FAIL`), or unfulfilled `GAP` when invoked with `--strict`.
   - `2`: Syntax error, unknown option, or missing precondition.

Detailed execution logs are written to `target/logs/<script>-<phase>.log` via `tee`. The long suite uses the canonical `target/logs/long-<name>.log` names listed in [§5.1.2](#512-long-audit-lifecycle-utilstests-longsh). The canonical `PHASE:`/`RESULT:` lines are also appended to `target/logs/summary.txt` and published by the local `.github/actions/qa-summary` composite action; the ARM CI job now invokes `utils/lints.sh` instead of an inline clippy command.

### 7.3 Core Automation Principles

- **No Tests of Test Tooling:** Test suites validate CLAP plugin interfaces, DSP kernels, neural inference, and GUI event loops. Meta-tests that verify auxiliary scripts, receipt JSON schemas, or internal QA tools are forbidden. Tooling integrity is guaranteed by strict typing, minimal surface area, and routine execution.
- **Quick Never Provisions Infrastructure:** `utils/tests-quick.sh` operates entirely on already-available fixtures and pre-built tooling. It never provisions external dependencies or triggers out-of-band builds. Missing optional tooling or fixtures gracefully emits a typed `GAP`.
- **Long Collects All Failures:** `utils/tests-long.sh` executes all configured preflights and stress phases to completion, aggregating passes, gaps, and failures into a final diagnostic summary without early termination.
- **Convention Over Parameterization:** All scripts default to host capabilities (all online cores, zero artificial thermal cooldown, unpinned execution). Environment variables only exist to override defaults when explicitly required.

### 7.4 Canonical Command-Line Flags

Only canonical flags are supported across scripts (no legacy aliases):

| Script | Supported Flags | Notes |
|:------ |:--------------- |:----- |
| **All scripts** | `-h`, `--help` | Displays concise usage and exits 0. |
| `lints.sh` | None | Strict zero-flag policy; formatting is applied in-place during local development and verified via `CI=true` on CI. |
| `tests-quick.sh` | None | Fast first-line discipline; no allow-gaps switch and no simulation mode. |
| `tests-long.sh` | `--strict`, `--only <name>[,<name>]`, `--artifact` | `--strict` promotes any `GAP` to `FAIL` (exit 1); `--only` runs only the named canonical phases, records the others as `SKIPPED`, and preserves `target/long`; `--artifact` tests canonical `target/dist/nam_plug.clap`. |
| `build-release.sh` | `--strict`, `--package`, `--install`, `--certify` | `--package` builds tarball and Flatpak; `--install` installs to `~/.clap/`; `--certify` validates receipts without recompiling. |

### 7.5 Canonical Environment Variables

#### Interface Variables (User & CI Control)

| Variable | Default | Scope | Purpose & Semantics |
|:-------- |:------- |:----- |:------------------- |
| `CI` | Unset | CI Runner | When `true`, instructs `lints.sh` to execute `cargo fmt --check` instead of modifying files in-place. |
| `NAM_BENCH_CORE` | `auto` | Benchmark | Core pinning: explicit CPU ID, isolated core if online, first allowed CPU, or unpinned fallback (`GAP`). |
| `NAM_PHASE_TIMEOUT_S` | Per-phase default | `tests-long.sh` | Overrides timeout duration across all audit phases for emergency debugging. |
| `NAM_GUI_TIMEOUT_MS` | `15000` in `tests-long.sh`; `4000` in the harness | GUI Testing | Timeout in milliseconds for headless Xvfb window and lifecycle assertions. |

#### Harness Variables (Internal Script → Rust Communication)

| Variable | Configured By | Read By | Purpose |
|:-------- |:------------- |:------- |:------- |
| `NAM_RUN_ID` | `utils/_lib.sh` | Test harnesses | Unique timestamped run identifier for logging and receipt isolation. |
| `NAM_REQUIRE_CPP_ORACLE` | `tests-long.sh --strict` | Parity suites | Enforces hard failure if the C++ reference render binary is unavailable. |
| `NAM_RT_STRICT` | `tests-long.sh --strict` | RT deadline tests | Rejects latency outliers and deadline budget misses fail-closed. |
| `CLAP_PLUGIN_UNDER_TEST` | `tests-long.sh --artifact` | Long audit harnesses | Specifies path to the canonical compiled `.clap` binary under test. |
| `NAM_GUI_DEDICATED_X11` | `tests-long.sh` (`gui` phase) | GUI lifecycle tests | Requests a dedicated X11 display path for the headless GUI tests. |
| `NAM_CORE_RENDER_BIN` | `_lib.sh` / preflights | C++ Parity tests | Absolute path to the compiled upstream C++ `render` CLI binary. |
| `NAM_FIXTURES_DIR` | `_lib.sh` / preflights | Model loaders | Explicit path to test model fixtures directory. |

### 7.6 Measured Durations & Budgets

Execution times measured on standard development hardware (16-core, SSD):

| Discipline / Script | Measured Duration | Budget Ceiling | Notes |
|:------------------- |:----------------- |:-------------- |:----- |
| `utils/lints.sh` | ~8s (warm cache, 2026-10-27) to ~45s (clean build) | $\le 2$ min | Covers fmt, check (3 permutations), clippy (`-D warnings`), doc, SPDX, policies, and RT allocation verification. |
| `utils/tests-quick.sh` | ~36s (warm cache, 2026-10-27) to ~90s | $\le 5$ min | Covers structural unit/integration tests, C++ NAMCore parity, oversampling parity, CabSim IR, and static RT codegen. |
| `utils/tests-long.sh` | ~8–12 min (full suite) | $\le 15$ min | GC swaps, off-RT drain, RT priority/latency with loaded `.clap`, and headless Xvfb GUI. |
| `utils/build-release.sh` | ~2–4 min (full PGO/BOLT) | $\le 8$ min | PGO profiling workload, BOLT optimization, and canonical packaging. |

#### QA footprint (métrica F-2, 2026-10-27)

`wc -l`: shell **1.257**, py/awk **670**, Rust QA (`src/bin/*`) **3.024** →
**4.951 linhas** de QA total. O quick foi medido em 36 s com a fase
`heap-audit` mantida (decisão S9-T05).

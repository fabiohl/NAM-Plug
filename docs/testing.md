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

| Feature Flag     | Description                                                                                                                                                                                   | Test / Bench Usage Scope                                                                              |
|:---------------- |:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |:----------------------------------------------------------------------------------------------------- |
| **`default`**    | Enables `dual-mono` by default (`default = ["dual-mono"]`).                                                                                                                                   | Standard production builds and default compilation checks.                                            |
| **`dual-mono`**  | Enables dual-channel L/R processing via `NeuralAmpModeler-rs/dual-mono` and CLAP Stereo port configuration announcement. Without this feature, only the Mono port configuration is announced. | Default feature enabled across standard builds and test runs.                                         |
| **`testing`**    | Enables engine test utilities, generators, and fixture resolution (`NeuralAmpModeler-rs/testing`).                                                                                            | Mandatory feature flag when running `NAM-Plug` integration tests, receipts, and Criterion benchmarks. |
| **`heap-audit`** | Intercepts memory allocations via `CountingAllocator` (`NeuralAmpModeler-rs/heap-audit`).                                                                                                     | Used by RT-safety tests to ensure zero heap allocations occur on the audio thread during `process()`. |
| **`gui-x11`**    | Display-server lifecycle tests (requires dedicated Xvfb/X11 server).                                                                                                                          | Off by default; exercised in dedicated GUI integration test phases.                                   |

### 1.1 Dependency Graph & Feature Unification Behavior

- **Engine Feature Persistence**: In [`Cargo.toml`](../Cargo.toml), the dependency `NeuralAmpModeler-rs` is declared with `default-features = false, features = ["dual-mono"]`. Consequently, even when `NAM-Plug` is compiled with `--no-default-features`, the underlying engine dependency still resolves with its `dual-mono` feature active.
- **Development Feature Unification**: `Cargo.toml` declares `dev-dependencies.nam-plug = { path = ".", features = ["testing"] }` and `dev-dependencies.NeuralAmpModeler-rs = { version = "2.0.0", features = ["testing"] }`. When building targets that include dev-dependencies (such as `cargo check --all-targets --no-default-features`), Cargo unifies features across the target graph, activating `testing`. Conversely, library-only checks without dev-dependencies (`cargo check --lib --no-default-features`, as verified in `utils/lints.sh`) keep `testing` and `gui-x11` inactive.

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
  Executes during artifact certification (`tests-long.sh` `artifact` phase) and packaging (`build-release.sh`). Operates under the canonical Class D lifecycle (installing `trap_fail`, emitting `PHASE: codegen`, generating `target/logs/verify_rt_codegen-status.tsv`, and concluding via `finish`). Disassembles the compiled `.so` using `objdump` and runs the unified filter ([`utils/dsp-hotpath-filter.py`](../utils/dsp-hotpath-filter.py)) against the repository symbol profile ([`utils/codegen-profile.json`](../utils/codegen-profile.json)), which declares 7 hot-path symbol targets (`process`, `process_sub_block`, `process_sub_block_chunked`, `process_crossfade_sub_block`, `process_tail_drain`, …) with per-symbol gates — zero allocations everywhere, zero `div`/TLS in the sub-block loops, and small explicit budgets for the `process()` entry. It verifies:
  - **Fail-closed disassembly:** Every invocation of `objdump` (full or per-symbol) validates exit status and rejects empty/truncated output.
  - **Zero heap calls:** No `malloc`, `free`, or `realloc` instructions.
  - **Zero integer divisions:** No `div` or `idiv` in the sub-block DSP loops (bounded budget on the `process()` entry).
  - **Bounded TLS access:** No `__tls_get_addr` calls in the sub-block loops (bounded budget on the `process()` entry).
  - **Post-BOLT resilience:** Disassembles symbol-by-symbol and normalizes `.bolt.org.text*` sections to prevent false positives from dead bytes left by binary reordering.

---

## 3. Automated Test Inventory

Test targets reside in [`tests/`](../tests/) and test modules within `src/`.

### 3.1 Main Harness Entrypoint

*Source: [`tests/clap.rs`](../tests/clap.rs)*

`tests/clap.rs` acts as the primary harness, binding test configuration, shared allocators, and submodules under `tests/clap/`.

### 3.2 Modular Sub-Suites

*Directory: [`tests/clap/`](../tests/clap/)*

| Submodule / Test Target                                                               | Scope & Invariants Verified                                                                                                                                        |
|:------------------------------------------------------------------------------------- |:------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **[`artifact_validator`](../tests/clap/artifact_validator.rs)**                       | Dynamic artifact discovery, `dlopen` loading, and SHA-256 fingerprint verification.                                                                                |
| **[`clap_cabsim_ir`](../tests/clap/clap_cabsim_ir.rs)**                               | Release-artifact CabSim test: a loaded impulse response measurably transforms the output.                                                                          |
| **[`clap_cross_machine`](../tests/clap/clap_cross_machine.rs)**                       | Cross-machine determinism, buffer chunk boundary invariance, and numerical float consistency.                                                                      |
| **[`clap_lifecycle_test`](../tests/clap/clap_lifecycle_test.rs)**                     | Lifecycle transitions (`init`, `activate`, `deactivate`, `reset`), host log quiescence coordinator (`HostLogQuiescence`), and dynamic audio configuration changes. |
| **[`clap_multi_instance`](../tests/clap/clap_multi_instance.rs)**                     | Multi-instance concurrency, thread safety, and RT-priority behavior across parallel instances.                                                                     |
| **[`clap_multi_instance_isolation`](../tests/clap/clap_multi_instance_isolation.rs)** | Per-instance state, diagnostics, and logging-sink isolation.                                                                                                       |
| **[`clap_parity_multi_sr`](../tests/clap/clap_parity_multi_sr.rs)**                   | CLAP × NAMCore C++ oracle parity across topologies and rates (44.1, 48, 96 kHz), oversampling, and CabSim IR path.                                                 |
| **[`clap_state_migration`](../tests/clap/clap_state_migration.rs)**                   | State serialization, parameter persistence, backward compatibility, and corrupt payload recovery.                                                                  |
| **[`tail_semantics`](../tests/clap/tail_semantics.rs)**                               | CLAP tail extension semantics, CabSim ring-out decay, and zero-allocation tail drain.                                                                              |

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

Additional artifact gates verified in the `artifact` phase (`phase3`):

- **Oversampling and CabSim parity:** see [§3.2.2](#322-artifact-level-oversampling-and-cabsim-parity).
- **External `clap-validator`:** version pinned in [`variables.env`](../variables.env) (`CLAP_VALIDATOR_VERSION`, currently `0.4.1`); the only expected skips are the `clap.note-ports` tests, because the plugin does not implement that extension (see [§5.4.4](#544-external-clap-validator-skips)).

All configurations pass the uniform gate `ESR < 1e-8, SNR > 80 dB`. The definitive certified artifact hash is recorded in `target/dist/release-receipt.json`.

#### 3.2.2 Artifact-Level Oversampling and CabSim Parity

- **Oversampling Parity (`test_clap_parity_oversample`):**
  Tests the release `.so` at 48 kHz with `OversampleFactor::X2` and `X4`. The reference feeds the native C++ oracle the identical half-band upsampled sequence produced by the plugin, then decimates the oracle output using the shared `OversampleEngine`.
  *Reference measurement:* X2 ESR ≈ 8.43e-12 / SNR ≈ 110.7 dB; X4 ESR ≈ 7.99e-12 / SNR ≈ 111.0 dB.
- **CabSim Convolution Parity (`test_clap_parity_cabsim`):**
  Convolves the artifact's dry model output with an impulse response using an independent direct time-domain FIR, comparing it against the artifact's internal convolution output (`max_frames_count = 1024`).
  *Reference measurement:* ESR ≈ 9.95e-14 / SNR ≈ 130.0 dB.

---

### 3.3 Additional Automated Test Suites

- **Regression Containment ([`tests/clap_e0_containment_test.rs`](../tests/clap_e0_containment_test.rs)):** Guards against architectural regressions (CabSim participation, parameter fidelity, reset semantics, sample rate negotiation, corrupt state recovery).
- **Host Quiescence & Teardown Safety ([`tests/clap_quiescence_test.rs`](../tests/clap_quiescence_test.rs)):** Verifies host call quiescence (`HostCallQuiescence`), ensuring teardown waits for in-flight callbacks and rejects subsequent dispatches to eliminate Use-After-Free (UAF). Uses explicit synchronization to prove a real in-flight host call finished before `destroy()` returned. The teardown follows the quarantine policy: after the grace-period diagnostic the wait remains, and plugin resources are never freed while the host may still access them (see [architecture §5.10](./architecture.md#510-host-log--host-call-quiescence-and-teardown-protocol)). The RT heap-audit region pre-allocates `AudioPorts`/`EventBuffer` outside the counted section.
- **Stereo CabSim Asymmetry ([`tests/clap_cabsim_stereo_asymmetric_test.rs`](../tests/clap_cabsim_stereo_asymmetric_test.rs)):** Proves that the dual-mono `CabSimPair` processes L and R independently (no channel duplication or crosstalk).
- **Property-Based Testing ([`tests/clap_e2_proptest.rs`](../tests/clap_e2_proptest.rs)):** Uses `proptest` to fuzz buffer lengths, parameter sequences, and event queues.
- **Processor Bypass Test ([`tests/processor_bypass_test.rs`](../tests/processor_bypass_test.rs)):** Verifies bit-transparent phase cancellation (< −120 dBFS) and smooth 64-sample crossfades.
- **Fault Containment & Poisoning ([`src/clap/processor_poisoning_test.rs`](../src/clap/processor_poisoning_test.rs)):** Ensures audio-thread panics latch `poisoned = true`, silence output via an $O(1)$ zero-alloc branch, raise `RT_STATUS_PROCESSOR_POISONED`, and safely trigger host restarts without crashing the DAW.
- **Fixtures Freshness Gate ([`tests/fixtures_freshness.rs`](../tests/fixtures_freshness.rs)):** Validates schema versions and cryptographic hashes of test models and audio fixtures.
- **Runtime Heap Audit ([`tests/heap_audit.rs`](../tests/heap_audit.rs)):** Enforces zero dynamic memory allocations during active processing.
- **Slint UI View Model, Widgets & Telemetry ([`tests/slint_view_model_test.rs`](../tests/slint_view_model_test.rs), [`tests/slint_widgets_test.rs`](../tests/slint_widgets_test.rs), [`tests/slint_telemetry_test.rs`](../tests/slint_telemetry_test.rs), [`tests/slint_alert_pill_test.rs`](../tests/slint_alert_pill_test.rs)):** Verifies declarative Slint view models, VU meter ballistics and synchronization, status-bar telemetry formatting, the alert pill, and headless widget rendering.

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
   - `MonoDualMono`, `Crossfade`, `RingOut`, `RoundTripIPC`: Stereo divergence, continuous 64-sample crossfade ramps (rearmed each block), CabSim tail-drain batch cycle with a deterministic dense decaying 512-sample IR, and multi-thread host worker round-trip IPC dispatch (allocation-free).
   - **Measurement criteria:** the crossfade benchmark validates outside the timer that the bypass target really alternates and that every block is processed/rewritten (NaN sentinel replaced by finite samples); the ring-out preflight accumulates effective drain energy and asserts a non-trivial tail. `stalls`/`lost_cycles` count **stalled drain blocks** (blocks with no effective tail energy), never raw CPU cycles; the report prints `drain_energy` and `stalls` per block size and requires zero stalls.
4. **`CLAP_Evidence_InferenceE2E`:** End-to-end regression anchor matrix for continuous baseline tracking.

### 4.2 Benchmark Fixtures & Real-Time Isolation

*Source: [`benches/common.rs`](../benches/common.rs)*

- **Deterministic Fixtures:** Pre-validated via SHA-256 hashes prior to benchmark execution.
- **Pre-Warming:** Models are pre-warmed for 2048 samples prior to `b.iter()` to avoid cache-cold bias.
- **Zero Heap Allocations:** Iteration loops operate strictly on pre-allocated, 64-byte aligned buffers (`AlignedVec<f32>`).
- **CPU Core Isolation:** `nam_perf_guard` resolves and then **applies** the affinity in-process with `sched_setaffinity`, verifying the observed core with `sched_getcpu` (see [§5.5](#55-deterministic-benchmark-execution--cpu-core-isolation-isolcpus--taskset)). Shell scripts never invoke `taskset` literally.

### 4.3 Performance Certification Gate

Performance is certified against the canonical artifact by `nam_perf_guard certify --clap target/dist/nam_plug.clap`, invoked by the `perf` phase of `utils/tests-long.sh --artifact` (receipt `phase_id=phase4`; without `--artifact` the phase is recorded as the typed GAP `perf:requires_artifact`):

- **Benchmark core:** resolved by the precedence in [§7](#7-script-contract--automation-disciplines): explicit `NAM_BENCH_CORE` → isolated online core → first allowed CPU → unpinned (`GAP`); then applied and verified in-process.
- **Paired reference:** certification uses `target/logs/perf-reference.json` when present, otherwise the previous `target/logs/perf-certification-report.json` as the rolling paired series. The reference must describe the same artifact SHA-256; a mismatch is a fatal error. The very first run has no series and is the typed GAP `perf:reference_missing`, never a PASS.
- **Report:** written to `target/logs/perf-certification-report.json`; a write failure aborts with exit 1 instead of silently losing evidence.
- **Budget:** p99 block execution must stay below 85% of the audio block budget (absolute real-time overload threshold).
- **Tail Certification (p99 vs Reference):** Relative regression checks compare p99 latency against the paired reference series (with p50 retained as informative). If a reference report lacks p99 statistics (e.g. legacy report), the comparison reports `INCONCLUSIVE` rather than PASS or FAIL.
- **Benchmark Groups vs. Shipped Artifact Certification:** Criterion benchmark groups (`cargo bench --bench clap_bench`) run under `[profile.bench]` (thin LTO, 16 CGU) to protect development machines from memory exhaustion (OOM), serving as developer orientation. Formal performance certification applies strictly to the shipped `.clap` artifact built under `[profile.dist]` (fat LTO, single CGU) and evaluated by `nam_perf_guard certify`. Declared in [`benches/clap_bench.manifest`](../benches/clap_bench.manifest) as operator diagnostic manifest.

### 4.4 Engine DSP vs. CLAP Plugin Boundary Differentiation

The benchmark suite differentiates between core neural math and plugin host integration overhead:

| Architectural Layer     | Core Engine (`NeuralAmpModeler-rs`)                                   | CLAP Plugin (`NAM-Plug`)                                                                                                  |
|:----------------------- |:--------------------------------------------------------------------- |:------------------------------------------------------------------------------------------------------------------------- |
| **Measurement Target**  | Raw DSP kernels (GEMM, Conv1D, WaveNet dilations, fast activations).  | Full CLAP plugin boundary, C ABI dispatch, buffer slicing, and audio/event routing.                                       |
| **Memory Topology**     | Pre-allocated, contiguous, SIMD-aligned Rust slices.                  | Raw host pointer arrays (`*const *const f32`), non-contiguous channel buffers, dynamic `AudioPorts`.                      |
| **Automation & Events** | Static parameters passed directly as scalar values to DSP kernels.    | Sample-accurate event demultiplexing, sub-block slicing, parameter smoothing ramps, and MIDI CC mapping.                  |
| **State & Lifecycle**   | Pure stateless or stateful mathematical models executed sequentially. | Lifecycle state transitions, dynamic delay compensation (PDC), bypass crossfading, CabSim ring-out.                       |
| **Thread Topology**     | Deterministic single-thread execution in controlled test harnesses.   | Multi-thread host worker dispatch (`RoundTripIPC`), evaluating channel synchronization and thread-pool dispatch overhead. |

---

## 5. Execution Commands & Developer Workflow

> [!NOTE]
> **Host Memory Management:**
> On systems with constrained RAM, configure `CARGO_BUILD_JOBS` in your host shell profile (e.g. `~/.bashrc`) to prevent memory exhaustion during parallel codegen.

All commands must be executed from inside `./NAM-Plug/`:

### 5.1 Verification Scripts (`utils/`)

| Script                           | Scope & Operation                                                                                                                        | Usage Context                                 |
|:-------------------------------- |:---------------------------------------------------------------------------------------------------------------------------------------- |:--------------------------------------------- |
| **`utils/lints.sh`**             | Static analysis (`fmt`, `check`/`clippy -D warnings` matrix, `cross_aarch64_check`, doc, SPDX headers, policies, static RT allocations). | Core continuous verification gate             |
| **`utils/tests-quick.sh`**       | Agile QA suite (≤ 5 min budget; `cargo test --features testing` + heap-audit unit gate).                                                 | First line of defense (local \& pre-commit)   |
| **`utils/tests-long.sh`**        | Pre-release \& nightly audit suite (GC, teardown, RT priority/latency, Xvfb GUI). Supports `--strict`, `--artifact`, and `--only`.       | Nightly \& release audit (operator)           |
| **`utils/ab-opt-ceremony.sh`**   | A/B optimization ceremony (`plain` vs. `pgo` vs. `pgo+bolt`); flags `--runs N` (≥ 3), `--blocks N`, `--no-pgo`, `--no-bolt`.             | Before a release build (see §5.4.2)           |
| **`utils/verify_rt_codegen.sh`** | Disassembly inspection of `.so` (0 `malloc`, 0 `div`/`idiv`, bounded TLS).                                                               | Invoked by `build-release.sh` (pre/post BOLT) |
| **`utils/build-release.sh`**     | Canonical release builder and receipt certifier (`--strict`, `--package`, `--certify`, `--install` with `--certify`).                    | Official release pipeline                     |
| **`utils/mod-update.sh`**        | Manual toolchain and Cargo dependency upgrade utility (Class D).                                                                         | Dependency maintenance                        |

#### 5.1.1 Quick Suite Lifecycle (`utils/tests-quick.sh`)

`utils/tests-quick.sh` executes two distinct phases with a runtime budget of $\le 5$ minutes, writing detailed phase logs to `target/logs/quick-<phase>.log` and concluding with a canonical `RESULT: PASS` line:

1. **Phase 1: Tests (`tests`):** Executes all non-ignored unit and integration test targets under debug assertions (`cargo test --locked --features testing --no-fail-fast`, enforcing execution of $\ge 400$ tests via `assert_ran_tests "$CUR_LOG" 400`, with measured baseline: x86=453, aarch64=441). Parity with C++ NAMCore, CabSim IR, oversampling, and codegen gates run exclusively in the long audit (`artifact` phase) and `build-release.sh`.
2. **Phase 2: RT-Safety Heap Audit (`heap-audit`):** Validates zero audio-thread memory allocations via `--features testing,heap-audit --lib processor_heap_audit_test`. Measured execution cost is ~12% of total quick execution, retaining this critical zero-allocation gate in the first-line discipline.

#### 5.1.2 Long Audit Lifecycle (`utils/tests-long.sh`)

The long suite uses the canonical phase names below; `utils/tests-long.sh --help` prints the same table. Each phase writes `target/logs/long-<name>.log`, and the typed receipt is staged at `target/logs/long-audit-receipt.<NAM_RUN_ID>.partial.jsonl` then published atomically as `target/logs/long-audit-receipt.jsonl` only after `summary`+`validate` succeed on a full run (with `phase_ids` `phase1`–`phase6` kept as the receipt contract). Timeouts per command are calibrated to $\approx 2\times$ observed $p_{100}$ (overridable via `NAM_PHASE_TIMEOUT_S`):

| Receipt `phase_id` | Canonical phase | Command, Invariant & Timeout                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    | Log                               |
|:------------------ |:--------------- |:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |:--------------------------------- |
| `phase1`           | `catch-all`     | `cargo test --locked --release --features testing --no-run` first under its own 1200s build guard (outside `NAM_PHASE_TIMEOUT_S`), then `cargo test --locked --release --features testing --no-fail-fast --lib --bins --tests` (execution timeout 300s; 485 tests executed). Only non-ignored tests run; every `#[ignore]` suite has a dedicated phase (`artifact`, `perf`, `gui`, `heap-audit`, `rt-heap`), so nothing is orphaned and the build no longer consumes the test timeout.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          | `target/logs/long-catch-all.log`  |
| `phase3`           | `artifact`      | Parity and CabSim tests executed immediately after `catch-all` (sharing the `--features testing` Cargo fingerprint without recompilation) with `--exact` against a neutral release `.so` (or `target/dist/nam_plug.clap` with `--artifact`) via `run_named` (timeout 400s; 14 subcases + `clap-validator`). Oracle-backed parity (`test_clap_parity_multi_rate`, `test_clap_parity_oversample`) runs only when `NAM_CORE_RENDER_BIN` or `build/namcore_render` is executable — then `NAM_REQUIRE_CPP_ORACLE=1` is exported under `--strict`; otherwise the phase records the typed `clap_parity:missing_cpp_oracle` GAP but still runs the oracle-free CabSim tests (`test_clap_parity_cabsim`, `test_cabsim_ir_changes_audio_release_artifact`), so `--strict` fails cleanly on the GAP instead of a contradictory `rc=101` panic. Each invocation writes `target/logs/long-artifact-<nome>.log` validated with `ran_tests`; optional `clap-validator` gate. On failure, `close_phase` prints the last 40 log lines to stderr. | `target/logs/long-artifact.log`   |
| `phase4`           | `perf`          | `nam_perf_guard certify --clap target/dist/nam_plug.clap --reference <paired-series> --out target/logs/perf-certification-report.json` (timeout 120s; worst command $p_{100} \approx 30$s); reuses `--features testing`; requires `--artifact`. When `target/logs/perf-reference.json` is absent or from an earlier artifact SHA-256, `tests-long.sh` automatically captures a baseline reference series for the current `.clap` artifact before running the paired certification pass.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         | `target/logs/long-perf.log`       |
| `phase2`           | `gui`           | Individual named tests executed with `--exact` under `xvfb-run` via `run_named` (timeout 300s; 5 tests; worst command $p_{100} \approx 120$s cold / 1.2s warm) under `--features testing,gui-x11`; each invocation writes `target/logs/long-gui-<nome>.log` validated with `ran_tests`; emits `gui:xvfb_not_found` when `xvfb-run` is absent.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | `target/logs/long-gui.log`        |
| `phase5`           | `heap-audit`    | Deterministic GC swap/destroy, off-RT teardown, RT-priority stress and TLS heap-stability soak tests under `--features testing,heap-audit` (timeout 500s; 505 subcases; worst command $p_{100} \approx 215$s under release heap-audit LTO). Each filter executes via `run_heap_test` writing a sublog `long-heap-<test>.log` verified with `ran_tests` fail-closed. The receipt declares the real subcase cardinality (including the 500 proptest cases). This phase never measures time thresholds, so it is hosted-safe.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | `target/logs/long-heap-audit.log` |
| `phase6`           | `rt-heap`       | Operator-only time-threshold certification (`test_structural_burst_p99_within_contract`, `processor_drain_latency_test` [automation burst p50=214ns / p99=279ns < 1.0µs; scalar saturation p50=2,501ns / p99=2,630ns < 13.3µs], `test_reset_wall_clock_time_within_contract` [4 subcases total]) under `--features testing,heap-audit` on a pinned core; never selected by the hosted workflow because hosted runners are not a comparable speed gate.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          | `target/logs/long-rt-heap.log`    |

Dedicated suites are selected by Cargo features instead of a shell skip list: the five `gui_x11_*` tests require `gui-x11`, and the micro-latency/timing tests (`processor_drain_latency_test`, `test_reset_wall_clock_time_within_contract`, `test_structural_burst_p99_within_contract`) require `heap-audit`. The catch-all phase compiles neither feature and never passes `--include-ignored`, so it cannot run them without a display or pinned core; the dedicated phases enable the corresponding feature explicitly and own every `#[ignore]` target. Time-threshold tests belong exclusively to `rt-heap` (operator-only); `heap-audit` contains only deterministic, hosted-safe cases.

`--only <name>[,<name>]` accepts only canonical phase names. Unselected phases are still appended to the receipt as `NOT_RUN` (with typed GAP), and `target/long` is preserved for iterative reruns (it is removed only when `--only` is absent). A partial run keeps its run-scoped staging receipt `target/logs/long-audit-receipt.<NAM_RUN_ID>.partial.jsonl`, emits `PHASE: phase-selection PASS only-subset:<fase>+<fase>`, records `scope=partial` in the summary and carrier (without counting as a GAP), and never deletes or replaces `target/logs/long-audit-receipt.jsonl` from a previous full run; a failed full run likewise keeps only its `.partial` for diagnosis. `target/dist`, logs and receipts are never removed. Certification (`utils/build-release.sh --certify`) strictly rejects partial scope. `RESULT:` lists are always comma-separated with no spaces (`fails=a,b gaps=c scope=partial`), matching the typed carrier.

**Absolute gain observability:** the parity matrix normalizes the comparison with the model's loudness calibration (`input_mult_adj`, `output_mult_adj`). To keep a gain or metadata regression visible, every matrix row logs `Absolute gain: oracle_rms=… artifact_rms=… ratio=±… dB` alongside both multipliers. The ratio is diagnostic only; the ESR/SNR gates are unchanged (measured cross-implementation floor above).

**Receipt test cardinality:** the downstream long script passes `append --tests-executed` explicitly, declaring subcases that a single Cargo invocation hides: the parity matrix contributes 9 (3 fixtures × 3 rates), oversampling contributes 2, and the GC property soak contributes 500 proptest cases; every other invocation contributes 1. The engine log parser is not modified.

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

#### 5.3.1 Hosted CI Jobs and Engine Source

- **`ci.yml`:** lints and quick run as separate jobs (`x86_64-linux-lints`, `x86_64-linux-quick`, `linux-arm64-lints`, `linux-arm64-quick`) plus `msrv` (which checks `--locked --all-targets --all-features`), so a lint failure cannot hide the functional suite. The ARM64 lint job is blocking; the ARM64 quick job is `continue-on-error` (experimental) until a native aarch64 execution is approved. Sibling checkout ref is pinned deterministically via [`variables.env`](../variables.env) (`ENGINE_COMMIT`). Each job, including `msrv`, writes `target/logs/ci-provenance.log` recording package version, git SHA, compiled engine provenance resolved from `Cargo.lock` (`engine_type`, `engine_version`, `engine_checksum`) and auxiliary sibling engine status (`auxiliary_engine`), uploading `target/logs/**` with `if: always()`. The MSRV job sets `RUSTUP_TOOLCHAIN=1.99.0` so the repository's `rust-toolchain.toml` cannot silently select stable. Its cargo-check step uses the canonical `utils/_lib.sh` lifecycle (`lib_init`, `run_step`, `finish`), so the five-column `target/logs/msrv-status.tsv` carrier (`msrv`, verdict, gaps, fails, `scope=full`) and the single `RESULT:` line are produced by the same code path as every other discipline. Every `Load environment variables` step also fails closed if `ENGINE_COMMIT` is empty after sourcing [`variables.env`](../variables.env), preventing `actions/checkout` from silently falling back to a branch head; provenance steps resolve the sibling through `$GITHUB_WORKSPACE/NeuralAmpModeler-rs` instead of a relative path.
- **Engine Provenance & Auxiliary Checkout:** `NeuralAmpModeler-rs` is compiled from its locked crates.io package release (`version = "2.0.0"` in `Cargo.lock`), ensuring deterministic builds independent of sibling checkout branches. The sibling repository `../NeuralAmpModeler-rs` is retained strictly as an auxiliary checkout providing the C++ parity oracle (`namcore_render`), reference fixtures, and fallback models pursuant to §5.3, pinned by commit in [`variables.env`](../variables.env). If the auxiliary checkout is absent, `auxiliary_engine=GAP:sibling_absent` is logged while the compiled engine provenance remains intact.
- **`long.yml`:** `workflow_dispatch` only, operator-triggered, **zero inputs**: the GitHub "Run workflow" button always uses the single recommended configuration. `timeout-minutes: 120`. Hosted runs select the fixed `--only catch-all,artifact,gui,heap-audit`; the `perf` and `rt-heap` timing phases are never selected because a hosted runner is not a comparable speed gate (`heap-audit` is deterministic and therefore hosted-safe). The hosted build is never strict; the operator release chain owns `--strict`. The runner provisions BOLT (`llvm-bolt`), `clap-validator` (pinned via `variables.env`), and the C++ parity oracle (by running `utils/setup-third-party.sh` and `utils/ensure_namcore_render.sh` inside the `NeuralAmpModeler-rs` checkout and exporting `NAM_CORE_RENDER_BIN`), so the `artifact` phase executes the full CLAP × NAMCore matrix without GAPs. Receipt and log upload enforces `if-no-files-found: error`. Certification and certification rehearsal are not performed in this workflow; the operator runs `build-release.sh --certify` on the isolated host. `clap-validator` is pinned by URL and SHA-256 in [`variables.env`](../variables.env) and the archive is verified with `sha256sum -c` before extraction.
- **Fidelity & Parity Ownership:** `docs/quality-contract.json` and `utils/quality-dashboard.sh` live in the upstream engine repository (`NeuralAmpModeler-rs`), not in `NAM-Plug`. Downstream fidelity is proven by the parity suites executed in `utils/tests-long.sh` (`catch-all` and `artifact` phases), which compare `f32` against the in-process `f64` oracle and, when `NAM_CORE_RENDER_BIN` is provisioned, against the C++ `NAMCore` oracle. `--strict` (which promotes GAPs to failures) is reserved for the operator's calibrated host; on the hosted runner the PGO/BOLT build remains the canonical artifact, while timing-sensitive phases stay typed GAPs rather than false failures.

### 5.4 Release Architecture & Verification Gates (`build-release.sh` & `plug_long_receipt certify`)

Production builds compile the **`dist`** profile (inherits `release`, PGO + optional BOLT, stripped, `panic = "unwind"`) into the canonical artifact directory `target/dist/nam_plug.clap`. Certification is decoupled from compilation and enforced via strongly typed receipts verified by the `plug_long_receipt certify` Rust binary.

#### The Three-Step Decoupled Release Chain

The canonical release workflow strictly decouples artifact compilation, long-audit stress testing, and final certification:

```text
[Step 0: Ceremony] utils/ab-opt-ceremony.sh --runs 3
                   -> target/logs/ab-opt-ceremony-receipt.json (empirical PGO / BOLT / PLAIN verdict)

[Step 1: Build]    utils/build-release.sh --strict [--package]
                   -> target/dist/nam_plug.clap (.sha256)
                   -> target/dist/build-receipt.json (preliminary build provenance)

[Step 2: Audit]    utils/tests-long.sh --strict --artifact
                   -> target/logs/long-audit-receipt.jsonl (stamps target/dist/nam_plug.clap SHA)

[Step 3: Certify]  utils/build-release.sh --strict --certify [--install]
                   -> Invokes plug_long_receipt certify (no artifact recompilation)
                   -> target/dist/release-receipt.json (authoritative CERTIFIED status)
                   -> with --install, only after CERTIFIED: plug_long_receipt verify-release,
                      atomic copy to ~/.clap/nam_plug.clap, Flatpak bundle if declared
```

| Step / Action                | Script & Command                                        | Role & Guarantees                                                                                                                                                                                                                                                                               |
|:---------------------------- |:------------------------------------------------------- |:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **0. A/B Ceremony**          | `utils/ab-opt-ceremony.sh --runs 3`                     | Benchmarks `plain`, `pgo`, and `pgo+bolt` across `wavenet_a1`, `wavenet_a2`, and `lstm` on an isolated core, emitting `target/logs/ab-opt-ceremony-receipt.json`.                                                                                                                               |
| **1. Canonical Build**       | `utils/build-release.sh --strict [--package]`           | Compiles `target/dist/nam_plug.clap` according to the ceremony verdict (`PGO+BOLT`, `PGO-ONLY`, or `UNOPTIMIZED` → `fallback_plain`). Packages distribution tarball and Flatpak extension if requested. Emits `target/dist/build-receipt.json`. Never installs.                                 |
| **2. Artifact Stress Audit** | `utils/tests-long.sh --strict --artifact`               | Executes the full pre-release stress suite (1,014 test subcases across 6 phases) directly against `target/dist/nam_plug.clap` via `dlopen`. Records results in `target/logs/long-audit-receipt.jsonl`.                                                                                          |
| **3. Final Certification**   | `utils/build-release.sh --strict --certify [--install]` | Validates cryptographic identity and release gates across receipts without recompilation. Emits final `target/dist/release-receipt.json`. `--install` (valid only with `--certify`) installs the verified artifact to `~/.clap/` after `certify PASS`, emitting `PHASE: install_artifact PASS`. |

#### Five Mandatory Release Gates

1. **Symbol & SONAME Validation:** Verifies export table hygiene.
2. **External `clap-validator`:** Validates protocol conformance against the distributed binary.
3. **NAMCore Float Parity:** Multi-rate parity, oversampling, and CabSim convolution against the C++ oracle.
4. **CabSim IR Artifact Test:** Confirms impulse responses alter output audio as expected.
5. **Performance Certification:** Invokes `nam_perf_guard certify` with affinity applied and verified in-process, a paired reference series, and no `environment_degraded` reason; p99 execution must stay below **85% of the audio block budget**.

- **`build-receipt.json` Schema & Gap Reconciliation:** `build-release.sh` uses `plug_long_receipt build-status --receipt target/dist/build-receipt.json --run-id $NAM_RUN_ID` to strictly validate schema conformance (`serde(deny_unknown_fields)`), verify `run_id` matching, and reconcile any multiline gaps into the build carrier fail-closed.
- **Certified Artifact Identity (F-CER-01):** The certification process (`build-release.sh --certify`) never recompiles after the long audit. The typed Rust certifier (`plug_long_receipt certify`) strictly enforces cryptographic parity across the artifact chain:

```text
sha256(long-audit receipt) == sha256(build receipt) == sha256(target/dist/nam_plug.clap)
```

Any divergence aborts immediately with a `F-CER-01` diagnostic error instead of rebuilding silently. The reused codegen/PGO evidence must match the preliminary `target/dist/build-receipt.json`: the PGO log and workload receipt are bound by SHA, and the source must match the certified checkout (`provenance.git_commit == HEAD`, `git_dirty == false`, plus `Cargo.lock`, `rustc` and engine provenance), ensuring that an artifact built from commit A or a dirty tree cannot be certified as commit B.

#### 5.4.2 A/B Optimization Ceremony & LLVM-BOLT Promotion Policy

The A/B optimization ceremony (`utils/ab-opt-ceremony.sh` and `plug_ab_opt compare`) evaluates DSP cycles across canonical topologies (`wavenet_a1`, `wavenet_a2`, `lstm`) over at least 3 measured runs. The promotion rule operates on median `p99` cycles per topology with an explicit 1.0% margin (`PROMOTION_MARGIN_PCT = 1.0%`):

- **`PGO-ONLY`**: Satisfied when, for **all** topologies $t$:
  $$\text{median}(p99_{\text{pgo}}[t]) \le \text{median}(p99_{\text{plain}}[t]) \times (1 - 0.010)$$
- **`PGO+BOLT`**: Satisfied when `PGO-ONLY` holds **and**, for **all** topologies $t$:
  $$\text{median}(p99_{\text{bolt}}[t]) \le \text{median}(p99_{\text{pgo}}[t]) \times (1 - 0.010)$$

##### Hardware LBR Requirements and Profiling Integrity

LLVM BOLT requires hardware branch sampling via Intel Last Branch Records (LBR, `perf record -j any,u` or `-b`).

- **Intel Hardware**: LBR captures exact basic-block branch transitions, enabling accurate profile-guided basic block layout.
- **AMD Hardware (Zen 1–4) & Virtualization**: Hardware LBR is unavailable (`perf record -j any,u` fails with `not supported`). In fallback sampling mode (`cycles:u`), perf cannot trace branch history, causing LLVM BOLT to operate with control-flow graph (CFG) discontinuity.
- **Automated Guard**: `ab-opt-ceremony.sh` actively probes for LBR support (`perf record -j any,u -o /dev/null -- true`). When LBR is absent (`LBR=0`), BOLT optimization is automatically bypassed, emitting gap `bolt_lbr_unsupported`.

##### Promotion Fallback Invariants & Measured Ceremony Baseline

1. `build-release.sh` detects `PGO+BOLT`, `PGO-ONLY`, or `UNOPTIMIZED` automatically via `plug_long_receipt ceremony-proof`.
2. **`PGO-ONLY` Fallback**: When `PGO-ONLY` is proven ($\ge 1.0\%$ win across all topologies) but BOLT does not beat PGO by $\ge 1.0\%$ (`verdict_reason=bolt_margin_not_met`) or LBR is absent: `build-release.sh` sets `bolt_status=not_proven` and `bolt_proof=fallback_pgo_only` in the build receipt, promoting the PGO-optimized binary.
3. **`PLAIN-ONLY` Fallback (`UNOPTIMIZED`)**: When PGO does not beat the plain release build by $\ge 1.0\%$ across all three topologies (`verdict=UNOPTIMIZED`, `verdict_reason=pgo_margin_not_met`), `build-release.sh` skips PGO/BOLT and promotes the deterministic plain release artifact (`bolt_status=not_proven`, `bolt_proof=fallback_plain`, `pgo_status=skipped_unproven`, `unproven_claims=["pgo_win", "bolt_win"]`).
4. **Fail-Closed BOLT Policy**: An applied BOLT binary (`bolt_status == "applied"`) without a valid ceremony receipt records a typed GAP (`bolt_not_proven`) and is strictly rejected under `--strict`. `release-ceremony` (`evaluate_certification`) strictly rejects `unverified` BOLT builds and dirty checkouts.

Measured A/B optimization ceremony baseline on isolated core 8 (AMD Ryzen 7 5700U, `performance` governor, 64-sample block @ 48 kHz, 3 runs):

| Topology     | Plain Median `p99` (cycles / µs) | PGO Median `p99` (cycles / µs) | $\Delta$ PGO vs. Plain | Promotion Gate ($\le -1.0\%$) |
|:------------ |:-------------------------------- |:------------------------------ |:---------------------- |:----------------------------- |
| `wavenet_a1` | 315,900 cyc (~175.9 µs)          | 315,162 cyc (~175.5 µs)        | `-0.23%`               | Not met (within 1.0% margin)  |
| `wavenet_a2` | 191,700 cyc (~106.7 µs)          | 190,764 cyc (~106.1 µs)        | `-0.49%`               | Not met (within 1.0% margin)  |
| `lstm`       | 50,130 cyc (~27.9 µs)            | 47,052 cyc (~26.2 µs)          | `-6.14%`               | Met ($\ge 1.0\%$ improvement) |

Because `wavenet_a1` and `wavenet_a2` are compute-bound inside hand-unrolled AVX2/FMA SIMD kernels (`sum0..sum3`), PGO yields `< 0.5%` delta and hardware LBR is unavailable on AMD Zen (`bolt_lbr_unsupported`), resulting in `VERDICT: UNOPTIMIZED` (`fallback_plain`).

#### 5.4.3 Measurement Host vs. Measurement Noise

`nam_perf_guard` applies in-process real-time hardening (`mlockall(MCL_CURRENT | MCL_FUTURE)`, 64 KiB stack prefaulting, `SCHED_FIFO` priority 80 promotion, and `sched_setaffinity` to the isolated benchmark core) before sampling, and reports two independent properties of a run:

| Field                                                   | Meaning                                                                                                                                                                                                                                     | Effect on the gate                                                                                                                |
|:------------------------------------------------------- |:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |:--------------------------------------------------------------------------------------------------------------------------------- |
| `environment_stable`                                    | Coefficient of variation ≤ 1.2 across the sampled series (noise/thermal).                                                                                                                                                                   | `false` ⇒ `INCONCLUSIVE` (exit `2`).                                                                                              |
| `environment_degraded` + `environment_degraded_reasons` | The host was not the isolated-core policy (no `isolcpus`/`nohz_full`, shared effective IRQ affinity, non-RT scheduling, non-`performance` governor, affinity not applied/observed on the selected core).                                    | `true` ⇒ `INCONCLUSIVE` (exit `2`); certification is blocked.                                                                     |
| paired reference series                                 | `--reference` report supplied to `nam_perf_guard certify`. Validates p99 latency against `--p99-ratio-max` (default 1.25) or within `--p99-noise-floor-us` (default 25.0 µs) to prevent false positives from microsecond-level host jitter. | Absent ⇒ `INCONCLUSIVE` (exit `2`); `tests-long.sh` bootstraps `perf-reference.json` automatically on the first pass when absent. |

The receipt mirrors both fields under `performance_certification`, and the pipeline prints `[DEGRADED HOST]` with the typed reasons. Reasons are stable tokens such as `bench_core_not_isolated:<N>`, `irq_isolation_shared:<N>:<count>` (evaluated against kernel `effective_affinity_list` so managed multi-queue PCI-MSIX masks that route away from the isolated core do not false-positive), `sched_policy_not_realtime:SCHED_OTHER`, `governor_not_performance:powersave`, `governor_unknown`, `affinity_apply_failed:<detail>`, `affinity_observed_mismatch:<requested>:<observed>`. A degraded host blocks certification instead of promoting the run to `PASS`.

`perf-certification-report.json` is written to `target/logs/` and uses `schema_version: "1.4"` with the optional `run_id`, the requested/applied/observed affinity fields and an ISO-8601 `timestamp_utc` (`YYYY-MM-DDTHH:MM:SSZ`), like the release receipt, so reports sort chronologically as text.

#### 5.4.4 External `clap-validator` Skips

Gate 2 records the validator outcome under `clap_validator` (`status`, `tests_run`, `passed`, `failed`, `skipped`, `skip_reasons`, `log_sha256`) and writes the full output to `target/logs/clap-validator.log`. Intentional skips are explicit evidence, not silence: the current artifact reports two `clap.note-ports` skips because the plugin does not implement that extension. A green run with skips is never presented as a zero-skip run.

#### 5.4.5 Long-Audit Artifact Exercise Coverage

`utils/tests-long.sh` stamps every receipt line with `artifact_exercised`. With `--artifact`, the `artifact` phase (`phase3`) loads `target/dist/nam_plug.clap` via `dlopen` and the `perf` phase (`phase4`) certifies that same file, so both record `true`; `catch-all` (`phase1`), `gui` (`phase2`), `heap-audit` (`phase5`) and `rt-heap` (`phase6`) run library/harness tests and record `false`. Without `--artifact`, the artifact phase uses a neutral release `.so` and records `false`, while `perf` is recorded with status `SKIP_CAPABILITY` and GAP `perf:requires_artifact`. The certifier summarizes this in the release receipt:

```json
"artifacts": {
  "long_audit_artifact_exercised": true,
  "long_audit_phases_exercising_artifact": ["phase3", "phase4"],
  "long_audit_phases_not_exercising_artifact": ["phase1", "phase2", "phase5", "phase6"]
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

- **Virtual Framebuffer:** The five GUI tests are executed individually in the long suite's `gui` phase with `--exact` via `run_named` under `xvfb-run`, verifying `ran_tests` on each per-invocation log `long-gui-<nome>.log`:

  ```bash
  xvfb-run -a cargo test --features testing,gui-x11 --release --lib <exact_test_id> -- --ignored --exact
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

- Smoothed gain parameters (`PARAM_INPUT_GAIN`, `PARAM_OUTPUT_GAIN`, and live calibration smoothers `smoother_cal_in` / `smoother_cal_out` driven by `PARAM_CALIBRATE_INPUT`, `PARAM_INPUT_CALIBRATION_LEVEL`, `PARAM_OUTPUT_MODE`) no longer fragment inference sub-blocks. Only state-discontinuous events (noise gate threshold, bypass, oversampling, model swaps) create sub-block splits.
- End-to-end calibration parity across `Raw` / `Normalized` / `Calibrated`, extreme dBu ranges, pre-nonlinearity saturation, gate/CabSim isolation, multi-instance independence, and `v0`/`v1`/`v2` state migration is verified in `tests/calibration_modes_test.rs`.
- Optional `Musical` noise gate parity against the C++ `AudioDSPTools/dsp/NoiseGate` reference across `44.1 / 48 / 96 kHz`, pre-calibration detector placement, post-model / pre-CabSim IR natural decay, stereo channel independence, `StateEnvelope` migration, and RT zero-allocation (`heap-audit`) is verified in `tests/musical_gate_parity_test.rs`.
- Single exclusive cabinet slot (`.wav` UPOLS IR and 1-in/1-out `.nam`/`.namb` neural cabinet models), fail-closed post-stack `head` and non-1x1 rejection leaving amp/cabinet intact, amp calibration / `IrLevelPolicy` isolation, C++/`f64` oracle parity (`Linear`, `Sequential`), `PARAM_CAB_BYPASS` latency-aligned 64-sample crossfade and steady bypass across `44.1 / 48 / 96 kHz` + unaligned blocks, `v0`/`v1`/`v2` state roundtrip, and zero RT heap allocations (`heap-audit`) are verified in `tests/cabinet_slot_and_bypass_test.rs`.
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

| Metric / Test Gate            | Threshold / Constraint                               | Enforced In                                                                     |
|:----------------------------- |:---------------------------------------------------- |:------------------------------------------------------------------------------- |
| **CLAP vs. NAMCore Parity**   | ESR < 1e-8, SNR > 80 dB; 3 fixtures × 44.1/48/96 kHz | `clap_parity_multi_sr.rs` (`artifact` phase of `tests-long.sh`, release Gate 3) |
| **Artifact OS/CabSim Parity** | ESR < 1e-8, SNR > 80 dB @ 2×/4× and CabSim           | `test_clap_parity_oversample`, `test_clap_parity_cabsim` (`artifact` phase)     |
| **Bypass Transparency**       | Phase cancellation < −120 dBFS                       | `processor_bypass_test.rs`                                                      |
| **RT Allocation Budget**      | Exactly 0 heap allocations during `process()`        | `verify_no_rt_alloc.sh` (static) & `alloc_audit.rs` (dynamic)                   |
| **RT Codegen Invariants**     | 0 heap allocs, 0 core-loop divs, 0 core-loop TLS     | `verify_rt_codegen.sh` (static disassembly analysis)                            |
| **CLAP Event Handling**       | 0 panics or unhandled boundary conditions            | `clap_e2_proptest.rs`                                                           |

### 6.1 Reference Performance Baseline (Criterion Benchmark Matrix)

Measured on an isolated CPU core (`x86-64-v3` AVX2/FMA baseline, `performance` governor, operator-pinned host):

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

| Class                         | Typical Scripts                         | Error Handling Policy                                                                                                                     | Architectural Rationale                                                                         |
|:----------------------------- |:--------------------------------------- |:----------------------------------------------------------------------------------------------------------------------------------------- |:----------------------------------------------------------------------------------------------- |
| **Class A — Interactive**     | `lints.sh`, `tests-quick.sh`            | `set -euo pipefail`; aborts on first failure; `ERR` trap emits `RESULT: FAIL`                                                             | Real-time developer loops. Immediate failure surfaces bugs fast and conserves developer cycles. |
| **Class B — Decoupled Audit** | `tests-long.sh`                         | `set -o pipefail` (no `-e`, no `-u`); each phase records status independently; collects all failures; exit `1` at end if any phase failed | Nightly and pre-release audits. Collects all phase evidence without aborting prematurely.       |
| **Class C — Build**           | `build-release.sh`                      | `set -euo pipefail`; aborts on first failure                                                                                              | Build pipelines where intermediate failure invalidates downstream artifacts.                    |
| **Class D — Manual Tool**     | `mod-update.sh`, `verify_rt_codegen.sh` | `set -euo pipefail`                                                                                                                       | Targeted utilities executed under direct operator supervision.                                  |

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
   RESULT: PASS|PASS_WITH_GAPS|FAIL (<seconds>s) fails=<list|none> gaps=<list|none> scope=<full|partial>
   ```

4. **Exit Codes:**

   - `0`: Complete success (`PASS`), or success with documented gaps (`PASS_WITH_GAPS`) when not in strict mode.
   - `1`: Execution failure (`FAIL`), or unfulfilled `GAP` when invoked with `--strict`.
   - `2`: Syntax error, unknown option, or missing precondition.

Detailed execution logs are written to `target/logs/<script>-<phase>.log` via `tee`. The long suite uses the canonical `target/logs/long-<name>.log` names listed in [§5.1.2](#512-long-audit-lifecycle-utilstests-longsh). The canonical `PHASE:`/`RESULT:` lines are also appended to `target/logs/summary.txt` and published by the local `.github/actions/qa-summary` composite action; the ARM CI job now invokes `utils/lints.sh` instead of an inline clippy command.

### 7.2.1 Evidence Lifecycle (`utils/_lib.sh`)

`utils/_lib.sh` is a pure definition file: sourcing it has no side effects. Every script parses `-h`/`--help` and rejects unknown flags **before** calling `lib_init`, so help never destroys evidence. `lib_init` is the single point that creates `target/logs`, resolves `NAM_RUN_ID` (honoring a pre-exported value, as CI does per run/attempt), truncates `target/logs/summary.txt` only when a pre-exported run id differs from `target/logs/.summary-run` (a locally generated id never truncates: it accumulates and prints a `RUN: <id>` marker line before the phases), and removes the script's own stale carrier. `utils/build-release.sh` writes `target/logs/build-run-id` after a successful build; `utils/tests-long.sh` reuses it when `NAM_RUN_ID` is not pre-exported, so the three local release commands reconcile. A discipline invoked as a child inside another discipline's phase sets `lib_quiet=1`, which suppresses its writes to `summary.txt`: the parent's verdict can never be replaced by a child's `PASS`.

- **One RESULT per invocation.** Every path through a script — success, phase failure, unexpected error (`ERR` trap → `trap_fail`), or usage error — terminates with exactly one `RESULT:` line and exit codes `0`/`1`/`2` as above.
- **Typed status carrier.** Each `finish` writes `target/logs/<script>-status.tsv`: one line, five TAB-separated columns — `<discipline>\t<PASS|PASS_WITH_GAPS|FAIL>\t<gaps|none>\t<fails|none>\t<scope=full|scope=partial>` (lists comma-separated). A partial selection (`--only`) records `scope=partial` without counting as a GAP; certification (`utils/build-release.sh --certify`) strictly rejects partial scope. A carrier write failure forces `FAIL` and appends `carrier` to the fails list; a missing or invalid carrier is never treated as success. `summary.txt` remains the human/AI-readable record, not the decision source.
- **GAP grammar.** Gap markers follow `PHASE: <phase_name> GAP <technical_reason>`, emitted by the `gap` helper with the current `PHASE_NAME`.
- **Write failures are failures.** `emit` records a failed `summary.txt` append in `EMIT_RC` without returning error (a `return 1` would escape the ERR trap under `set -e` and abort with neither `RESULT` nor carrier); `finish` then downgrades the verdict to `FAIL`, rewrites the carrier with `fails=emit` and prints a single `RESULT: FAIL`. `run_step` captures both `PIPESTATUS[0]` (the producer) and the `tee` status. Neither path can silently report `PASS`.

### 7.3 Core Automation Principles

- **No Tests of Test Tooling:** Test suites validate CLAP plugin interfaces, DSP kernels, neural inference, and GUI event loops. Meta-tests that verify auxiliary scripts, receipt JSON schemas, or internal QA tools are forbidden. Tooling integrity is guaranteed by strict typing, minimal surface area, and routine execution.
- **Quick Never Provisions Infrastructure:** `utils/tests-quick.sh` operates entirely on already-available fixtures and pre-built tooling. It never provisions external dependencies or triggers out-of-band builds. Missing optional tooling or fixtures gracefully emits a typed `GAP`.
- **Long Collects All Failures:** `utils/tests-long.sh` executes all configured preflights and stress phases to completion, aggregating passes, gaps, and failures into a final diagnostic summary without early termination.
- **Convention Over Parameterization:** All scripts default to host capabilities (all online cores, zero artificial thermal cooldown, unpinned execution). Environment variables only exist to override defaults when explicitly required.

### 7.4 Canonical Command-Line Flags

Only canonical flags are supported across scripts (no legacy aliases):

| Script             | Supported Flags                                    | Notes                                                                                                                                                                                                                                |
|:------------------ |:-------------------------------------------------- |:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **All scripts**    | `-h`, `--help`                                     | Displays concise usage and exits 0.                                                                                                                                                                                                  |
| `lints.sh`         | None                                               | Strict zero-flag policy; formatting is applied in-place during local development and verified via `CI=true` on CI.                                                                                                                   |
| `tests-quick.sh`   | None                                               | Fast first-line discipline; no allow-gaps switch and no simulation mode.                                                                                                                                                             |
| `tests-long.sh`    | `--strict`, `--only <name>[,<name>]`, `--artifact` | `--strict` promotes any `GAP` to `FAIL` (exit 1); `--only` runs only the named canonical phases, records the others as `NOT_RUN` (typed GAP), and preserves `target/long`; `--artifact` tests canonical `target/dist/nam_plug.clap`. |
| `build-release.sh` | `--strict`, `--package`, `--certify`, `--install`  | `--package` builds tarball and Flatpak; `--certify` validates receipts without recompiling; `--install` (only with `--certify`) installs the verified artifact to `~/.clap/` and the declared Flatpak bundle.                        |

### 7.5 Canonical Environment Variables

#### Interface Variables (User & CI Control)

| Variable              | Default                                           | Scope           | Purpose & Semantics                                                                                      |
|:--------------------- |:------------------------------------------------- |:--------------- |:-------------------------------------------------------------------------------------------------------- |
| `CI`                  | Unset                                             | CI Runner       | When `true`, instructs `lints.sh` to execute `cargo fmt --check` instead of modifying files in-place.    |
| `NAM_BENCH_CORE`      | `auto`                                            | Benchmark       | Core pinning: explicit CPU ID, isolated core if online, first allowed CPU, or unpinned fallback (`GAP`). |
| `NAM_PHASE_TIMEOUT_S` | Per-phase default                                 | `tests-long.sh` | Overrides timeout duration across all audit phases for emergency debugging.                              |
| `NAM_GUI_TIMEOUT_MS`  | `15000` in `tests-long.sh`; `4000` in the harness | GUI Testing     | Timeout in milliseconds for headless Xvfb window and lifecycle assertions.                               |

#### Harness Variables (Internal Script → Rust Communication)

| Variable                 | Configured By                 | Read By                    | Purpose                                                                  |
|:------------------------ |:----------------------------- |:-------------------------- |:------------------------------------------------------------------------ |
| `NAM_RUN_ID`             | `utils/_lib.sh`               | Test harnesses             | Unique timestamped run identifier for logging and receipt isolation.     |
| `NAM_REQUIRE_CPP_ORACLE` | `tests-long.sh --strict`      | Parity suites              | Enforces hard failure if the C++ reference render binary is unavailable. |
| `NAM_RT_STRICT`          | `tests-long.sh --strict`      | RT deadline tests          | Rejects latency outliers and deadline budget misses fail-closed.         |
| `CLAP_PLUGIN_UNDER_TEST` | `tests-long.sh --artifact`    | Long audit harnesses       | Specifies path to the canonical compiled `.clap` binary under test.      |
| `NAM_GUI_DEDICATED_X11`  | `tests-long.sh` (`gui` phase) | GUI lifecycle tests        | Requests a dedicated X11 display path for the headless GUI tests.        |
| `NAM_CORE_RENDER_BIN`    | `_lib.sh` / preflights        | C++ Parity tests           | Absolute path to the compiled upstream C++ `render` CLI binary.          |
| `NAM_THERMAL_COOLDOWN_S` | `2`                           | `utils/ab-opt-ceremony.sh` | Operator                                                                 |
| `NAM_FIXTURES_DIR`       | `_lib.sh` / preflights        | Model loaders              | Explicit path to test model fixtures directory.                          |

### 7.6 Measured Durations & Budgets

Execution times measured on standard development hardware (16-core, SSD):

| Discipline / Script      | Measured Duration                      | Budget Ceiling | Notes                                                                                                                                                                                                                                                               |
|:------------------------ |:-------------------------------------- |:-------------- |:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `utils/lints.sh`         | ~8s (warm cache) to ~45s (clean build) | $\le 2$ min    | Covers fmt, check (3 permutations), clippy (`-D warnings`), cross-target `aarch64-unknown-linux-gnu` clippy (native aarch64 hosts reuse the earlier clippy pass; x86_64 hosts set `PKG_CONFIG_ALLOW_CROSS=1`), doc, SPDX, policies, and RT allocation verification. |
| `utils/tests-quick.sh`   | ~36s (warm cache) to ~90s              | $\le 5$ min    | Covers structural unit/integration tests and RT-safety heap audit. C++ NAMCore parity, oversampling, CabSim, and codegen run in Long (`artifact`) and build.                                                                                                        |
| `utils/tests-long.sh`    | ~19.7 min (measured baseline)          | $\le 25$ min   | Catch-all, headless Xvfb GUI, neutral artifact / C++ NAMCore parity, oversampling, CabSim IR, perf certification, and heap-audit soak.                                                                                                                              |
| `utils/build-release.sh` | ~11.8 min (measured baseline)          | $\le 15$ min   | PGO profiling workload, BOLT optimization, and canonical packaging.                                                                                                                                                                                                 |

#### Automated QA Footprint

The verification suite maintains a compact footprint across shell scripts (~1,250 lines), AWK/Python filters (~670 lines), and Rust test/receipt drivers in `src/bin/` (~3,000 lines). The quick suite preserves the deterministic `heap-audit` allocation check in its fast-feedback gate while deferring compute-heavy parity checks to the artifact audit.

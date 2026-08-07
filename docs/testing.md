<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Automated Test Suite & Benchmark Architecture — `NAM-Plug`

This document details the automated test suite, integration harness, property-based testing, allocation auditing, and Criterion benchmark targets for the `NAM-Plug` CLAP plugin subproject ([`file:///home/fabio/NAM/NAM-Plug/`](file:///home/fabio/NAM/NAM-Plug/)).

> [!NOTE]
> For manual QA procedures and DAW-specific human testing workflows (Bitwig, Fender Studio Pro), see [functional-tests.md](file:///home/fabio/NAM/NAM-Plug/docs/functional-tests.md). For overall plugin architecture and internal design, see [architecture.md](file:///home/fabio/NAM/NAM-Plug/docs/architecture.md).

---

## 1. Scope & Crate Features Taxonomy

`NAM-Plug` testing relies on Cargo feature flags defined in [`Cargo.toml`](file:///home/fabio/NAM/NAM-Plug/Cargo.toml):

| Feature Flag     | Description                                                        | Test/Bench Usage Scope                                                                                |
|:---------------- |:------------------------------------------------------------------ |:----------------------------------------------------------------------------------------------------- |
| **`testing`**    | Enables engine test utilities, generators, and fixture resolution. | Mandatory feature flag when running `NAM-Plug` integration tests and benches.                         |
| **`heap-audit`** | Intercepts memory allocations via `CountingAllocator`.             | Used by RT-safety tests to ensure zero heap allocations occur on the audio thread during `process()`. |
| **`stereo`**     | Enables dual-channel L/R processing.                               | Default feature enabled across standard builds and test runs.                                         |

---

## 2. Integration Harness & Dynamic Artifact Validation

The `NAM-Plug` test suite simulates a real CLAP host environment using [`clack-host`](https://crates.io/crates/clack-host).

### 2.1 Dynamic Artifact Validation ([`tests/clap/artifact_validator.rs`](file:///home/fabio/NAM/NAM-Plug/tests/clap/artifact_validator.rs))

Integration tests execute against the dynamically compiled `.so` plugin binary rather than in-process static links where applicable. The `ArtifactValidator` helper:

1. Locates the compiled target artifact (`target/debug/libnam_plug.so` or `target/release/libnam_plug.so`).
2. Computes the SHA-256 fingerprint of the `.so` binary to ensure test traceability.
3. Loads the plugin entrypoint dynamically via `PluginEntry::load(&artifact.path)`.

### 2.2 RT Heap Allocation Audit ([`tests/common/alloc_audit.rs`](file:///home/fabio/NAM/NAM-Plug/tests/common/alloc_audit.rs))

When compiled with `--features "testing heap-audit"`, `tests/clap.rs` registers `CountingAllocator` as the `#[global_allocator]`. The test harness captures allocation counters before and after calling `started_processor.process()`, enforcing **zero heap allocations** on the audio thread.

---

## 3. Automated Test Inventory

The automated test targets under [`file:///home/fabio/NAM/NAM-Plug/tests/`](file:///home/fabio/NAM/NAM-Plug/tests/) are structured into root test files and modular sub-suites.

### 3.1 Main Harness Entrypoint ([`tests/clap.rs`](file:///home/fabio/NAM/NAM-Plug/tests/clap.rs))

`tests/clap.rs` acts as the root harness declaring common utilities and submodules under `tests/clap/`.

### 3.2 Modular Sub-Suites ([`tests/clap/`](file:///home/fabio/NAM/NAM-Plug/tests/clap/))

| Test Module Target         | File Link                                                                                       | Scope & Verification Objective                                                                                                                                                                       |
|:-------------------------- |:----------------------------------------------------------------------------------------------- |:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **`artifact_validator`**   | [`artifact_validator.rs`](file:///home/fabio/NAM/NAM-Plug/tests/clap/artifact_validator.rs)     | Dynamic artifact discovery, path verification, and SHA256 integrity reporting for `.so` binary testing.                                                                                              |
| **`clap_cross_machine`**   | [`clap_cross_machine.rs`](file:///home/fabio/NAM/NAM-Plug/tests/clap/clap_cross_machine.rs)     | Cross-platform determinism, float consistency, and sample rate conversion (SRC) output identity across varying frame boundaries.                                                                     |
| **`clap_lifecycle_test`**  | [`clap_lifecycle_test.rs`](file:///home/fabio/NAM/NAM-Plug/tests/clap/clap_lifecycle_test.rs)   | CLAP plugin lifecycle transitions (`activate` → `start_processing` → `process` → `stop_processing` → `deactivate` → `reset`). Verifies audio configuration renegotiation (sample rate & block size). |
| **`clap_multi_instance`**  | [`clap_multi_instance.rs`](file:///home/fabio/NAM/NAM-Plug/tests/clap/clap_multi_instance.rs)   | Multi-instance concurrency and thread-safety. Confirms that multiple active plugin instances operate without shared SPSC queue leakage or parameter crosstalk.                                       |
| **`clap_parity_multi_sr`** | [`clap_parity_multi_sr.rs`](file:///home/fabio/NAM/NAM-Plug/tests/clap/clap_parity_multi_sr.rs) | Parity validation across sample rates (44.1 kHz, 48 kHz, 88.2 kHz, 96 kHz, 192 kHz) against `NeuralAmpModeler-rs` reference audio engine outputs (ESR < 1e-11, SNR > 110 dB).                        |
| **`clap_state_migration`** | [`clap_state_migration.rs`](file:///home/fabio/NAM/NAM-Plug/tests/clap/clap_state_migration.rs) | CLAP state persistence (`clap.state` and `clap.state-context`), preset loading, parameter serialization, and version migration handling.                                                             |
| **`tail_semantics`**       | [`tail_semantics.rs`](file:///home/fabio/NAM/NAM-Plug/tests/clap/tail_semantics.rs)             | Implementation of CLAP tail extension (`clap_plugin_tail`). Asserts that tail length is reported accurately and silence tail flushing completes cleanly during gate decay.                           |

### 3.3 Epic E0 Regression Containment Suite ([`tests/clap_e0_containment_test.rs`](file:///home/fabio/NAM/NAM-Plug/tests/clap_e0_containment_test.rs))

TDD red/green containment suite guarding against specific architectural regression cases:

- **CLAP-F001 (CabSim Participation):** Ensures loaded impulse responses participate directly in audio processing and latency calculations.
- **CLAP-F004 (Parameter State Fidelity):** Asserts parameter changes remain faithful through save/restore cycles.
- **CLAP-F009 (Reset Semantics):** Validates DSP internal state clear during `reset()`.
- **CLAP-F014 (Sample Rate Negotiation):** Confirms DSP pipeline re-initializes upon host sample rate changes.
- **CLAP-F007 (State Deserialization):** Guarantees corrupt or partial state payloads recover gracefully.

### 3.4 Property-Based Testing ([`tests/clap_e2_proptest.rs`](file:///home/fabio/NAM/NAM-Plug/tests/clap_e2_proptest.rs))

Uses `proptest` to generate random audio buffer lengths, parameter value sequences, and event queues to stress test CLAP event handling and boundary condition handling.

### 3.5 Processor Bypass Test ([`tests/processor_bypass_test.rs`](file:///home/fabio/NAM/NAM-Plug/tests/processor_bypass_test.rs))

Tests plugin bypass processing, verifying bit-transparent phase cancellation (< -120 dBFS) when bypassed and smooth crossfade transitions during bypass state toggles.

### 3.6 Common Test Utilities ([`tests/common/`](file:///home/fabio/NAM/NAM-Plug/tests/common/))

- **[`alloc_audit.rs`](file:///home/fabio/NAM/NAM-Plug/tests/common/alloc_audit.rs):** Global memory allocation counting interceptor.
- **[`metrics.rs`](file:///home/fabio/NAM/NAM-Plug/tests/common/metrics.rs):** Off-RT audio fidelity metrics: Peak, RMS, SNR (Signal-to-Noise Ratio), and ESR (Error-to-Signal Ratio).
- **[`perceptual.rs`](file:///home/fabio/NAM/NAM-Plug/tests/common/perceptual.rs):** Spectral and perceptual comparison helpers.
- **[`wav.rs`](file:///home/fabio/NAM/NAM-Plug/tests/common/wav.rs):** WAV file reading/writing helpers for fixture comparison.

---

## 4. Benchmark Suite Architecture — `benches/clap_bench.rs`

The benchmark suite under [`file:///home/fabio/NAM/NAM-Plug/benches/`](file:///home/fabio/NAM/NAM-Plug/benches/) uses [Criterion.rs](https://bheisler.github.io/criterion.rs/book/index.html) to measure host process block throughput and CLAP event dispatch overhead.

### 4.1 Measured Execution Targets

1. **SIMD Fast-Path Throughput:** Measures `process()` execution duration with empty CLAP event queues across block sizes:
   - **64 samples** (ultra-low latency mode)
   - **128 samples** (standard Live mode)
   - **256, 512, 1024 samples** (DAW mixing/mastering buffers)
2. **CLAP Parameter Event Queue Overhead:** Measures `process()` execution duration when handling active parameter modulation events (`ParamValueEvent`) queued per sample.
3. **Render Mode Scaling:** Compares processing times in `RenderMode::Realtime` vs `RenderMode::Offline` (HQ oversampling mode).

### 4.2 Benchmark Fixtures ([`benches/common.rs`](file:///home/fabio/NAM/NAM-Plug/benches/common.rs))

Provides 64-byte aligned audio buffer allocations, synthetic test signal generators (440 Hz sine wave, log frequency sweep, Gaussian white noise), and dummy `BenchHost` handlers.

---

## 5. Execution Commands & Developer Workflow

All test and benchmark execution commands **must be executed inside `./NAM-Plug/`**:

### 5.1 Verification Scripts (`utils/`)

```bash
# 1. Static analysis quality gate (formatting, SPDX headers, cargo check, cargo clippy)
./utils/lints.sh

# 2. Agile first line of defense QA suite (cargo test in debug & release)
./utils/tests-quick.sh
```

### 5.2 Direct Cargo Commands

```bash
# 1. Quick compilation and lint check for tests and benches
cargo check --tests --benches --features testing

# 2. Run standard automated unit and integration tests
cargo test --features testing

# 3. Run allocation audit RT-safety tests
cargo test --features "testing heap-audit"

# 4. Run property-based tests
cargo test --features testing --test clap_e2_proptest

# 5. Run Criterion benchmarks
cargo bench --features testing --bench clap_bench
```

---

## 6. Quality Gates & Baseline Standards

| Metric / Test Gate       | Threshold / Constraint                        | Enforced In                  |
|:------------------------ |:--------------------------------------------- |:---------------------------- |
| **Audio Engine Parity**  | ESR < 1e-11, SNR > 110 dB                     | `clap_parity_multi_sr.rs`    |
| **Bypass Transparency**  | Phase cancellation < -120 dBFS                | `processor_bypass_test.rs`   |
| **RT Allocation Budget** | Exactly 0 heap allocations during `process()` | `alloc_audit.rs` / `clap.rs` |
| **CLAP Event Handling**  | 0 panics / unhandled boundary conditions      | `clap_e2_proptest.rs`        |

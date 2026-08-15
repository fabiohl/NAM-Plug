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

### 2.2 RT Heap Allocation Audit ([`tests/common/alloc_audit.rs`](../tests/common/alloc_audit.rs))

When compiled with `--features "testing heap-audit"`, `tests/clap.rs` registers `CountingAllocator` as the `#[global_allocator]`. The test harness captures allocation counters before and after calling `started_processor.process()`, enforcing **zero heap allocations** on the audio thread.

---

## 3. Automated Test Inventory

The automated test targets under [`../tests/`](../tests/) are structured into root test files and modular sub-suites.

### 3.1 Main Harness Entrypoint ([`tests/clap.rs`](../tests/clap.rs))

`tests/clap.rs` acts as the root harness declaring common utilities and submodules under `tests/clap/`.

### 3.2 Modular Sub-Suites ([`tests/clap/`](../tests/clap/))

The root harness declares modular sub-suites covering: dynamic artifact discovery and SHA256 integrity (`artifact_validator`), cross-machine determinism and float consistency across frame boundaries (`clap_cross_machine`), plugin lifecycle transitions and audio configuration renegotiation (`clap_lifecycle_test`), multi-instance concurrency and thread-safety (`clap_multi_instance`), CLAP × NAMCore C++ oracle parity with ESR/SNR gates (`clap_parity_multi_sr`), state persistence and version migration (`clap_state_migration`), and CLAP tail extension semantics (`tail_semantics`). Individual thresholds are defined at the top of each module and summarized in section 6, rather than duplicated in an inventory table.

### 3.3 Epic E0 Regression Containment Suite ([`tests/clap_e0_containment_test.rs`](../tests/clap_e0_containment_test.rs))

TDD red/green containment suite guarding against specific architectural regression cases:

- **CLAP-F001 (CabSim Participation):** Ensures loaded impulse responses participate directly in audio processing and latency calculations.
- **CLAP-F004 (Parameter State Fidelity):** Asserts parameter changes remain faithful through save/restore cycles.
- **CLAP-F009 (Reset Semantics):** Validates DSP internal state clear during `reset()`.
- **CLAP-F014 (Sample Rate Negotiation):** Confirms DSP pipeline re-initializes upon host sample rate changes.
- **CLAP-F007 (State Deserialization):** Guarantees corrupt or partial state payloads recover gracefully.

### 3.4 Property-Based Testing ([`tests/clap_e2_proptest.rs`](../tests/clap_e2_proptest.rs))

Uses `proptest` to generate random audio buffer lengths, parameter value sequences, and event queues to stress test CLAP event handling and boundary condition handling.

### 3.5 Processor Bypass Test ([`tests/processor_bypass_test.rs`](../tests/processor_bypass_test.rs))

Tests plugin bypass processing, verifying bit-transparent phase cancellation (< -120 dBFS) when bypassed and smooth crossfade transitions during bypass state toggles.

### 3.6 Common Test Utilities ([`tests/common/`](../tests/common/))

- **[`alloc_audit.rs`](../tests/common/alloc_audit.rs):** Global memory allocation counting interceptor.
- **[`metrics.rs`](../tests/common/metrics.rs):** Off-RT audio fidelity metrics: Peak, RMS, SNR (Signal-to-Noise Ratio), and ESR (Error-to-Signal Ratio).
- **[`perceptual.rs`](../tests/common/perceptual.rs):** Spectral and perceptual comparison helpers.
- **[`wav.rs`](../tests/common/wav.rs):** WAV file reading/writing helpers for fixture comparison.

---

## 4. Benchmark Suite Architecture — `benches/clap_bench.rs`

The benchmark suite under [`../benches/`](../benches/) uses [Criterion.rs](https://bheisler.github.io/criterion.rs/book/index.html) to measure host process block throughput and CLAP event dispatch overhead.

### 4.1 Measured Execution Targets

1. **SIMD Fast-Path Throughput:** Measures `process()` execution duration with empty CLAP event queues across block sizes:
   - **64 samples** (ultra-low latency mode)
   - **128 samples** (standard Live mode)
   - **256, 512, 1024 samples** (DAW mixing/mastering buffers)
2. **CLAP Parameter Event Queue Overhead:** Measures `process()` execution duration when handling active parameter modulation events (`ParamValueEvent`) queued per sample.
3. **Render Mode Scaling:** Compares processing times in `RenderMode::Realtime` vs `RenderMode::Offline` (HQ oversampling mode).

### 4.2 Benchmark Fixtures ([`benches/common.rs`](../benches/common.rs))

Provides 64-byte aligned audio buffer allocations, synthetic test signal generators (440 Hz sine wave, log frequency sweep, Gaussian white noise), and dummy `BenchHost` handlers.

---

## 5. Execution Commands & Developer Workflow

All test and benchmark execution commands **must be executed inside `./NAM-Plug/`**:

### 5.1 Verification Scripts (`utils/`)

```bash
# 1. Static analysis quality gate (formatting, SPDX headers, cargo check, cargo clippy)
./utils/lints.sh

# 2. Agile first line of defense QA suite
./utils/tests-quick.sh
```

`utils/tests-quick.sh` runs three phases, each persisting its output to `target/logs/quick-phaseN.log`, and closes with a typed receipt (`target/logs/quick-receipt.txt`):

1. **Structural (debug)** — unit + integration tests with debug assertions ON. `ensure_clap_artifact debug` validates the `.so` artifact (fail-closed: missing artifact aborts with `FATAL:`) and logs its SHA256 before any test that `dlopen`s it.
2. **Release verification (release)** — the release-only surface (S6-T04 / RES-04): `ensure_clap_artifact release` builds the `.so` under release codegen, then the **CLAP × NAMCore parity oracle**: `test_clap_parity_multi_rate` (ESR < 1e-8, SNR > 80 dB) compares the release `.so` against the C++ render binary (`NAM_CORE_RENDER_BIN` or `build/namcore_render`), executing when the render binary, the release `.so` and the model fixture are all present. The Phase 1 targets are not re-run under `--release` — debug assertions ON already validate that logic, and release codegen of the `.so` is exactly what the oracle measures. Missing prerequisites are never masked — they are recorded as `GAPS+=("clap_parity_multi_rate:missing_render_or_fixtures")` and reported as a `WARN GAP`.
3. **RT-Safety heap-audit (debug)** — zero-allocation `process()` gate via `--features testing,heap-audit` (`processor_heap_audit_test`).

The run closes with `OVERALL: PASSED` or `OVERALL: PASSED_WITH_GAPS` (with `NAM_QUICK_STRICT=1`, any GAP turns the run into a failure).

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
#    also Phase 2 of tests-quick.sh when prerequisites are present)
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

---

## 6. Quality Gates & Baseline Standards

| Metric / Test Gate       | Threshold / Constraint                        | Enforced In                  |
|:------------------------ |:--------------------------------------------- |:---------------------------- |
| **CLAP vs NAMCore Parity** | ESR < 1e-8, SNR > 80 dB                    | `clap_parity_multi_sr.rs` (Phase 2 of `tests-quick.sh`) |
| **Bypass Transparency**  | Phase cancellation < -120 dBFS                | `processor_bypass_test.rs`   |
| **RT Allocation Budget** | Exactly 0 heap allocations during `process()` | `alloc_audit.rs` / `clap.rs` |
| **CLAP Event Handling**  | 0 panics / unhandled boundary conditions      | `clap_e2_proptest.rs`        |

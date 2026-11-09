<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Description of Changes
<!-- Provide a clear, concise summary of what was changed and the technical rationale behind it. -->

## Linked Issues
<!-- e.g. Fixes #123, Closes #456, or Relates to #789 -->
Fixes #

## Subsystem & Scope
<!-- Check all that apply. For documentation, GUI-only, or packaging PRs, RT/CLAP checklists below may be marked N/A. -->
- [ ] **CLAP Protocol & Extensions** (`src/clap/`, parameter automation, state context, latency reporting)
- [ ] **Real-Time Audio Processing** (`src/audio/`, audio callback, SPSC parameter & model sync)
- [ ] **GUI & User Interface** (`src/clap/gui/`, Slint 1.18 controls, visual meters, FemtoVG/OpenGL rendering, X11 XEmbed & Wayland)
- [ ] **Preset & Model Management** (`src/presets/`, IR loading, state serialization)
- [ ] **Benchmarks & Quality Gates** (`benches/`, `tests/`)
- [ ] **Documentation & Assets** (`docs/`, `README.md`)

---

## Real-Time Audio Safety Checklist (RT-Safe)
<!-- Applicable if modifying audio processing paths. Skip or mark N/A for GUI-only, docs, or tooling PRs. -->

- [ ] **N/A**: This PR does not touch audio callback code.
- [ ] **Zero Dynamic Heap Allocations in Audio Path**: The audio callback (`process()`, inner DSP blocks) makes zero calls to the heap allocator (`malloc`, `Box`, `Vec`, `String`, `Arc::new()`, `format!()`). GUI and parameter updates are decoupled via lock-free SPSC queues.
- [ ] **Lock-Free Concurrency**: No mutexes, RwLocks, or blocking thread synchronization primitives exist on the audio thread.
- [ ] **Zero Blocking I/O**: No filesystem access, network I/O, or synchronous output (`println!`, `eprintln!`) on the audio thread.
- [ ] **Zero `log::*` on Hot-Path**: No `log::*` invocations occur inside the audio process loop (status signaled via atomic bitmasks `RtStatusFlags`).
- [ ] **Panic Elimination**: No `unwrap()` or `expect()` on the audio hot-path; loops structured for static bounds-check elision.

---

## CLAP & Host Integration Checklist
<!-- Applicable if modifying CLAP protocol, state, or parameter handling. Skip or mark N/A otherwise. -->

- [ ] **N/A**: This PR does not touch CLAP protocol or host integration code.
- [ ] **Sample-Accurate Automation**: Parameter changes and automations are processed at exact sample offsets.
- [ ] **State Serialization**: State save/restore is robust, deterministic, and backwards-compatible with saved projects.
- [ ] **Latency & Tail Reporting**: Accurate latency (in samples) and tail length are reported when oversampling or linear phase filters change.
- [ ] **Render Mode Awareness**: Plugin correctly responds to `CLAP_RENDER_REALTIME` vs `CLAP_RENDER_OFFLINE` modes.

---

## Verification & Quality Gates (Local & GitHub CI)
<!--
Responsible Verification Model:
- Fast Local Feedback: Run cargo check and targeted tests for quick local iteration.
- GitHub CI Automation: The full test matrix (x86_64, Linux ARM64 with virtual X11/xvfb, MSRV 1.99.0, and sibling engine integration) runs automatically on every push and PR.
- Friendly Tip for Contributors: If you don't have X11/Wayland dev libraries, OpenGL, or the sibling engine configured locally, open a DRAFT PR and let GitHub Actions run the full verification matrix for you!
-->

### 1. Recommended Local Checks (Fast Feedback)

```bash
# Fast check during development:
cargo check --all-targets

# Targeted test for changed components (example):
cargo test --lib <module>
```

### 2. Optional Full Local Suite (If environment allows)

```bash
utils/lints.sh        # Static analysis, fmt, clippy (-D warnings), SPDX validation
utils/tests-quick.sh  # Agile testing, host integration tests, CLAP validation
```

- [ ] **Targeted Local Checks Passed**: Code compiles cleanly without warnings (`cargo check --all-targets`) and unit tests for modified components pass.
- [ ] **Full Local Suite Executed (Optional)**: Successfully ran `utils/lints.sh` and/or `utils/tests-quick.sh` (or deferring full matrix to GitHub CI via Draft PR).
- [ ] **Automated GitHub CI**: All checks pass on GitHub Actions (`x86_64-linux-lints`, `x86_64-linux-quick`, `linux-arm64-lints`, `linux-arm64-quick`, `msrv`).
- [ ] **License & SPDX Headers**: All new and modified files include the GPL-3.0-or-later SPDX header and copyright notice:

  ```text
  SPDX-License-Identifier: GPL-3.0-or-later
  Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
  ```

- [ ] **Subproject Self-Containment**: No references or links escape the repository root.
- [ ] **Undocumented Clippy Allows**: Any `#[allow(clippy::...)]` attribute includes an explanatory justification comment on the preceding line.

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
<!-- Check all that apply -->
- [ ] **CLAP Protocol & Extensions** (`src/clap/`, parameter automation, state context, latency reporting)
- [ ] **Real-Time Audio Processing** (`src/audio/`, audio callback, SPSC parameter & model sync)
- [ ] **GUI & User Interface** (`src/gui/`, egui controls, visual meters, OpenGL rendering)
- [ ] **Preset & Model Management** (`src/presets/`, IR loading, state serialization)
- [ ] **Benchmarks & Quality Gates** (`benches/`, `tests/`)
- [ ] **Documentation & Assets** (`docs/`, `README.md`)

---

## Real-Time Audio Safety Checklist (RT-Safe)
<!-- The audio callback thread runs at high priority with strict sub-millisecond deadlines. RT safety is non-negotiable. -->

- [ ] **Zero Dynamic Heap Allocations in Audio Path**: The audio callback (`process()`, inner DSP blocks) makes zero calls to the heap allocator (`malloc`, `Box`, `Vec`, `String`, `Arc::new()`, `format!()`). GUI and parameter updates are decoupled via lock-free SPSC queues.
- [ ] **Lock-Free Concurrency**: No mutexes, RwLocks, or blocking thread synchronization primitives exist on the audio thread.
- [ ] **Zero Blocking I/O**: No filesystem access, network I/O, or synchronous output (`println!`, `eprintln!`) on the audio thread.
- [ ] **Zero `log::*` on Hot-Path**: No `log::*` invocations occur inside the audio process loop (status signaled via atomic bitmasks `RtStatusFlags`).
- [ ] **Panic Elimination**: No `unwrap()` or `expect()` on the audio hot-path; loops structured for static bounds-check elision.

---

## CLAP & Host Integration Checklist

- [ ] **Sample-Accurate Automation**: Parameter changes and automations are processed at exact sample offsets.
- [ ] **State Serialization**: State save/restore is robust, deterministic, and backwards-compatible with saved projects.
- [ ] **Latency & Tail Reporting**: Accurate latency (in samples) and tail length are reported when oversampling or linear phase filters change.
- [ ] **Render Mode Awareness**: Plugin correctly responds to `CLAP_RENDER_REALTIME` vs `CLAP_RENDER_OFFLINE` modes.

---

## Pre-Submission Verification Suite (Mandatory)
<!-- Run these verification scripts from the repository root before opening or marking PR ready for review: -->

```bash
utils/lints.sh        # Static analysis, fmt, clippy (-D warnings), SPDX validation
utils/tests-quick.sh  # Agile testing, host integration tests, CLAP validation
```

- [ ] **`utils/lints.sh` Passed**: 100% clean across all feature permutations (`--all-features`, `--no-default-features`, `stereo`, `testing`).
- [ ] **`utils/tests-quick.sh` Passed**: All unit tests and CLAP host integration tests pass cleanly without regressions.
- [ ] **License & SPDX Headers**: All new and modified files include the GPL-3.0-or-later SPDX header and copyright notice:

  ```text
  SPDX-License-Identifier: GPL-3.0-or-later
  Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
  ```

- [ ] **Subproject Self-Containment**: No references or links escape the repository root.
- [ ] **Undocumented Clippy Allows**: Any `#[allow(clippy::...)]` attribute includes an explanatory justification comment on the preceding line.

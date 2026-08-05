// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

// Bypass is now handled per-sub-block inside the orchestrator's sub-block loop.
// Events (including bypass state changes) are applied sample-accurately at
// sub-block boundaries, and the bypass decision is evaluated per sub-block
// within process_sub_block(). See:
//   - orchestrator.rs:process_dsp_audio() for the sub-block loop
//   - orchestrator.rs:copy_bypass_to_output() for dry passthrough copy
//   - S2-E2-T03 for the future unified bypass/wet scheduler with crossfade.

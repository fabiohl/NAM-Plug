#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# =============================================================================
# Performance Regression Gate — thin wrapper over the engine `nam_perf_gate`
# =============================================================================
#
# Unified performance governance (Sprint 3 / EC-03; orchestration shared since
# Sprint 9 / T9.3): runs the Criterion `clap_bench` suite and delegates every
# interpretation (environment fingerprint, baseline persistence, per-benchmark
# coverage and the machine regression verdict over Criterion
# `change/estimates.json`) to the engine binary `nam_perf_gate`, the single
# statistical authority across the ecosystem. The shared orchestration body lives
# in the canonical `lib/_lib_core.sh` (`run_perf_regression_gate`), drift-pinned
# by `utils/test-lib-drift.sh`; this file only carries the repository-specific
# configuration (bench suite + expected-id manifest).
#
# The expected benchmark set is declared in `benches/clap_bench.manifest`
# (trailing-`*` group patterns), so a group that did not run at all — or any
# executed benchmark without a saved baseline series — fails closed.
#
# Modes
#   --check (default)     Compare the current build against the saved baseline.
#   --bootstrap-baseline  Create a new baseline + environment fingerprint
#                         (human operator only).
#
# Environment
#   NAM_PERF_GATE          Path to a prebuilt engine `nam_perf_gate` (else a
#                          PATH install, else a sibling engine checkout build).
#   NAM_ENGINE_DIR         Engine checkout used to build the gate when unset
#                          (default: ../NeuralAmpModeler-rs).
#   NAM_BENCH_CORE         CPU core pinned via taskset (default pick_bench_core).
#   NAM_BASELINE_NAME      Criterion baseline name (default ci-baseline).
#   NAM_BENCH_MANIFEST     Expected-id manifest (default benches/clap_bench.manifest).
#   NAM_THERMAL_COOLDOWN_S Cooldown before benchmarking (default 180, 0 to skip).
#   NAM_GOVERNOR_FILE      Governor sysfs path override (tests only).
#
# Usage
#   utils/tests-performance-regression.sh [--check|--bootstrap-baseline]
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib/_lib.sh"

# Single source of truth for the Criterion suite name: the expected-id manifest
# default is derived from it, so the two can never drift apart.
BENCH_SUITE="clap_bench"

# ── Governor guard (F-EV-02) ─────────────────────────────────────────────────
# A baseline captured under a DVFS-scaling governor is not comparable, so the
# guard runs before delegating: it covers --check and --bootstrap-baseline
# alike. Help/unknown arguments are delegated untouched so the shared core
# keeps ownership of usage reporting.
guard_governor() {
    local mode="$1"
    local bench_core governor_file governor

    bench_core="$(pick_bench_core)"
    # Pin the resolved core so the shared core reuses the same value instead of
    # resolving it twice (and possibly recording a second cpu_not_isolated gap).
    export NAM_BENCH_CORE="$bench_core"

    governor_file="${NAM_GOVERNOR_FILE:-/sys/devices/system/cpu/cpu${bench_core}/cpufreq/scaling_governor}"
    if [ ! -r "$governor_file" ]; then
        die "governor cannot be verified in ${mode} mode: '${governor_file}' missing or unreadable (fail-closed). The gate requires the 'performance' governor. Fix with 'sudo cpupower frequency-set -g performance' (or re-run environment bootstrap) and retry."
    fi
    governor="$(tr -d '[:space:]' < "$governor_file" 2>/dev/null || true)"
    if [ "$governor" != "performance" ]; then
        die "current governor for CPU ${bench_core} is '${governor:-unknown}', but gate requires 'performance' (${governor_file}). Fix with 'sudo cpupower frequency-set -g performance' (or re-run environment bootstrap) and retry."
    fi
}

guard_mode="--check"
guard_enabled=1
for arg in "$@"; do
    case "$arg" in
        --check) guard_mode="--check" ;;
        --bootstrap-baseline) guard_mode="--bootstrap-baseline" ;;
        *) guard_enabled=0; break ;;
    esac
done
if [ "$guard_enabled" -eq 1 ]; then
    guard_governor "$guard_mode"
fi

run_perf_regression_gate \
    "$SCRIPT_DIR" \
    "$BENCH_SUITE" \
    "$PROJECT_DIR/benches/${BENCH_SUITE}.manifest" \
    "$@"

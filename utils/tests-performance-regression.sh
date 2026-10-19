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
#   NAM_PERF_NOISE_THRESHOLD Noise band threshold for stability advisory (default: 0.05).
#   NAM_PERF_STABILITY_STRICT If 1, turns baseline stability advisory warning into fatal error.
#
# Note:
#   `--check` immediately after `--bootstrap-baseline` measures self-consistency
#   (repeatability of identical binary); the regression gate requires a baseline
#   from another session/commit.
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

# ── Baseline-stability advisory (two-sided) ──────────────────────────────────
# The shared gate only fails on REGRESSIONS. A large improvement between two
# back-to-back runs of the same build (e.g. -14% observed on
# A2_Slimmable/32_samp in a --bootstrap-baseline + --check pair) proves the
# baseline itself was unstable (first-run thermal/DVFS/cache state), so a green
# verdict would be false comfort. This advisory reports every series whose
# median change exceeds the noise band in EITHER direction. Non-fatal by
# default; NAM_PERF_STABILITY_STRICT=1 turns it into a failure.
perf_stability_advisory() {
    local threshold="${NAM_PERF_NOISE_THRESHOLD:-0.05}"
    local crit="${CARGO_TARGET_DIR:-$PROJECT_DIR/target}/criterion"
    [ -d "$crit" ] || return 0
    python3 - "$crit" "$threshold" <<'PY'
import json, os, sys
root, thr = sys.argv[1], float(sys.argv[2])
bad = []
for d, _, files in os.walk(root):
    if os.path.basename(d) == "change" and "estimates.json" in files:
        try:
            with open(os.path.join(d, "estimates.json")) as fh:
                med = json.load(fh)["median"]["point_estimate"]
        except (OSError, KeyError, ValueError):
            continue
        if abs(med) > thr:
            bad.append((os.path.relpath(os.path.dirname(d), root), med))
for name, med in sorted(bad):
    print(f"  [STABILITY] {name}: median change {med:+.1%} exceeds +/-{thr:.0%}")
sys.exit(3 if bad else 0)
PY
}

# ── Confirmation by re-execution for signaled series (B4, D5) ────────────────
confirm_signaled_regressions() {
    local gate_output_file="$1"
    local baseline_name="${NAM_BASELINE_NAME:-ci-baseline}"
    local noise="${NAM_PERF_NOISE_THRESHOLD:-0.05}"
    local crit_root="${CARGO_TARGET_DIR:-$PROJECT_DIR/target}/criterion"
    local perf_log="${CARGO_TARGET_DIR:-$PROJECT_DIR/target}/logs/perf-regression.log"
    mkdir -p "$(dirname "$perf_log")"

    # Only attempt confirmation if gate failure is SOLELY due to REGRESSION_DETECTED.
    # Non-regression failures (coverage gaps, environment mismatch, bench crash) fail closed immediately.
    if grep -qE 'FAIL:(baseline_coverage_gap|environment_or_baseline|benchmark_run)' "$gate_output_file" \
       || grep -qE '^REGRESSION_BLIND ' "$gate_output_file"; then
        return 1
    fi

    local -a reg_ids=()
    mapfile -t reg_ids < <(grep -E '^REGRESSION_DETECTED ' "$gate_output_file" | awk '{print $2}' | sort -u)
    if [ "${#reg_ids[@]}" -eq 0 ]; then
        return 1
    fi

    echo -e "\n${BLUE}${BOLD}[CONFIRM] ${#reg_ids[@]} series signaled for regression. Verifying with single-series re-execution...${NC}"

    # Resolve benchmark executable: use NAM_BENCH_EXE override if provided (tests), else extract
    local bench_bin="${NAM_BENCH_EXE:-}"
    if [ -z "$bench_bin" ] || [ ! -x "$bench_bin" ]; then
        local json_out
        json_out=$(cargo bench --bench "$BENCH_SUITE" --features testing --no-run --message-format=json 2>/dev/null || true)
        bench_bin=$(printf '%s\n' "$json_out" \
            | grep -F '"reason":"compiler-artifact"' \
            | grep -F "\"name\":\"${BENCH_SUITE}\"" \
            | grep -F '"executable":' \
            | tail -1 \
            | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p')
        if [ -z "$bench_bin" ] || [ ! -x "$bench_bin" ]; then
            bench_bin=$(ls -t "${CARGO_TARGET_DIR:-target}/release/deps/${BENCH_SUITE}-"* 2>/dev/null | head -1 || true)
        fi
    fi

    local bench_core="${NAM_BENCH_CORE:-$(pick_bench_core)}"
    local -a taskset_cmd=()
    if command -v taskset >/dev/null 2>&1; then
        taskset_cmd=(taskset -c "$bench_core")
    fi
    local -a chrt_cmd=()
    if command -v chrt >/dev/null 2>&1 && chrt -f 80 true 2>/dev/null; then
        chrt_cmd=(chrt -f 80)
    fi

    local -a persisted_ids=()
    local -a clean_ids=()

    for id in "${reg_ids[@]}"; do
        local est_file="$crit_root/$id/change/estimates.json"
        local m1_point="?" m1_lower="?" m1_upper="?"
        if [ -f "$est_file" ]; then
            read -r m1_point m1_lower m1_upper _ < <(python3 - "$est_file" "$noise" <<'PY'
import json, sys
try:
    with open(sys.argv[1]) as f:
        d = json.load(f)
    m = d["mean"]
    ci = m["confidence_interval"]
    print(f"{m['point_estimate']:+.2%} {ci['lower_bound']:+.2%} {ci['upper_bound']:+.2%} 0")
except Exception:
    print("? ? ? 0")
PY
)
        fi

        local escaped_id
        escaped_id=$(printf '%s' "$id" | sed 's/[.[\*^$()+?{|]/\\&/g')
        echo -e "  ${BLUE}[CONFIRM] Re-executing: ${id}...${NC}"

        local reexec_rc=0
        if [ -n "$bench_bin" ] && [ -x "$bench_bin" ]; then
            "${taskset_cmd[@]}" "${chrt_cmd[@]}" "$bench_bin" --bench --baseline "$baseline_name" "^${escaped_id}\$" 2>&1 | tee -a "$perf_log" || reexec_rc=$?
        else
            "${taskset_cmd[@]}" "${chrt_cmd[@]}" cargo bench --bench "$BENCH_SUITE" --features testing -- --baseline "$baseline_name" "^${escaped_id}\$" 2>&1 | tee -a "$perf_log" || reexec_rc=$?
        fi

        if [ "$reexec_rc" -ne 0 ] || [ ! -f "$est_file" ]; then
            echo "[CONFIRMATION] $id: 1st=[point=$m1_point, CI=($m1_lower, $m1_upper)] 2nd=EXEC_FAILED(rc=$reexec_rc) verdict=PERSISTED" >> "$perf_log"
            persisted_ids+=("$id")
            continue
        fi

        local m2_point m2_lower m2_upper is_reg
        read -r m2_point m2_lower m2_upper is_reg < <(python3 - "$est_file" "$noise" <<'PY'
import json, sys
path, noise = sys.argv[1], float(sys.argv[2])
try:
    with open(path) as f:
        d = json.load(f)
    m = d["mean"]
    ci = m["confidence_interval"]
    pt, lb, ub = m['point_estimate'], ci['lower_bound'], ci['upper_bound']
    reg = 1 if lb > noise else 0
    print(f"{pt:+.2%} {lb:+.2%} {ub:+.2%} {reg}")
except Exception:
    print("? ? ? 1")
PY
)
        if [ "$is_reg" -eq 1 ]; then
            echo "[CONFIRMATION] $id: 1st=[point=$m1_point, CI=($m1_lower, $m1_upper)] 2nd=[point=$m2_point, CI=($m2_lower, $m2_upper)] verdict=PERSISTED" >> "$perf_log"
            persisted_ids+=("$id")
        else
            echo "[CONFIRMATION] $id: 1st=[point=$m1_point, CI=($m1_lower, $m1_upper)] 2nd=[point=$m2_point, CI=($m2_lower, $m2_upper)] verdict=CLEAN" >> "$perf_log"
            clean_ids+=("$id")
        fi
    done

    if [ "${#persisted_ids[@]}" -gt 0 ]; then
        echo -e "${RED}[CONFIRM] Regression persisted on 2nd measurement: ${persisted_ids[*]}${NC}" >&2
        echo "TEST_RESULT[perf_regression]=FAIL:regression_detected"
        return 1
    else
        warn "Flake de regressão confirmado limpo por reexecução (${#clean_ids[@]} séries): ${clean_ids[*]}"
        echo "TEST_RESULT[perf_regression]=PASS_WITH_NOTE:flake_confirmed_clean"
        return 0
    fi
}

gate_capture=$(mktemp)
trap 'rm -f "$gate_capture"' EXIT

gate_rc=0
if [ -n "${NAM_MOCK_GATE_OUTPUT:-}" ] && [ -f "$NAM_MOCK_GATE_OUTPUT" ]; then
    cat "$NAM_MOCK_GATE_OUTPUT" | tee "$gate_capture"
    gate_rc="${NAM_MOCK_GATE_RC:-1}"
else
    run_perf_regression_gate \
        "$SCRIPT_DIR" \
        "$BENCH_SUITE" \
        "$PROJECT_DIR/benches/${BENCH_SUITE}.manifest" \
        "$@" 2>&1 | tee "$gate_capture" || gate_rc=${PIPESTATUS[0]}
fi

if [ "$gate_rc" -ne 0 ] && [ "$guard_mode" = "--check" ] && [ "${NAM_PERF_CONFIRM:-1}" != "0" ]; then
    if confirm_signaled_regressions "$gate_capture"; then
        gate_rc=0
    fi
fi

if [ "$guard_mode" = "--check" ] && [ "$guard_enabled" -eq 1 ]; then
    if ! perf_stability_advisory; then
        warn "Baseline instável: séries acima variaram além da banda de ruído entre execuções consecutivas; re-registre o baseline (--bootstrap-baseline) em host estabilizado antes de confiar neste veredito."
        if [ "${NAM_PERF_STABILITY_STRICT:-0}" = "1" ]; then
            echo "TEST_RESULT[perf_regression]=FAIL:baseline_unstable"
            exit 1
        fi
    fi
fi
exit "$gate_rc"

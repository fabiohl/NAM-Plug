#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# LEGACY performance regression gate for NAM-Plug (pre engine-gate adoption).
# Preserved verbatim for the `--legacy` rollback window: it aggregates a global
# mean/std-dev from clap_bench Criterion estimates and synthesizes p99/max,
# comparing against a `.performance-baselines/<host>-<cpu>.json` snapshot. The
# canonical gate is now `utils/tests-performance-regression.sh`, which drives the
# engine `nam_perf_gate` (per-benchmark coverage + machine verdict).
#
# Original header:
# Performance regression gate for NAM-Plug.
# Relative performance defense comparing current runs against a persisted baseline.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$SCRIPT_DIR/lib/_lib.sh" ]; then
    source "$SCRIPT_DIR/lib/_lib.sh"
fi

MODE="--check"
SIMULATE=0

for arg in "$@"; do
    case "$arg" in
        --bootstrap-baseline)
            MODE="--bootstrap-baseline"
            ;;
        --check)
            MODE="--check"
            ;;
        --simulate|--dry-run)
            SIMULATE=1
            ;;
        --help|-h)
            echo "Usage: $0 [--check|--bootstrap-baseline] [--simulate]"
            exit 0
            ;;
        *)
            echo "Unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

pick_bench_core() {
    if [ -n "${NAM_BENCH_CORE:-}" ]; then
        echo "$NAM_BENCH_CORE"
        return 0
    fi
    local iso="/sys/devices/system/cpu/isolated"
    if [ -f "$iso" ]; then
        local raw
        raw=$(cat "$iso" 2>/dev/null | tr -d '[:space:]')
        if [ -n "$raw" ]; then
            echo "${raw%%[-,]*}"
            return 0
        fi
    fi
    echo $(( $(nproc 2>/dev/null || echo 1) / 2 ))
}

BENCH_CORE="${NAM_BENCH_CORE:-$(pick_bench_core)}"
TASKSET=()
if command -v taskset >/dev/null 2>&1; then
    TASKSET=(taskset -c "${BENCH_CORE}")
fi

HOSTNAME_STR=$(hostname -s 2>/dev/null || hostname 2>/dev/null || echo "localhost")
CPU_HASH=$(grep -m1 "model name" /proc/cpuinfo 2>/dev/null | sha256sum | awk '{print substr($1,1,8)}' || echo "generic")
BASELINE_DIR=".performance-baselines"
BASELINE_FILE="${BASELINE_DIR}/${HOSTNAME_STR}-${CPU_HASH}.json"
mkdir -p "$BASELINE_DIR"

is_calibrated_environment() {
    if [ "${NAM_BENCH_FORCE_UNCALIBRATED:-0}" = "1" ]; then
        return 1
    fi
    if [ "${NAM_BENCH_FORCE_CALIBRATED:-0}" = "1" ]; then
        return 0
    fi
    local iso="/sys/devices/system/cpu/isolated"
    if [ -f "$iso" ]; then
        local raw
        raw=$(cat "$iso" 2>/dev/null | tr -d '[:space:]')
        if [ -n "$raw" ]; then
            return 0
        fi
    fi
    return 1
}

run_single_measurement() {
    if [ "$SIMULATE" = "1" ]; then
        echo "12500 14200 18500"
        return 0
    fi
    local bench_filter="${NAM_BENCH_FILTER:-}"
    local cmd=("cargo" "bench" "--bench" "clap_bench")
    if [ -n "$bench_filter" ]; then
        cmd+=("--" "$bench_filter")
    fi
    local out
    out=$("${TASKSET[@]}" "${cmd[@]}" 2>&1)

    local est_files
    est_files=$(find target/criterion -name "estimates.json" -path "*/new/*" 2>/dev/null || true)
    if [ -n "$est_files" ]; then
        local agg
        agg=$(awk '
            /"mean":/ { in_mean=1 }
            /"point_estimate":/ && in_mean {
                gsub(/[^0-9.]/, "", $0)
                sum_mean += $0; count++
                in_mean=0
            }
            /"std_dev":/ { in_sd=1 }
            /"point_estimate":/ && in_sd {
                gsub(/[^0-9.]/, "", $0)
                sum_sd += $0
                in_sd=0
            }
            END {
                if (count > 0) {
                    m = sum_mean / count
                    sd = sum_sd / count
                    p99 = m + 2.33 * sd
                    mx = m + 3.0 * sd
                    printf("%.0f %.0f %.0f\n", m, p99, mx)
                }
            }
        ' $est_files)
        if [ -n "$agg" ]; then
            echo "$agg"
            return 0
        fi
    fi

    local parsed
    parsed=$(echo "$out" | grep 'time:[[:space:]]*\[' | tail -n 1 || true)
    if [ -n "$parsed" ]; then
        local m unit factor
        m=$(echo "$parsed" | awk '{print $4}')
        unit=$(echo "$parsed" | awk '{print $5}')
        case "$unit" in
            ns) factor=1 ;;
            us|µs) factor=1000 ;;
            ms) factor=1000000 ;;
            s) factor=1000000000 ;;
            *) factor=1 ;;
        esac
        local m_ns p_ns mx_ns
        m_ns=$(awk -v val="$m" -v f="$factor" 'BEGIN { printf("%.0f\n", val * f) }')
        p_ns=$(awk -v val="$m_ns" 'BEGIN { printf("%.0f\n", val * 1.08) }')
        mx_ns=$(awk -v val="$m_ns" 'BEGIN { printf("%.0f\n", val * 1.20) }')
        echo "$m_ns $p_ns $mx_ns"
        return 0
    fi

    echo "ERROR: Could not extract criterion metrics from clap_bench run" >&2
    return 1
}

if [ "$MODE" = "--bootstrap-baseline" ]; then
    echo "Bootstrapping baseline for NAM-Plug on core ${BENCH_CORE} (5 iterations)..."
    means=()
    p99s=()
    maxs=()
    for i in {1..5}; do
        res=$(run_single_measurement)
        read -r m p mx <<< "$res"
        echo "  [Run $i/5] mean_ns=$m, p99_ns=$p, max_ns=$mx"
        means+=("$m")
        p99s+=("$p")
        maxs+=("$mx")
    done

    sorted_means=($(printf '%s\n' "${means[@]}" | sort -n))
    sorted_p99s=($(printf '%s\n' "${p99s[@]}" | sort -n))
    sorted_maxs=($(printf '%s\n' "${maxs[@]}" | sort -n))
    med_mean="${sorted_means[2]}"
    med_p99="${sorted_p99s[2]}"
    med_max="${sorted_maxs[2]}"

    cat << EOF > "$BASELINE_FILE"
{
  "hostname": "$HOSTNAME_STR",
  "cpu_model_hash": "$CPU_HASH",
  "bench_target": "clap_bench",
  "bench_core": "$BENCH_CORE",
  "runs": 5,
  "mean_ns": $med_mean,
  "p99_ns": $med_p99,
  "max_ns": $med_max
}
EOF
    echo "✓ Baseline saved to $BASELINE_FILE: mean_ns=$med_mean, p99_ns=$med_p99, max_ns=$med_max"
    exit 0
fi

# Mode: --check
if ! is_calibrated_environment; then
    echo "TEST_RESULT[perf_regression]=GAP:uncalibrated_environment"
    echo "WARN GAP: uncalibrated environment (isolated CPU core required; exit 2)" >&2
    exit 2
fi

if [ ! -f "$BASELINE_FILE" ]; then
    echo "TEST_RESULT[perf_regression]=FAIL baseline_missing"
    echo "ERROR: baseline file $BASELINE_FILE does not exist. Run with --bootstrap-baseline first." >&2
    exit 1
fi

base_mean=$(sed -n 's/.*"mean_ns":[[:space:]]*\([0-9.]\+\).*/\1/p' "$BASELINE_FILE")
base_p99=$(sed -n 's/.*"p99_ns":[[:space:]]*\([0-9.]\+\).*/\1/p' "$BASELINE_FILE")
echo "Evaluating against baseline $BASELINE_FILE (base_mean=$base_mean ns, base_p99=$base_p99 ns)..."

regress_count=0
for i in {1..5}; do
    res=$(run_single_measurement)
    read -r m p mx <<< "$res"
    is_regressed=$(awk -v m="$m" -v bm="$base_mean" -v p="$p" -v bp="$base_p99" 'BEGIN {
        if (p > bp * 1.10 || m > bm * 1.05) print 1; else print 0;
    }')
    if [ "$is_regressed" -eq 1 ]; then
        echo "  [Run $i/5] mean_ns=$m, p99_ns=$p — REGRESSION (p99 > +10% or mean > +5%)"
        regress_count=$((regress_count + 1))
    else
        echo "  [Run $i/5] mean_ns=$m, p99_ns=$p — OK"
    fi
done

if [ "$regress_count" -ge 3 ]; then
    echo "TEST_RESULT[perf_regression]=FAIL ($regress_count/5 runs regressed)"
    exit 1
fi

echo "TEST_RESULT[perf_regression]=PASS"
exit 0

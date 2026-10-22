#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Deterministic regression test for bootstrap post-baseline --check policy (D5).
# Verifies non-fatal self-consistency flake handling vs strict mode and baseline failure.

set -euo pipefail

unset NAM_BOOTSTRAP_FULL_AUDIT NAM_BOOTSTRAP_STRICT_PERF NAM_BOOTSTRAP_CHECK_PERF NAM_BOOTSTRAP_SKIP_PERF_CHECK || true

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Locate bootstrap-repo.sh via environment override or system path
BOOTSTRAP_SCRIPT="${NAM_BOOTSTRAP_SCRIPT:-$(command -v bootstrap-repo.sh || true)}"

if [ -z "$BOOTSTRAP_SCRIPT" ] || [ ! -f "$BOOTSTRAP_SCRIPT" ]; then
    echo "NOTICE: bootstrap-repo.sh not found (set NAM_BOOTSTRAP_SCRIPT to run); skipping."
    exit 0
fi

# ANSI style helpers
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD}    test_bootstrap_perf_policy: post-bootstrap --check (D5)    ${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

# Extract the performance regression gate block from bootstrap-repo.sh
PERF_GATE_CODE=$(sed -n '/if \[ -x "utils\/tests-performance-regression.sh" \]; then/,/perf_gate_ran=1/{p; /perf_gate_ran=1/ { n; p; q; }}' "$BOOTSTRAP_SCRIPT")

if [ -z "$PERF_GATE_CODE" ]; then
    echo -e "${RED}ERROR: Failed to extract performance regression gate block from $BOOTSTRAP_SCRIPT${NC}" >&2
    exit 1
fi

# Create mini-repo in temp dir
MINI_REPO="$TMP_DIR/mini-repo"
mkdir -p "$MINI_REPO/utils"
STUB_SCRIPT="$MINI_REPO/utils/tests-performance-regression.sh"

cat <<'EOF' > "$STUB_SCRIPT"
#!/bin/bash
mode="${1:-}"
if [ "$mode" = "--bootstrap-baseline" ]; then
    if [ "${STUB_BASELINE_FAIL:-0}" = "1" ]; then
        echo "Stub: --bootstrap-baseline failure simulated" >&2
        exit 1
    fi
    echo "Stub: --bootstrap-baseline ok"
    exit 0
elif [ "$mode" = "--check" ]; then
    if [ "${STUB_CHECK_FAIL:-1}" = "1" ]; then
        echo "REGRESSION_DETECTED DryDelay_Mono/64_samp_d0 relative mean +10.5%" >&2
        exit 1
    fi
    echo "Stub: --check ok"
    exit 0
fi
echo "Stub: unknown mode $mode" >&2
exit 2
EOF
chmod +x "$STUB_SCRIPT"

# Helper to execute the extracted block inside the mini-repo
run_perf_policy() {
    (
        cd "$MINI_REPO"
        run_rt() { "$@"; }
        ran_any_bench=0
        perf_gate_ran=0
        eval "$PERF_GATE_CODE"
    )
}

# ── Case (a): Default mode (--check fails -> WARN GAP, exit 0) ───────────────
echo -e "\n${BLUE}[1/8] Testing Case (a): Default mode (self-consistency check failure is non-fatal)...${NC}"
STDERR_OUT="$TMP_DIR/case_a_stderr.log"
STUB_BASELINE_FAIL=0 STUB_CHECK_FAIL=1 export NAM_BOOTSTRAP_CHECK_PERF=1; unset NAM_BOOTSTRAP_STRICT_PERF || true

set +e
(
    cd "$MINI_REPO"
    run_rt() { "$@"; }
    ran_any_bench=0
    perf_gate_ran=0
    eval "$PERF_GATE_CODE"
) 2> "$STDERR_OUT"
rc=$?
set -e

if [ "$rc" -ne 0 ]; then
    echo -e "${RED}FAIL: Case (a) exited with code $rc (expected 0)${NC}" >&2
    cat "$STDERR_OUT" >&2
    exit 1
fi
if ! grep -q "WARN GAP: perf_self_consistency_check_failed" "$STDERR_OUT"; then
    echo -e "${RED}FAIL: Case (a) did not output expected WARN GAP message${NC}" >&2
    cat "$STDERR_OUT" >&2
    exit 1
fi
echo -e "  ${GREEN}✓ Default mode recorded WARN GAP and returned 0 (non-fatal).${NC}"

# ── Case (b): Strict mode (NAM_BOOTSTRAP_STRICT_PERF=1 -> FATAL, exit != 0) ──
echo -e "\n${BLUE}[2/8] Testing Case (b): Strict mode (NAM_BOOTSTRAP_STRICT_PERF=1 is fatal)...${NC}"
STDERR_OUT="$TMP_DIR/case_b_stderr.log"
set +e
(
    cd "$MINI_REPO"
    run_rt() { "$@"; }
    ran_any_bench=0
    perf_gate_ran=0
    export NAM_BOOTSTRAP_CHECK_PERF=1
    export NAM_BOOTSTRAP_STRICT_PERF=1
    eval "$PERF_GATE_CODE"
) 2> "$STDERR_OUT"
rc=$?
set -e

if [ "$rc" -eq 0 ]; then
    echo -e "${RED}FAIL: Case (b) unexpectedly succeeded under strict mode!${NC}" >&2
    cat "$STDERR_OUT" >&2
    exit 1
fi
if ! grep -q "\[FATAL\] performance-regression --check falhou\!" "$STDERR_OUT"; then
    echo -e "${RED}FAIL: Case (b) did not output expected [FATAL] message${NC}" >&2
    cat "$STDERR_OUT" >&2
    exit 1
fi
echo -e "  ${GREEN}✓ Strict mode aborted with fatal error on check failure (exit $rc).${NC}"

# ── Case (c): Baseline creation failure -> ALWAYS FATAL (exit != 0) ──────────
echo -e "\n${BLUE}[3/8] Testing Case (c): Baseline failure is always fatal...${NC}"
STDERR_OUT="$TMP_DIR/case_c_stderr.log"
set +e
(
    cd "$MINI_REPO"
    run_rt() { "$@"; }
    ran_any_bench=0
    perf_gate_ran=0
    export STUB_BASELINE_FAIL=1
    unset NAM_BOOTSTRAP_STRICT_PERF || true
    eval "$PERF_GATE_CODE"
) 2> "$STDERR_OUT"
rc=$?
set -e

if [ "$rc" -eq 0 ]; then
    echo -e "${RED}FAIL: Case (c) unexpectedly succeeded when baseline failed!${NC}" >&2
    cat "$STDERR_OUT" >&2
    exit 1
fi
if ! grep -q "\[FATAL\] performance-regression --bootstrap-baseline falhou\!" "$STDERR_OUT"; then
    echo -e "${RED}FAIL: Case (c) did not output expected [FATAL] baseline failure message${NC}" >&2
    cat "$STDERR_OUT" >&2
    exit 1
fi
echo -e "  ${GREEN}✓ Baseline failure is always fatal regardless of strict setting (exit $rc).${NC}"

# ── Case (d): Preserved compatible baseline -> restore + default --check ─────
echo -e "\n${BLUE}[4/8] Testing Case (d): preserved compatible baseline is restored and checked by default...${NC}"
CASE_D_REPO="$TMP_DIR/mini-repo-d"
cp -a "$MINI_REPO" "$CASE_D_REPO"
BACKUP_DIR="$TMP_DIR/previous-baseline"
mkdir -p "$CASE_D_REPO/.performance-baselines" "$BACKUP_DIR"
printf '{"stub":"fresh"}\n' > "$CASE_D_REPO/.performance-baselines/baseline-fingerprint.json"
printf '{"stub":"previous"}\n' > "$BACKUP_DIR/baseline-fingerprint.json"
OUT_D="$TMP_DIR/case_d.log"

set +e
(
    cd "$CASE_D_REPO"
    run_rt() { "$@"; }
    ran_any_bench=0
    perf_gate_ran=0
    perf_fingerprint_compatible() { return 0; }
    export PERF_BASELINE_BACKUP="$BACKUP_DIR"
    export NAM_PERF_BASELINE_CACHE="$TMP_DIR/case_d_cache"
    unset NAM_BOOTSTRAP_CHECK_PERF NAM_BOOTSTRAP_SKIP_PERF_CHECK NAM_BOOTSTRAP_STRICT_PERF || true
    STUB_BASELINE_FAIL=0 STUB_CHECK_FAIL=0 eval "$PERF_GATE_CODE"
) > "$OUT_D" 2>&1
rc=$?
set -e

if [ "$rc" -ne 0 ]; then
    echo -e "${RED}FAIL: Case (d) exited with code $rc (expected 0)${NC}" >&2
    cat "$OUT_D" >&2
    exit 1
fi
if ! grep -q "BASELINE RESTORED" "$OUT_D"; then
    echo -e "${RED}FAIL: Case (d) did not restore the preserved baseline${NC}" >&2
    cat "$OUT_D" >&2
    exit 1
fi
if ! grep -q "Stub: --check ok" "$OUT_D"; then
    echo -e "${RED}FAIL: Case (d) did not run the default --check against the preserved baseline${NC}" >&2
    cat "$OUT_D" >&2
    exit 1
fi
if ! grep -q '"stub":"previous"' "$CASE_D_REPO/.performance-baselines/baseline-fingerprint.json"; then
    echo -e "${RED}FAIL: Case (d) left the fresh baseline in place instead of the preserved one${NC}" >&2
    cat "$OUT_D" >&2
    exit 1
fi
if [ ! -f "$TMP_DIR/case_d_cache/current/baseline-fingerprint.json" ]; then
    echo -e "${RED}FAIL: Case (d) did not archive the fresh baseline under cache/current${NC}" >&2
    cat "$OUT_D" >&2
    exit 1
fi
echo -e "  ${GREEN}✓ Preserved baseline restored, fresh series archived, and default --check executed.${NC}"

# ── Case (e): incomparable preserved baseline -> NO_REFERENCE, no --check ────
make_preserved_repo() {
    local repo="$1"
    cp -a "$MINI_REPO" "$repo"
    mkdir -p "$repo/.performance-baselines" "$TMP_DIR/previous-baseline-$(basename "$repo")"
    printf '{"stub":"fresh"}\n' > "$repo/.performance-baselines/baseline-fingerprint.json"
    printf '{"stub":"previous"}\n' > "$TMP_DIR/previous-baseline-$(basename "$repo")/baseline-fingerprint.json"
}

run_preserved_block() {
    # run_preserved_block <repo> <out> : runs the gate block with incompatible fingerprint
    (
        cd "$1"
        run_rt() { "$@"; }
        ran_any_bench=0
        perf_gate_ran=0
        perf_fingerprint_compatible() { echo "BASELINE_INCOMPATIBLE: bench_binary_sha256"; return 1; }
        export PERF_BASELINE_BACKUP="$TMP_DIR/previous-baseline-$(basename "$1")"
        export NAM_PERF_BASELINE_CACHE="$TMP_DIR/cache-$(basename "$1")"
        STUB_BASELINE_FAIL=0 STUB_CHECK_FAIL=0 eval "$PERF_GATE_CODE"
    ) > "$2" 2>&1
}

echo -e "\n${BLUE}[5/8] Testing Case (e): incompatible baseline -> NO_REFERENCE, --check omitted (non-fatal by default)...${NC}"
CASE_E_REPO="$TMP_DIR/mini-repo-e"
make_preserved_repo "$CASE_E_REPO"
OUT_E="$TMP_DIR/case_e.log"
rc=0
unset NAM_BOOTSTRAP_CHECK_PERF NAM_BOOTSTRAP_SKIP_PERF_CHECK NAM_BOOTSTRAP_STRICT_PERF NAM_BOOTSTRAP_FULL_AUDIT || true
run_preserved_block "$CASE_E_REPO" "$OUT_E" || rc=$?
if [ "$rc" -ne 0 ]; then
    echo -e "${RED}FAIL: Case (e) exited with code $rc (expected 0)${NC}" >&2
    cat "$OUT_E" >&2
    exit 1
fi
if ! grep -q "BASELINE_INCOMPATIBLE: bench_binary_sha256" "$OUT_E" || ! grep -q "NO_REFERENCE" "$OUT_E"; then
    echo -e "${RED}FAIL: Case (e) did not record BASELINE_INCOMPATIBLE with NO_REFERENCE${NC}" >&2
    cat "$OUT_E" >&2
    exit 1
fi
if grep -q "Stub: --check ok" "$OUT_E" || grep -q "BASELINE RESTORED" "$OUT_E"; then
    echo -e "${RED}FAIL: Case (e) used an incomparable baseline for --check${NC}" >&2
    cat "$OUT_E" >&2
    exit 1
fi
echo -e "  ${GREEN}✓ Incomparable baseline: verdict NO_REFERENCE, no --check, non-fatal by default.${NC}"

# ── Case (f): incompatible baseline is fatal under full audit / strict perf ──
echo -e "\n${BLUE}[6/8] Testing Case (f): incompatible baseline is fatal under NAM_BOOTSTRAP_FULL_AUDIT=1 and NAM_BOOTSTRAP_STRICT_PERF=1...${NC}"
for env_name in NAM_BOOTSTRAP_FULL_AUDIT NAM_BOOTSTRAP_STRICT_PERF; do
    CASE_F_REPO="$TMP_DIR/mini-repo-f-$env_name"
    make_preserved_repo "$CASE_F_REPO"
    OUT_F="$TMP_DIR/case_f_$env_name.log"
    rc=0
    export "$env_name=1"
    run_preserved_block "$CASE_F_REPO" "$OUT_F" || rc=$?
    unset "$env_name"
    if [ "$rc" -eq 0 ]; then
        echo -e "${RED}FAIL: Case (f) with $env_name=1 unexpectedly succeeded${NC}" >&2
        cat "$OUT_F" >&2
        exit 1
    fi
    if ! grep -q "\[FATAL\] BASELINE_INCOMPATIBLE: bench_binary_sha256" "$OUT_F"; then
        echo -e "${RED}FAIL: Case (f) with $env_name=1 did not emit the FATAL incompatibility message${NC}" >&2
        cat "$OUT_F" >&2
        exit 1
    fi
done
echo -e "  ${GREEN}✓ BASELINE_INCOMPATIBLE is fatal under full audit and under strict perf.${NC}"

# ── Case (g): NAM_BOOTSTRAP_STRICT_PERF truth table : 0, empty -> WARN GAP ─
echo -e "\n${BLUE}[7/8] Testing Case (g): NAM_BOOTSTRAP_STRICT_PERF=0 and empty are non-fatal; =1 is fatal...${NC}"
for strict_value in 0 "" 1; do
    STRICT_REPO="$TMP_DIR/mini-repo-strict-${strict_value:-empty}"
    cp -a "$MINI_REPO" "$STRICT_REPO"
    OUT_G="$TMP_DIR/case_g_${strict_value:-empty}.log"
    rc=0
    (
        cd "$STRICT_REPO"
        run_rt() { "$@"; }
        ran_any_bench=0
        perf_gate_ran=0
        export NAM_BOOTSTRAP_CHECK_PERF=1
        export NAM_BOOTSTRAP_STRICT_PERF="$strict_value"
        unset NAM_BOOTSTRAP_FULL_AUDIT || true
        STUB_BASELINE_FAIL=0 STUB_CHECK_FAIL=1 eval "$PERF_GATE_CODE"
    ) > "$OUT_G" 2>&1 || rc=$?
    if [ "$strict_value" = "1" ]; then
        if [ "$rc" -eq 0 ] || ! grep -q "\[FATAL\] performance-regression --check falhou" "$OUT_G"; then
            echo -e "${RED}FAIL: Case (g) STRICT_PERF=1 must be fatal (rc=$rc)${NC}" >&2
            cat "$OUT_G" >&2
            exit 1
        fi
    else
        if [ "$rc" -ne 0 ] || ! grep -q "WARN GAP: perf_self_consistency_check_failed" "$OUT_G"; then
            echo -e "${RED}FAIL: Case (g) STRICT_PERF='${strict_value}' must be non-fatal WARN GAP (rc=$rc)${NC}" >&2
            cat "$OUT_G" >&2
            exit 1
        fi
    fi
done
echo -e "  ${GREEN}✓ STRICT_PERF semantics: only \"1\" is strict (0 and empty are WARN GAP).${NC}"

# ── Case (h): full audit makes a failing --check fatal ───────────────────────
echo -e "\n${BLUE}[8/8] Testing Case (h): NAM_BOOTSTRAP_FULL_AUDIT=1 makes a failing --check fatal...${NC}"
FULL_REPO="$TMP_DIR/mini-repo-full"
cp -a "$MINI_REPO" "$FULL_REPO"
OUT_H="$TMP_DIR/case_h.log"
rc=0
(
    cd "$FULL_REPO"
    run_rt() { "$@"; }
    ran_any_bench=0
    perf_gate_ran=0
    export NAM_BOOTSTRAP_CHECK_PERF=1
    export NAM_BOOTSTRAP_FULL_AUDIT=1
    unset NAM_BOOTSTRAP_STRICT_PERF || true
    STUB_BASELINE_FAIL=0 STUB_CHECK_FAIL=1 eval "$PERF_GATE_CODE"
) > "$OUT_H" 2>&1 || rc=$?
if [ "$rc" -eq 0 ] || ! grep -q "\[FATAL\] performance-regression --check falhou" "$OUT_H"; then
    echo -e "${RED}FAIL: Case (h) full audit must make the failing --check fatal (rc=$rc)${NC}" >&2
    cat "$OUT_H" >&2
    exit 1
fi
echo -e "  ${GREEN}✓ Failing --check is fatal under full audit.${NC}"

echo -e "\n${GREEN}${BOLD}All 8 test_bootstrap_perf_policy scenarios passed!${NC}"

#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Deterministic regression test for bootstrap post-baseline --check policy (D5).
# Verifies non-fatal self-consistency flake handling vs strict mode and baseline failure.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Locate bootstrap-repo.sh
BOOTSTRAP_SCRIPT=""
for candidate in "$PROJECT_DIR/../.agents/scripts/bootstrap-repo.sh" "$HOME/NAM/.agents/scripts/bootstrap-repo.sh"; do
    if [ -f "$candidate" ]; then
        BOOTSTRAP_SCRIPT="$candidate"
        break
    fi
done

if [ -z "$BOOTSTRAP_SCRIPT" ]; then
    echo "ERROR: bootstrap-repo.sh not found!" >&2
    exit 1
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
echo -e "\n${BLUE}[1/3] Testing Case (a): Default mode (self-consistency check failure is non-fatal)...${NC}"
STDERR_OUT="$TMP_DIR/case_a_stderr.log"
STUB_BASELINE_FAIL=0 STUB_CHECK_FAIL=1 unset NAM_BOOTSTRAP_STRICT_PERF || true

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
echo -e "\n${BLUE}[2/3] Testing Case (b): Strict mode (NAM_BOOTSTRAP_STRICT_PERF=1 is fatal)...${NC}"
STDERR_OUT="$TMP_DIR/case_b_stderr.log"
set +e
(
    cd "$MINI_REPO"
    run_rt() { "$@"; }
    ran_any_bench=0
    perf_gate_ran=0
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
echo -e "\n${BLUE}[3/3] Testing Case (c): Baseline failure is always fatal...${NC}"
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

echo -e "\n${GREEN}${BOLD}All 3 test_bootstrap_perf_policy scenarios passed!${NC}"

#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Aggregate runner for the shell contract tests under tests/scripts/.
#
# The runner discovers every `tests/scripts/test_*.sh` file so a new contract
# test cannot be orphaned, and additionally requires the regression scripts that
# guard the receipt, performance-baseline and codegen contracts to be present.
# A missing required script or any failing script makes the runner exit 1 with
# an aggregated report; callers (utils/lints.sh) therefore fail closed.
#
# Per-script output is kept under target/logs/test-scripts/ so a failure can be
# diagnosed without re-running the whole aggregate gate.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[0;33m'
BOLD='\033[1m'
NC='\033[0m'

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

REQUIRED_SCRIPTS=(
    test_bootstrap_perf_policy.sh
    test_ceremony_fail_closed.sh
    test_ceremony_reuse_binary.sh
    test_perf_confirmation.sh
    test_receipt_artifact_consistency.sh
    test_rt_codegen_post_bolt.sh
    test_tests_quick_harness.sh
    test_quick_receipt_contract.sh
)

cd "$PROJECT_DIR"

LOG_DIR="$PROJECT_DIR/target/logs/test-scripts"
REPORT="$LOG_DIR/report.txt"
mkdir -p "$LOG_DIR"
: > "$REPORT"

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD}    Shell contract tests (tests/scripts/test_*.sh)             ${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

missing_required=()
for required in "${REQUIRED_SCRIPTS[@]}"; do
    if [ ! -f "tests/scripts/$required" ]; then
        missing_required+=("$required")
    fi
done

if [ "${#missing_required[@]}" -gt 0 ]; then
    echo -e "${RED}${BOLD}FAIL: required contract tests are missing:${NC}" >&2
    printf '  - %s\n' "${missing_required[@]}" >&2
    {
        echo "TEST_SCRIPTS: FAIL missing_required=${#missing_required[@]}"
        printf 'MISSING: %s\n' "${missing_required[@]}"
    } | tee -a "$REPORT" >&2
    exit 1
fi

mapfile -t discovered < <(find tests/scripts -maxdepth 1 -type f -name 'test_*.sh' | sort)
if [ "${#discovered[@]}" -eq 0 ]; then
    echo -e "${RED}${BOLD}FAIL: no tests/scripts/test_*.sh files found.${NC}" >&2
    echo "TEST_SCRIPTS: FAIL discovered=0" | tee -a "$REPORT" >&2
    exit 1
fi

passed=0
failed=()
declare -A rc_by_script=()

for script in "${discovered[@]}"; do
    name="$(basename "$script")"
    log="$LOG_DIR/${name%.sh}.log"
    printf '  %b→%b %s ... ' "$BLUE" "$NC" "$name"

    start_ns=$(date +%s%N)
    set +e
    bash "$script" >"$log" 2>&1
    rc=$?
    set -e

    elapsed_ms=$(( ($(date +%s%N) - start_ns) / 1000000 ))
    if [ "$rc" -eq 0 ]; then
        passed=$((passed + 1))
        echo -e "${GREEN}PASS${NC} (${elapsed_ms} ms)"
        echo "PASS $name ${elapsed_ms}ms log=$log" >> "$REPORT"
    else
        failed+=("$name")
        rc_by_script["$name"]=$rc
        echo -e "${RED}FAIL${NC} (rc=$rc, ${elapsed_ms} ms)"
        echo "FAIL $name rc=$rc ${elapsed_ms}ms log=$log" >> "$REPORT"
        echo -e "${YELLOW}----- tail: $log -----${NC}" >&2
        tail -n 40 "$log" >&2 || true
        echo -e "${YELLOW}----------------------${NC}" >&2
    fi
done

total="${#discovered[@]}"
echo -e "\n${BLUE}${BOLD}---------------------------------------------------------------${NC}"
echo -e "  Script tests: ${GREEN}${passed} passed${NC}, ${RED}${#failed[@]} failed${NC} (total ${total})"
echo -e "${BLUE}${BOLD}---------------------------------------------------------------${NC}"

if [ "${#failed[@]}" -gt 0 ]; then
    echo -e "${RED}${BOLD}Failed contract tests:${NC}" >&2
    for name in "${failed[@]}"; do
        echo "  - $name (rc=${rc_by_script[$name]})" >&2
    done
    echo "TEST_SCRIPTS: FAIL passed=$passed failed=${#failed[@]} total=$total" | tee -a "$REPORT" >&2
    exit 1
fi

echo "TEST_SCRIPTS: PASS passed=$passed failed=0 total=$total" | tee -a "$REPORT"

#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Deterministic regression test for performance regression confirmation by re-execution (T2.2, D5).
# Verifies:
#   (a) 1st measurement bad + 2nd good -> PASS_WITH_NOTE:flake_confirmed_clean (exit 0)
#   (b) 1st bad + 2nd bad -> FAIL:regression_detected (exit 1)
#   (c) coverage failure -> FAIL:baseline_coverage_gap without confirmation attempt (exit 1)
#   (d) NAM_PERF_CONFIRM=0 -> disables confirmation, exits 1 on failure

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ANSI style helpers
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD}    test_perf_confirmation: re-execution confirmation (T2.2)    ${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

# Setup environment mocks
MOCK_GOVERNOR="$TMP_DIR/scaling_governor"
echo "performance" > "$MOCK_GOVERNOR"

STUB_BENCH="$TMP_DIR/stub_bench.sh"
CALL_LOG="$TMP_DIR/bench_calls.log"

cat <<'EOF' > "$STUB_BENCH"
#!/bin/bash
set -euo pipefail
# Target bench invocation format:
# taskset ... bench_bin --bench --baseline ci-baseline ^<escaped_id>$
SERIES_ID=""
for arg in "$@"; do
    if [[ "$arg" =~ ^\^.*\$ ]]; then
        SERIES_ID="${arg#^}"
        SERIES_ID="${SERIES_ID%\$}"
        # Unescape regex characters if any
        SERIES_ID="$(printf '%b' "$SERIES_ID")"
    fi
done

echo "STUB_BENCH: id='$SERIES_ID' mode='${STUB_MODE:-}' args='$*'" >> "$CALL_LOG"

if [ -n "$SERIES_ID" ] && [ -n "${MOCK_TARGET_DIR:-}" ]; then
    CHANGE_DIR="$MOCK_TARGET_DIR/criterion/$SERIES_ID/change"
    mkdir -p "$CHANGE_DIR"
    if [ "${STUB_MODE:-clean}" = "clean" ]; then
        # 2nd measurement: point=+1.0%, CI=[-1.0%, +3.0%] -> clean (lower_bound <= noise 0.05)
        cat <<JSON > "$CHANGE_DIR/estimates.json"
{"mean":{"confidence_interval":{"confidence_level":0.95,"lower_bound":-0.01,"upper_bound":0.03},"point_estimate":0.01,"standard_error":0.001},"median":{"confidence_interval":{"confidence_level":0.95,"lower_bound":-0.01,"upper_bound":0.03},"point_estimate":0.01,"standard_error":0.001}}
JSON
    elif [ "${STUB_MODE:-}" = "regressed" ]; then
        # 2nd measurement: point=+10.0%, CI=[+7.0%, +13.0%] -> regressed (lower_bound > noise 0.05)
        cat <<JSON > "$CHANGE_DIR/estimates.json"
{"mean":{"confidence_interval":{"confidence_level":0.95,"lower_bound":0.07,"upper_bound":0.13},"point_estimate":0.10,"standard_error":0.001},"median":{"confidence_interval":{"confidence_level":0.95,"lower_bound":0.07,"upper_bound":0.13},"point_estimate":0.10,"standard_error":0.001}}
JSON
    elif [ "${STUB_MODE:-}" = "crash" ]; then
        exit 2
    fi
fi
exit 0
EOF
chmod +x "$STUB_BENCH"

write_1st_estimates() {
    local target_dir="$1"
    local id="$2"
    local change_dir="$target_dir/criterion/$id/change"
    mkdir -p "$change_dir"
    cat <<JSON > "$change_dir/estimates.json"
{"mean":{"confidence_interval":{"confidence_level":0.95,"lower_bound":0.08,"upper_bound":0.14},"point_estimate":0.11,"standard_error":0.001},"median":{"confidence_interval":{"confidence_level":0.95,"lower_bound":0.08,"upper_bound":0.14},"point_estimate":0.11,"standard_error":0.001}}
JSON
}

# ── Case (a): 1st measurement bad + 2nd good -> PASS_WITH_NOTE ───────────────
echo -e "\n${BLUE}[1/4] Testing Case (a): 1st bad + 2nd good -> PASS_WITH_NOTE:flake_confirmed_clean...${NC}"
CASE_A_DIR="$TMP_DIR/case_a"
mkdir -p "$CASE_A_DIR"
write_1st_estimates "$CASE_A_DIR" "CLAP_WorstCase_DryDelay_Mono/64_samp_d0"

MOCK_GATE_OUT="$CASE_A_DIR/gate_out.txt"
cat <<'EOF' > "$MOCK_GATE_OUT"
REGRESSION_DETECTED CLAP_WorstCase_DryDelay_Mono/64_samp_d0 relative mean +11.00%
TEST_RESULT[perf_regression]=FAIL:regression_detected
EOF

: > "$CALL_LOG"
set +e
OUT_A=$(
    export NAM_GOVERNOR_FILE="$MOCK_GOVERNOR"
    export NAM_BENCH_EXE="$STUB_BENCH"
    export NAM_BENCH_CORE=0
    export CARGO_TARGET_DIR="$CASE_A_DIR"
    export MOCK_TARGET_DIR="$CASE_A_DIR"
    export CALL_LOG="$CALL_LOG"
    export STUB_MODE="clean"
    export NAM_MOCK_GATE_OUTPUT="$MOCK_GATE_OUT"
    export NAM_MOCK_GATE_RC=1
    export NAM_PERF_CONFIRM=1
    "$PROJECT_DIR/utils/tests-performance-regression.sh" --check 2>&1
)
RC_A=$?
set -e

if [ "$RC_A" -ne 0 ]; then
    echo -e "${RED}[FAIL] Case (a) expected exit 0, got $RC_A. Output:\n$OUT_A${NC}" >&2
    exit 1
fi
if ! echo "$OUT_A" | grep -qF "TEST_RESULT[perf_regression]=PASS_WITH_NOTE:flake_confirmed_clean"; then
    echo -e "${RED}[FAIL] Case (a) missing PASS_WITH_NOTE marker. Output:\n$OUT_A${NC}" >&2
    exit 1
fi
if ! grep -qF "CLAP_WorstCase_DryDelay_Mono/64_samp_d0" "$CALL_LOG"; then
    echo -e "${RED}[FAIL] Case (a) benchmark re-execution was not called.${NC}" >&2
    exit 1
fi
if ! grep -qF "verdict=CLEAN" "$CASE_A_DIR/logs/perf-regression.log"; then
    echo -e "${RED}[FAIL] Case (a) audit log missing CLEAN entry.${NC}" >&2
    exit 1
fi
echo -e "${GREEN}[OK] Case (a) passed successfully.${NC}"

# ── Case (b): 1st bad + 2nd bad -> FAIL:regression_detected ──────────────────
echo -e "\n${BLUE}[2/4] Testing Case (b): 1st bad + 2nd bad -> FAIL:regression_detected...${NC}"
CASE_B_DIR="$TMP_DIR/case_b"
mkdir -p "$CASE_B_DIR"
write_1st_estimates "$CASE_B_DIR" "CLAP_WorstCase_DryDelay_Mono/64_samp_d0"

MOCK_GATE_OUT_B="$CASE_B_DIR/gate_out.txt"
cat <<'EOF' > "$MOCK_GATE_OUT_B"
REGRESSION_DETECTED CLAP_WorstCase_DryDelay_Mono/64_samp_d0 relative mean +11.00%
TEST_RESULT[perf_regression]=FAIL:regression_detected
EOF

: > "$CALL_LOG"
set +e
OUT_B=$(
    export NAM_GOVERNOR_FILE="$MOCK_GOVERNOR"
    export NAM_BENCH_EXE="$STUB_BENCH"
    export NAM_BENCH_CORE=0
    export CARGO_TARGET_DIR="$CASE_B_DIR"
    export MOCK_TARGET_DIR="$CASE_B_DIR"
    export CALL_LOG="$CALL_LOG"
    export STUB_MODE="regressed"
    export NAM_MOCK_GATE_OUTPUT="$MOCK_GATE_OUT_B"
    export NAM_MOCK_GATE_RC=1
    export NAM_PERF_CONFIRM=1
    "$PROJECT_DIR/utils/tests-performance-regression.sh" --check 2>&1
)
RC_B=$?
set -e

if [ "$RC_B" -eq 0 ]; then
    echo -e "${RED}[FAIL] Case (b) expected non-zero exit, got 0. Output:\n$OUT_B${NC}" >&2
    exit 1
fi
if ! echo "$OUT_B" | grep -qF "TEST_RESULT[perf_regression]=FAIL:regression_detected"; then
    echo -e "${RED}[FAIL] Case (b) missing FAIL:regression_detected marker. Output:\n$OUT_B${NC}" >&2
    exit 1
fi
if ! grep -qF "verdict=PERSISTED" "$CASE_B_DIR/logs/perf-regression.log"; then
    echo -e "${RED}[FAIL] Case (b) audit log missing PERSISTED entry.${NC}" >&2
    exit 1
fi
echo -e "${GREEN}[OK] Case (b) passed successfully.${NC}"

# ── Case (c): Coverage failure -> FAIL without confirmation attempt ──────────
echo -e "\n${BLUE}[3/4] Testing Case (c): Coverage failure -> FAIL without confirmation attempt...${NC}"
CASE_C_DIR="$TMP_DIR/case_c"
mkdir -p "$CASE_C_DIR"

MOCK_GATE_OUT_C="$CASE_C_DIR/gate_out.txt"
cat <<'EOF' > "$MOCK_GATE_OUT_C"
REGRESSION_DETECTED CLAP_WorstCase_DryDelay_Mono/64_samp_d0 relative mean +11.00%
FAIL:baseline_coverage_gap
TEST_RESULT[perf_regression]=FAIL:baseline_coverage_gap
EOF

: > "$CALL_LOG"
set +e
OUT_C=$(
    export NAM_GOVERNOR_FILE="$MOCK_GOVERNOR"
    export NAM_BENCH_EXE="$STUB_BENCH"
    export NAM_BENCH_CORE=0
    export CARGO_TARGET_DIR="$CASE_C_DIR"
    export MOCK_TARGET_DIR="$CASE_C_DIR"
    export CALL_LOG="$CALL_LOG"
    export STUB_MODE="clean"
    export NAM_MOCK_GATE_OUTPUT="$MOCK_GATE_OUT_C"
    export NAM_MOCK_GATE_RC=1
    export NAM_PERF_CONFIRM=1
    "$PROJECT_DIR/utils/tests-performance-regression.sh" --check 2>&1
)
RC_C=$?
set -e

if [ "$RC_C" -eq 0 ]; then
    echo -e "${RED}[FAIL] Case (c) expected non-zero exit, got 0. Output:\n$OUT_C${NC}" >&2
    exit 1
fi
if [ -s "$CALL_LOG" ]; then
    echo -e "${RED}[FAIL] Case (c) benchmark stub was invoked despite coverage failure! Log:\n$(cat "$CALL_LOG")${NC}" >&2
    exit 1
fi
echo -e "${GREEN}[OK] Case (c) passed successfully.${NC}"

# ── Case (d): NAM_PERF_CONFIRM=0 disables confirmation ───────────────────────
echo -e "\n${BLUE}[4/4] Testing Case (d): NAM_PERF_CONFIRM=0 disables confirmation...${NC}"
CASE_D_DIR="$TMP_DIR/case_d"
mkdir -p "$CASE_D_DIR"
write_1st_estimates "$CASE_D_DIR" "CLAP_WorstCase_DryDelay_Mono/64_samp_d0"

MOCK_GATE_OUT_D="$CASE_D_DIR/gate_out.txt"
cat <<'EOF' > "$MOCK_GATE_OUT_D"
REGRESSION_DETECTED CLAP_WorstCase_DryDelay_Mono/64_samp_d0 relative mean +11.00%
TEST_RESULT[perf_regression]=FAIL:regression_detected
EOF

: > "$CALL_LOG"
set +e
OUT_D=$(
    export NAM_GOVERNOR_FILE="$MOCK_GOVERNOR"
    export NAM_BENCH_EXE="$STUB_BENCH"
    export NAM_BENCH_CORE=0
    export CARGO_TARGET_DIR="$CASE_D_DIR"
    export MOCK_TARGET_DIR="$CASE_D_DIR"
    export CALL_LOG="$CALL_LOG"
    export STUB_MODE="clean"
    export NAM_MOCK_GATE_OUTPUT="$MOCK_GATE_OUT_D"
    export NAM_MOCK_GATE_RC=1
    export NAM_PERF_CONFIRM=0
    "$PROJECT_DIR/utils/tests-performance-regression.sh" --check 2>&1
)
RC_D=$?
set -e

if [ "$RC_D" -eq 0 ]; then
    echo -e "${RED}[FAIL] Case (d) expected non-zero exit with confirmation disabled, got 0. Output:\n$OUT_D${NC}" >&2
    exit 1
fi
if [ -s "$CALL_LOG" ]; then
    echo -e "${RED}[FAIL] Case (d) benchmark stub was invoked despite NAM_PERF_CONFIRM=0! Log:\n$(cat "$CALL_LOG")${NC}" >&2
    exit 1
fi
echo -e "${GREEN}[OK] Case (d) passed successfully.${NC}"

echo -e "\n${GREEN}${BOLD}All test_perf_confirmation cases passed successfully!${NC}"
exit 0

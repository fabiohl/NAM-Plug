#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Contract test for the fail-closed release ceremony (Sprint S2, F-CER-02).
#
# In --release-ceremony mode the process exit code must be 0 if and only if the
# receipt status is CERTIFIED. Any other status exits CEREMONY_FAIL_EXIT (3).
# Outside ceremony mode the preliminary run keeps exit 0.
#   (a) CERTIFIED + ceremony              -> 0
#   (b) INCOMPLETE + ceremony             -> 3
#   (c) FUNCTIONALLY_CERTIFIED + ceremony -> 3
#   (d) INCOMPLETE without ceremony       -> 0
#   (e) empty status + ceremony           -> 3 (fail closed)
#   (f) FUNCTIONALLY_CERTIFIED w/o ceremony -> 0
#
# The gate block is extracted from utils/build-release.sh and executed as-is,
# so removing the `exit` from the real call site makes this test fail.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

BUILD_RELEASE="$PROJECT_DIR/utils/build-release.sh"
LIB_FILE="$PROJECT_DIR/utils/lib/_lib.sh"

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD}  test_ceremony_fail_closed: exit 3 unless CERTIFIED (F-CER-02)${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

# 1. Helper contract (pure function, no side effects).
unit_expect() {
    local label="$1" expected="$2" status="$3" ceremony="$4"
    local got
    got=$(NAM_LIB_NO_CD=1 source "$LIB_FILE" && ceremony_exit_code "$status" "$ceremony")
    if [ "$got" != "$expected" ]; then
        echo -e "${RED}FAIL: ceremony_exit_code [$label] expected $expected, got $got${NC}" >&2
        exit 1
    fi
    echo -e "  ${GREEN}✓${NC} ceremony_exit_code: $label -> $got"
}

unit_expect "CERTIFIED + ceremony" 0 CERTIFIED true
unit_expect "INCOMPLETE + ceremony" 3 INCOMPLETE true
unit_expect "FUNCTIONALLY_CERTIFIED + ceremony" 3 FUNCTIONALLY_CERTIFIED true
unit_expect "INCOMPLETE, no ceremony" 0 INCOMPLETE false
unit_expect "empty status + ceremony" 3 "" true

# 2. Real call-site block from build-release.sh, executed end-to-end.
read -r START_LINE END_LINE < <(python3 - "$BUILD_RELEASE" <<'PY'
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    lines = f.readlines()

start = [i for i, l in enumerate(lines) if 'CEREMONY_EXIT=$(ceremony_exit_code' in l]
if len(start) != 1:
    print("0 0")
    sys.exit(0)
start = start[0]
mv = [i for i, l in enumerate(lines) if 'mv -f "$RECEIPT_TMP" "$RECEIPT_TARGET"' in l]
if len(mv) != 1 or mv[0] > start:
    print("0 0")
    sys.exit(0)
end = next(i for i in range(start, len(lines)) if lines[i].rstrip("\n") == "fi")
print(f"{start + 1} {end + 1}")
PY
)

if [ "$START_LINE" -eq 0 ]; then
    echo -e "${RED}FAIL: ceremony gate block not found in build-release.sh (after receipt mv).${NC}" >&2
    exit 1
fi

GATE_BLOCK="$TMP_DIR/gate-block.sh"
sed -n "${START_LINE},${END_LINE}p" "$BUILD_RELEASE" > "$GATE_BLOCK"

if ! grep -q 'exit "\$CEREMONY_EXIT"' "$GATE_BLOCK"; then
    echo -e "${RED}FAIL: ceremony gate block does not exit with CEREMONY_EXIT.${NC}" >&2
    cat "$GATE_BLOCK" >&2
    exit 1
fi

run_gate() {
    local label="$1" expected_rc="$2" status="$3" ceremony="$4"
    local harness="$TMP_DIR/harness.sh" rc out
    {
        echo '#!/bin/bash'
        echo 'set -euo pipefail'
        printf 'PROJECT_DIR=%q\n' "$PROJECT_DIR"
        echo 'NAM_LIB_NO_CD=1 source "$PROJECT_DIR/utils/lib/_lib.sh"'
        printf 'RECEIPT_STATUS=%q\n' "$status"
        printf 'RELEASE_CEREMONY=%q\n' "$ceremony"
        echo 'GATE_SKIPS_JOINED="gui_tests:unverified|artifact_consistency:absent"'
        echo 'RECEIPT_TARGET="/tmp/nam-plug-test-receipt.json"'
        cat "$GATE_BLOCK"
        echo 'echo "GATE_SURVIVED"'
    } > "$harness"
    set +e
    out=$(bash "$harness" 2>&1)
    rc=$?
    set -e
    if [ "$rc" -ne "$expected_rc" ]; then
        echo -e "${RED}FAIL: $label: expected exit $expected_rc, got $rc\n$out${NC}" >&2
        exit 1
    fi
    if [ "$expected_rc" -eq 0 ] && ! printf '%s\n' "$out" | grep -q '^GATE_SURVIVED$'; then
        echo -e "${RED}FAIL: $label: exit 0 but gate did not fall through.\n$out${NC}" >&2
        exit 1
    fi
    if [ "$expected_rc" -ne 0 ] && ! printf '%s\n' "$out" | grep -q 'CEREMONY-FAIL'; then
        echo -e "${RED}FAIL: $label: missing [CEREMONY-FAIL] banner.\n$out${NC}" >&2
        exit 1
    fi
    echo -e "  ${GREEN}✓${NC} $label (rc=$rc)"
}

run_gate "(a) CERTIFIED + ceremony" 0 CERTIFIED true
run_gate "(b) INCOMPLETE + ceremony" 3 INCOMPLETE true
run_gate "(c) FUNCTIONALLY_CERTIFIED + ceremony" 3 FUNCTIONALLY_CERTIFIED true
run_gate "(d) INCOMPLETE without ceremony" 0 INCOMPLETE false
run_gate "(e) empty status + ceremony" 3 "" true
run_gate "(f) FUNCTIONALLY_CERTIFIED without ceremony" 0 FUNCTIONALLY_CERTIFIED false

DOC_FILE="$PROJECT_DIR/docs/testing.md"
for doc_term in 'Exit `3`' "reused_certified_binary" "never rebuilt after certification" "provenance.ceremony"; do
    if ! grep -qF "$doc_term" "$DOC_FILE"; then
        echo -e "${RED}FAIL: docs/testing.md does not document '$doc_term' (T4.3 drift guard).${NC}" >&2
        exit 1
    fi
done
echo -e "  ${GREEN}✓${NC} docs/testing.md documents exit 3, reused_certified_binary and the rebuild invariant"

echo -e "${GREEN}${BOLD}PASS: test_ceremony_fail_closed${NC}"

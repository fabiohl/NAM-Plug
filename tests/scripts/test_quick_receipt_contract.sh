#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Contract test between the quick-suite producer (utils/tests-quick.sh), the
# frozen golden receipt (tests/fixtures/quick-receipt-golden.txt) and the
# release-gate parser (quick_receipt_check).
#
# The golden fixture is what the producer actually emitted; passing it through
# the real parser with the release-gate flags proves the parser accepts the
# producer's format rather than a synthetic PASS-only line. The producer grep is
# additionally mutation-tested so that changing the RT_CODEGEN prefix turns this
# test red instead of silently drifting from the grammar.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

GOLDEN="$PROJECT_DIR/tests/fixtures/quick-receipt-golden.txt"
PRODUCER="$PROJECT_DIR/utils/tests-quick.sh"

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

cd "$PROJECT_DIR"

run_checker() {
    local receipt="$1"
    cargo run --quiet --locked --bin quick_receipt_check -- \
        "$receipt" --require-passed --require-heap-audit
}

producer_has_contract() {
    local producer="$1"
    grep -qF 'RT_CODEGEN_METRICS:' "$producer" \
        && grep -qF 'RT_CODEGEN: PASS' "$producer"
}

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD} test_quick_receipt_contract: producer <-> parser agreement     ${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

if [ ! -f "$GOLDEN" ]; then
    echo -e "${RED}FAIL: golden receipt missing: $GOLDEN${NC}" >&2
    exit 1
fi
if [ ! -f "$PRODUCER" ]; then
    echo -e "${RED}FAIL: quick-suite producer missing: $PRODUCER${NC}" >&2
    exit 1
fi

echo -e "\n${BLUE}[1/4] Golden receipt passes the release gate (--require-passed --require-heap-audit)...${NC}"
if ! OUT=$(run_checker "$GOLDEN" 2>&1); then
    echo -e "${RED}FAIL: parser rejected the golden producer receipt:\n$OUT${NC}" >&2
    exit 1
fi
if ! printf '%s\n' "$OUT" | grep -q '^quick_receipt_check: OK '; then
    echo -e "${RED}FAIL: unexpected checker output:\n$OUT${NC}" >&2
    exit 1
fi
echo -e "  ${GREEN}✓${NC} golden receipt accepted with pass + heap-audit requirements."

echo -e "\n${BLUE}[2/4] Producer still emits the RT_CODEGEN PASS + metrics contract...${NC}"
if ! producer_has_contract "$PRODUCER"; then
    echo -e "${RED}FAIL: $PRODUCER no longer emits 'RT_CODEGEN: PASS' and 'RT_CODEGEN_METRICS:'.${NC}" >&2
    exit 1
fi
echo -e "  ${GREEN}✓${NC} producer emits RT_CODEGEN PASS derived from RT_CODEGEN_METRICS."

echo -e "\n${BLUE}[3/4] Parser rejects a receipt whose RT_CODEGEN verdict is not PASS...${NC}"
sed 's/^RT_CODEGEN: PASS/RT_CODEGEN: OK/' "$GOLDEN" > "$TMP_DIR/bad-receipt.txt"
if cmp -s "$GOLDEN" "$TMP_DIR/bad-receipt.txt"; then
    echo -e "${RED}FAIL: could not mutate the golden RT_CODEGEN line.${NC}" >&2
    exit 1
fi
set +e
OUT=$(run_checker "$TMP_DIR/bad-receipt.txt" 2>&1)
RC=$?
set -e
if [ "$RC" -eq 0 ]; then
    echo -e "${RED}FAIL: parser accepted RT_CODEGEN: OK (exit 0):\n$OUT${NC}" >&2
    exit 1
fi
echo -e "  ${GREEN}✓${NC} mutated receipt rejected (rc=$RC)."

echo -e "\n${BLUE}[4/4] Producer mutation is detected by the contract grep...${NC}"
sed 's/RT_CODEGEN: PASS/RT_CODEGEN: OK/' "$PRODUCER" > "$TMP_DIR/tests-quick-mutated.sh"
if producer_has_contract "$TMP_DIR/tests-quick-mutated.sh"; then
    echo -e "${RED}FAIL: producer contract grep accepted a changed RT_CODEGEN prefix.${NC}" >&2
    exit 1
fi
echo -e "  ${GREEN}✓${NC} changed prefix is rejected by the producer contract."

echo -e "\n${GREEN}${BOLD}✓ Quick receipt producer/parser contract is intact.${NC}"

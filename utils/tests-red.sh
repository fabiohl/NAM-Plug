#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# TDD RED Suite for NAM-Plug — expected-to-fail containment tests.
#
# Division of responsibility among QA scripts:
#   * utils/lints.sh       — Static quality gate (fmt, SPDX, cargo check, clippy).
#   * utils/tests-quick.sh — Agile green test suite (cargo test).
#   * utils/tests-red.sh   — THIS script. TDD RED suite for known bug containment (clap_e0_containment_test).
#
# Runs the clap_e0_containment_test suite (red-by-design) which documents and isolates
# active behavioral regressions. This suite verifies the RED signal without breaking
# normal developer flow. Once all targeted bugs in the suite pass, developers are prompted
# to promote the test suite to tests-quick.sh.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"

PHASE_TOTAL=1
source "$SCRIPT_DIR/_lib.sh"

# Re-execute with low CPU and I/O priority (nice and ionice) to prevent overloading the system.
if [ "${NAM_LOW_PRIORITY:-0}" != "1" ] && [ "${NAM_NO_LOW_PRIORITY:-0}" != "1" ]; then
    export NAM_LOW_PRIORITY=1
    CMD_PREFIX=""
    if command -v nice >/dev/null 2>&1; then
        CMD_PREFIX="nice -n 19"
    fi
    if command -v ionice >/dev/null 2>&1; then
        CMD_PREFIX="$CMD_PREFIX ionice -c 3"
    fi
    if [ -n "$CMD_PREFIX" ]; then
        echo -e "${YELLOW}ⓘ Restarting script with low priority (CPU/IO) to prevent system overload...${NC}"
        exec $CMD_PREFIX "$SCRIPT_PATH" "$@"
    fi
fi

trap 'echo -e "\n${RED}${BOLD}❌ Unexpected error: Command \"$BASH_COMMAND\" failed at line $LINENO with status $?. Aborting test suite.${NC}"; exit 1' ERR

echo -e "${BLUE}${BOLD}========================================${NC}"
echo -e "${BLUE}${BOLD}        NAM-Plug TDD RED Suite          ${NC}"
echo -e "${BLUE}${BOLD}========================================${NC}"

# ── Phase 1: Containment tests (red-by-design) ───────────────────────────────
phase "Containment tests (red-by-design)..."
set +e
trap - ERR
cargo test --features testing --test clap_e0_containment_test -- --skip ignored
RED_STATUS=$?
set -e
trap 'echo -e "\n${RED}${BOLD}❌ Unexpected error: Command \"$BASH_COMMAND\" failed at line $LINENO with status $?. Aborting test suite.${NC}"; exit 1' ERR

echo
if [ $RED_STATUS -ne 0 ]; then
    echo -e "${GREEN}${BOLD}================================================================${NC}"
    echo -e "${GREEN}${BOLD}   Expected RED status confirmed — containment bugs active.     ${NC}"
    echo -e "${GREEN}${BOLD}================================================================${NC}"
else
    echo -e "${YELLOW}${BOLD}================================================================${NC}"
    echo -e "${YELLOW}${BOLD}   All containment tests passed!                                ${NC}"
    echo -e "${YELLOW}${BOLD}   Consider promoting this test suite to tests-quick.sh.        ${NC}"
    echo -e "${YELLOW}${BOLD}================================================================${NC}"
fi


#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Quick QA Suite for NAM-Plug — agile first line of defense.
#
# Division of responsibility among QA scripts:
#   * utils/lints.sh       — Static quality gate (fmt, SPDX, cargo check, clippy).
#   * utils/tests-quick.sh — THIS script. Agile green test suite (cargo test).
#   * utils/tests-red.sh   — TDD RED suite for known bug containment (clap_e0_containment_test).
#
# NAM-Plug is a CLAP plugin crate with unit tests in src/ and integration tests in tests/.
#
# Phases:
#   1. Structural (debug)   — unit + integration tests with debug assertions ON.
#   2. Release verification — tests production release codegen path.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"

PHASE_TOTAL=2
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
echo -e "${BLUE}${BOLD}        NAM-Plug Quick QA Suite         ${NC}"
echo -e "${BLUE}${BOLD}========================================${NC}"

# ── Phase 1: Structural unit & integration tests (debug) ─────────────────────
phase "Structural: unit & integration tests (debug)..."
cargo test --features testing --lib \
    --test clap \
    --test clap_e2_proptest \
    --test processor_bypass_test \
    -- --skip ignored

# ── Phase 2: Release verification (release) ─────────────────────────────────
phase "Release verification: unit & integration tests (release)..."
cargo test --features testing --lib \
    --test clap \
    --test clap_e2_proptest \
    --test processor_bypass_test \
    --release -- --nocapture

# ── Summary ──────────────────────────────────────────────────────────────────
echo -e "${GREEN}${BOLD}========================================${NC}"
echo -e "${GREEN}${BOLD}    All quick tests passed! (CLAP)      ${NC}"
echo -e "${GREEN}${BOLD}========================================${NC}"

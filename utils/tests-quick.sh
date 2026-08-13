#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Quick QA Suite for NAM-Plug — agile first line of defense.
#
# Division of responsibility among QA scripts:
#   * utils/lints.sh       — Static quality gate (fmt, SPDX, cargo check, clippy).
#   * utils/tests-quick.sh — THIS script. Agile green test suite (cargo test).
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
    if command -v nice > /dev/null 2>&1; then
        CMD_PREFIX="nice -n 19"
    fi
    if command -v ionice > /dev/null 2>&1; then
        CMD_PREFIX="$CMD_PREFIX ionice -c 3"
    fi
    if [ -n "$CMD_PREFIX" ]; then
        warn "Restarting script with low priority (CPU/IO) to prevent system overload..."
        exec $CMD_PREFIX "$SCRIPT_PATH" "$@"
    fi
fi

trap 'echo -e "\n${RED}${BOLD}❌ Unexpected error: Command \"$BASH_COMMAND\" failed at line $LINENO with status $?. Aborting test suite.${NC}"; exit 1' ERR

echo -e "${BLUE}${BOLD}========================================${NC}"
echo -e "${BLUE}${BOLD}        NAM-Plug Quick QA Suite         ${NC}"
echo -e "${BLUE}${BOLD}========================================${NC}"

# Helper: Ensure the CLAP plugin shared library artifact for the requested
# profile is present before running integration tests that dlopen it.
#
# Accepts profile "debug" or "release". Checks for the exact profile artifact
# first; does NOT silently fall back to the other profile, since that could
# mask build-time failures specific to debug assertions or release codegen.
ensure_clap_artifact() {
    local profile="${1:-debug}"
    local flag=""
    if [ "$profile" = "release" ]; then
        flag="--release"
    fi

    # If the caller explicitly pointed to a pre-built artifact, honour it only
    # when the file actually exists — never silently accept a stale path.
    if [ -n "${CLAP_PLUGIN_PATH:-}" ]; then
        if [ -f "$CLAP_PLUGIN_PATH" ]; then
            return 0
        else
            warn "CLAP_PLUGIN_PATH set but file not found: $CLAP_PLUGIN_PATH. Rebuilding..."
        fi
    fi

    local target_dir="${CARGO_TARGET_DIR:-target}"
    local artifact_path="$target_dir/$profile/libnam_plug.so"

    if [ ! -f "$artifact_path" ]; then
        warn "CLAP plugin ($profile) artifact not found. Pre-building..."
        # shellcheck disable=SC2086
        cargo build $flag
    fi
}

# ── Phase 1: Structural unit & integration tests (debug) ─────────────────────
phase "Structural: unit & integration tests (debug)..."
ensure_clap_artifact debug
timeout 300 cargo test --features testing --lib \
    --test clap \
    --test clap_e0_containment_test \
    --test clap_e2_proptest \
    --test processor_bypass_test \
    -- --skip ignored

# ── Phase 2: Release verification (release) ─────────────────────────────────
phase "Release verification: unit & integration tests (release)..."
ensure_clap_artifact release
timeout 300 cargo test --features testing --lib \
    --test clap \
    --test clap_e0_containment_test \
    --test clap_e2_proptest \
    --test processor_bypass_test \
    --release -- --skip ignored --test-threads=1 --nocapture

# ── Summary ──────────────────────────────────────────────────────────────────
echo -e "${GREEN}${BOLD}========================================${NC}"
echo -e "${GREEN}${BOLD}    All quick tests passed! (CLAP)      ${NC}"
echo -e "${GREEN}${BOLD}========================================${NC}"

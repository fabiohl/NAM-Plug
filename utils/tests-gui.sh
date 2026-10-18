#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Headless GUI test runner for NAM-Plug, backed by Xvfb.
#
# Allocates an isolated virtual X11 display, sets up software-rendering
# environment variables, executes the four GUI/X11 integration tests that are
# normally `#[ignore]`d (they require a dedicated display server), and tears
# down the virtual display unconditionally via a trap — preserving the host's
# DISPLAY and X11 state regardless of test outcome.
#
# Tests exercised:
#   - gui_embedded_x11_open_close_cycles_no_leak  (10 open/close XEmbed cycles)
#   - gui_lifecycle_hide_after_show_with_real_window  (F1 defect regression)
#   - test_headless_clipboard_works  (arboard X11 round-trip)
#   - test_headless_gui_floating_window_lifecycle  (create → transient → destroy)
#   - gui_lifecycle_x11_embedded_set_parent_honest  (X11 embedded set_parent cycle)
#
# Usage:
#   ./utils/tests-gui.sh [--nocapture] [--dry-run] [--strict]
#
# Options:
#   --nocapture   Forward --nocapture to cargo test (verbose output).
#   --dry-run     Print planned commands without executing them.
#   --strict      Exit 1 if xvfb-run is not available (default: report gap).
#
# Log output: target/logs/gui-tests.log
# Receipt:    target/logs/gui-receipt.txt

set -euo pipefail

NOCAPTURE=0
DRY_RUN=0
STRICT=0
TEST_FAIL_CLOSED=0

for arg in "$@"; do
    case "$arg" in
        --nocapture)        NOCAPTURE=1 ;;
        --dry-run)          DRY_RUN=1   ;;
        --strict)           STRICT=1    ;;
        --test-fail-closed) TEST_FAIL_CLOSED=1 ;;
        --help|-h)
            echo "Usage: $(basename "$0") [--nocapture] [--dry-run] [--strict] [--test-fail-closed]"
            echo ""
            echo "Headless GUI test runner — executes ignored X11/GUI tests under Xvfb."
            echo ""
            echo "Options:"
            echo "  --nocapture        Pass --nocapture to cargo test"
            echo "  --dry-run          Print planned commands without executing"
            echo "  --strict           Fail if xvfb-run is not installed"
            echo "  --test-fail-closed Self-test fail-closed receipt logic and exit"
            echo "  -h, --help         Show this help and exit"
            echo ""
            echo "Log:     target/logs/gui-tests.log (or target/logs/gui-tests.dryrun.log in --dry-run)"
            echo "Receipt: target/logs/gui-receipt.txt (or target/logs/gui-receipt.dryrun.txt in --dry-run)"
            exit 0
            ;;
        *)
            echo "Unknown option: $arg" >&2
            echo "Usage: $(basename "$0") [--nocapture] [--dry-run] [--strict] [--test-fail-closed]" >&2
            exit 1
            ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"
LIB_DIR="$SCRIPT_DIR/lib"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Source shared helpers if available (colors, phase, die, ok, warn).
if [ -f "$LIB_DIR/_lib.sh" ]; then
    PHASE_TOTAL=1
    export PHASE_TOTAL
    # shellcheck source=utils/lib/_lib.sh
    source "$LIB_DIR/_lib.sh"
else
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
    BLUE='\033[0;34m'; CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'
    die()  { echo -e "${RED}${BOLD}[FATAL]${NC} $*" >&2; exit 1; }
    ok()   { echo -e "  ${GREEN}OK${NC} $*"; }
    warn() { echo -e "  ${YELLOW}ⓘ${NC} $*"; }
    cd "$PROJECT_DIR" || exit 1
fi

if [ "${NAM_LIB_NO_CD:-0}" != "1" ]; then
    cd "$PROJECT_DIR" || exit 1
fi

# ── Memory guard (OOM containment) ──────────────────────────────────────────
# This suite builds the test harness in release mode. On a 16 GiB host with no
# swap, that build plus rust-analyzer/IDE can exhaust RAM and trigger a global
# OOM that kills the editor. Re-exec inside a transient, memory-capped systemd
# scope (fail-open when unavailable) so the build is the only thing at risk.
if [ "${NAM_MEM_GUARD:-0}" != "1" ] && [ "${NAM_NO_MEM_GUARD:-0}" != "1" ] && [ "$DRY_RUN" = "0" ]; then
    export NAM_MEM_GUARD=1
    exec "$SCRIPT_DIR/mem-guard.sh" "$SCRIPT_PATH" "$@"
fi

# Release test artifacts never need the shipped fat-LTO profile: disabling LTO
# and restoring parallel codegen keeps the build's peak memory bounded without
# affecting the GUI lifecycle semantics under test. Exported (not per-command)
# so the pre-compile and every test invocation reuse the exact same artifacts.
export CARGO_PROFILE_RELEASE_LTO="${CARGO_PROFILE_RELEASE_LTO:-false}"
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${CARGO_PROFILE_RELEASE_CODEGEN_UNITS:-16}"

# ── State ───────────────────────────────────────────────────────────────────

# Save original display so the trap restores it in any signal path.
ORIG_DISPLAY="${DISPLAY:-}"
XVFB_PID=0

cleanup_xvfb() {
    local rc=$?
    # Unset the virtual display; restore the original one if it was set.
    unset DISPLAY
    if [ -n "$ORIG_DISPLAY" ]; then
        export DISPLAY="$ORIG_DISPLAY"
    fi
    # Kill any Xvfb server we spawned directly (xvfb-run manages its own).
    if [ "${XVFB_PID:-0}" -ne 0 ] && kill -0 "$XVFB_PID" 2>/dev/null; then
        kill "$XVFB_PID" 2>/dev/null || true
        wait "$XVFB_PID" 2>/dev/null || true
    fi
    # Hardening (E-02 / T-R0.2): fail closed if receipt is empty or SIMULATED outside dry-run
    if [ "${DRY_RUN:-0}" = "0" ]; then
        if [ ! -s "${GUI_RECEIPT:-}" ]; then
            echo -e "  ${RED}${BOLD}❌ Fail-closed: receipt '${GUI_RECEIPT:-}' is missing or empty.${NC}" >&2
            rc=1
        elif grep -q "status=SIMULATED" "${GUI_RECEIPT:-}"; then
            echo -e "  ${RED}${BOLD}❌ Fail-closed: receipt '${GUI_RECEIPT:-}' contains status=SIMULATED outside --dry-run.${NC}" >&2
            rc=1
        fi
    fi
    return $rc
}

if [ "$TEST_FAIL_CLOSED" = "1" ]; then
    echo "Running self-test for tests-gui.sh fail-closed logic..."
    TMP_RECEIPT=$(mktemp)
    : > "$TMP_RECEIPT"
    if ( GUI_RECEIPT="$TMP_RECEIPT" DRY_RUN=0 OVERALL_RC=0 cleanup_xvfb 2>/dev/null ); then
        rm -f "$TMP_RECEIPT"
        die "Self-test failed: cleanup_xvfb did not fail on empty receipt"
    fi
    echo "gui_test_suite status=SIMULATED" > "$TMP_RECEIPT"
    if ( GUI_RECEIPT="$TMP_RECEIPT" DRY_RUN=0 OVERALL_RC=0 cleanup_xvfb 2>/dev/null ); then
        rm -f "$TMP_RECEIPT"
        die "Self-test failed: cleanup_xvfb did not fail on SIMULATED receipt in non-dry-run mode"
    fi
    rm -f "$TMP_RECEIPT"
    ok "Self-test passed: tests-gui.sh fails closed on empty or SIMULATED receipt"
    exit 0
fi

if [ "$DRY_RUN" = "1" ]; then
    GUI_LOG="target/logs/gui-tests.dryrun.log"
    GUI_RECEIPT="target/logs/gui-receipt.dryrun.txt"
else
    GUI_LOG="target/logs/gui-tests.log"
    GUI_RECEIPT="target/logs/gui-receipt.txt"
fi

mkdir -p target/logs
: > "$GUI_LOG"
: > "$GUI_RECEIPT"

trap 'rc=$?; cleanup_xvfb; if [ $rc -ne 0 ]; then echo -e "\n${RED}${BOLD}❌ GUI tests interrupted at line $LINENO (rc=$rc)${NC}"; fi; exit $rc' INT TERM EXIT ERR

# ── Dependency check ────────────────────────────────────────────────────────
HAVE_XVFB_RUN=0
if [ "${NAM_NO_XVFB:-0}" != "1" ] && command -v xvfb-run >/dev/null 2>&1; then
    HAVE_XVFB_RUN=1
fi

if [ "$HAVE_XVFB_RUN" = "0" ]; then
    if [ "$STRICT" = "1" ]; then
        die "xvfb-run not found. Install the 'xvfb' package and re-run."
    else
        TIMESTAMP="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
        echo -e "${YELLOW}${BOLD}[GAP] xvfb-run not installed — GUI tests cannot run.${NC}"
        echo -e "      Install with: sudo apt-get install -y xvfb"
        echo -e "      Then re-run:  ./utils/tests-gui.sh"
        {
            echo "gui_test_suite status=GAP timestamp=$TIMESTAMP"
            echo "  log=$GUI_LOG"
            echo "  reason=xvfb-run not installed"
        } | tee "$GUI_RECEIPT"
        exit 0
    fi
fi

# ── Header ──────────────────────────────────────────────────────────────────
echo -e "${BLUE}${BOLD}=============================================================${NC}"
echo -e "${BLUE}${BOLD}     NAM-Plug Headless GUI / X11 Test Suite (Xvfb)          ${NC}"
echo -e "${BLUE}${BOLD}=============================================================${NC}"
echo -e "  nocapture: ${BOLD}$NOCAPTURE${NC}  dry-run: ${BOLD}$DRY_RUN${NC}  strict: ${BOLD}$STRICT${NC}"
echo -e "  Log:     ${CYAN}$GUI_LOG${NC}"
echo -e "  Receipt: ${CYAN}$GUI_RECEIPT${NC}"

if [ "$DRY_RUN" = "1" ]; then
    echo -e "  ${YELLOW}Dry-run mode — no tests will be executed.${NC}"
fi

# ── Build compile artifacts first (--no-run) ─────────────────────────────────
# A dedicated pre-compile step avoids Xvfb timeout pressure during linking.
# Only the lib test binary is needed: every GUI/X11 test below lives in `src/`
# (lib test harness), so building all integration-test targets would multiply
# linker memory for nothing.
echo -e "\n${BLUE}→${NC} Pre-compiling test artifacts (--no-run)..."
if [ "$DRY_RUN" = "0" ]; then
    cargo test --features testing --release --no-run --lib \
        2>&1 | tee -a "$GUI_LOG"
    echo -e "  ${GREEN}✓${NC} Compile done."
else
    echo -e "  ${YELLOW}[dry-run]${NC} cargo test --features testing --release --no-run --lib"
fi

# ── Test list ───────────────────────────────────────────────────────────────
#
# Each entry is: "<cargo_args_for_filter>"
# Note: gui_embedded must run in isolation (Slint one-platform-per-process rule,
# documented in docs/architecture.md §7.5). All four tests share one Xvfb
# session so they don't fight for the same display number, but they are
# serialised by running them one at a time with --test-threads=1.

NOCAPTURE_FLAG=""
[ "$NOCAPTURE" = "1" ] && NOCAPTURE_FLAG="--nocapture"

OVERALL_RC=0

run_gui_test() {
    local label="$1"
    local filter="$2"
    local cargo_target_flag="$3"  # e.g. "--lib" or empty for default search

    echo -e "\n  ${BLUE}→${NC} $label ..."
    if [ "$DRY_RUN" = "1" ]; then
        echo -e "  ${YELLOW}[dry-run]${NC} xvfb-run -a --server-args='-screen 0 1280x800x24' \\"
        echo -e "      cargo test --features testing --release $cargo_target_flag $filter -- --ignored --test-threads=1 $NOCAPTURE_FLAG"
        return 0
    fi

    local test_rc=0
    # xvfb-run -a: allocates the first free display port automatically.
    # --server-args: 24-bit colour depth matches Slint/Mesa expectations.
    LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
    NAM_GUI_DEDICATED_X11=1 NAM_GUI_TIMEOUT_MS="${NAM_GUI_TIMEOUT_MS:-15000}" \
        xvfb-run -a --server-args="-screen 0 1280x800x24" \
        cargo test --features testing --release $cargo_target_flag "$filter" \
            -- --ignored --test-threads=1 $NOCAPTURE_FLAG \
        2>&1 | tee -a "$GUI_LOG" || test_rc=$?

    if [ "$test_rc" -ne 0 ]; then
        echo -e "  ${RED}✗${NC} $label FAILED (rc=$test_rc)"
        OVERALL_RC=1
    else
        echo -e "  ${GREEN}✓${NC} $label passed"
    fi
}

# gui_embedded must be the only test matching its prefix to respect the
# one-platform-per-process invariant (see docs/architecture.md §7.5).
run_gui_test \
    "gui_embedded_x11_open_close_cycles_no_leak" \
    "gui_embedded_x11_open_close_cycles_no_leak" \
    "--lib"

run_gui_test \
    "gui_lifecycle_hide_after_show_with_real_window" \
    "gui_lifecycle_hide_after_show_with_real_window" \
    "--lib"

run_gui_test \
    "test_headless_clipboard_works" \
    "test_headless_clipboard_works" \
    "--lib"

run_gui_test \
    "test_headless_gui_floating_window_lifecycle" \
    "test_headless_gui_floating_window_lifecycle" \
    "--lib"

run_gui_test \
    "gui_lifecycle_x11_embedded_set_parent_honest" \
    "gui_lifecycle_x11_embedded_set_parent_honest" \
    "--lib"

# ── Receipt ─────────────────────────────────────────────────────────────────
TIMESTAMP="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
GUI_STATUS="PASS"
[ "$OVERALL_RC" -ne 0 ] && GUI_STATUS="FAILED"
[ "$DRY_RUN" = "1" ]    && GUI_STATUS="SIMULATED"

{
    echo "gui_test_suite status=$GUI_STATUS timestamp=$TIMESTAMP"
    echo "  log=$GUI_LOG"
    echo "  tests=gui_embedded_x11_open_close_cycles_no_leak,gui_lifecycle_hide_after_show_with_real_window,test_headless_clipboard_works,test_headless_gui_floating_window_lifecycle,gui_lifecycle_x11_embedded_set_parent_honest"
} | tee "$GUI_RECEIPT"

# Post-write fail-closed verification
if [ "$DRY_RUN" = "0" ]; then
    if [ ! -s "$GUI_RECEIPT" ]; then
        echo -e "  ${RED}${BOLD}❌ Fail-closed: receipt $GUI_RECEIPT is empty.${NC}" >&2
        OVERALL_RC=1
    elif grep -q "status=SIMULATED" "$GUI_RECEIPT"; then
        echo -e "  ${RED}${BOLD}❌ Fail-closed: receipt $GUI_RECEIPT contains status=SIMULATED outside --dry-run.${NC}" >&2
        OVERALL_RC=1
    elif ! grep -q "status=PASS" "$GUI_RECEIPT"; then
        echo -e "  ${RED}${BOLD}❌ Fail-closed: receipt $GUI_RECEIPT does not contain status=PASS.${NC}" >&2
        OVERALL_RC=1
    fi
fi

echo -e "\n${BLUE}${BOLD}================== GUI TEST SUMMARY ==================${NC}"
if [ "$GUI_STATUS" = "PASS" ] || [ "$GUI_STATUS" = "PASSED" ]; then
    echo -e "  ${GREEN}${BOLD}✓ All GUI tests PASSED${NC}"
elif [ "$GUI_STATUS" = "SIMULATED" ]; then
    echo -e "  ${YELLOW}Dry-run completed — simulated receipt in $GUI_RECEIPT (no tests executed).${NC}"
else
    echo -e "  ${RED}${BOLD}❌ GUI tests FAILED — see $GUI_LOG${NC}"
fi
echo -e "  Log:     ${CYAN}$GUI_LOG${NC}"
echo -e "  Receipt: ${CYAN}$GUI_RECEIPT${NC}"
echo -e "${BLUE}${BOLD}======================================================${NC}"

exit "$OVERALL_RC"

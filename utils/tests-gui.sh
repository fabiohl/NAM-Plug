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

for arg in "$@"; do
    case "$arg" in
        --nocapture) NOCAPTURE=1 ;;
        --dry-run)   DRY_RUN=1   ;;
        --strict)    STRICT=1    ;;
        --help|-h)
            echo "Usage: $(basename "$0") [--nocapture] [--dry-run] [--strict]"
            echo ""
            echo "Headless GUI test runner — executes ignored X11/GUI tests under Xvfb."
            echo ""
            echo "Options:"
            echo "  --nocapture  Pass --nocapture to cargo test"
            echo "  --dry-run    Print planned commands without executing"
            echo "  --strict     Fail if xvfb-run is not installed"
            echo "  -h, --help   Show this help and exit"
            echo ""
            echo "Log:     target/logs/gui-tests.log"
            echo "Receipt: target/logs/gui-receipt.txt"
            exit 0
            ;;
        *)
            echo "Unknown option: $arg" >&2
            echo "Usage: $(basename "$0") [--nocapture] [--dry-run] [--strict]" >&2
            exit 1
            ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
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

# ── Dependency check ────────────────────────────────────────────────────────
HAVE_XVFB_RUN=0
if command -v xvfb-run >/dev/null 2>&1; then
    HAVE_XVFB_RUN=1
fi

if [ "$HAVE_XVFB_RUN" = "0" ]; then
    if [ "$STRICT" = "1" ]; then
        die "xvfb-run not found. Install the 'xvfb' package and re-run."
    else
        echo -e "${YELLOW}${BOLD}[GAP] xvfb-run not installed — GUI tests cannot run.${NC}"
        echo -e "      Install with: sudo apt-get install -y xvfb"
        echo -e "      Then re-run:  ./utils/tests-gui.sh"
        exit 0
    fi
fi

# ── State ───────────────────────────────────────────────────────────────────

# Save original display so the trap restores it in any signal path.
ORIG_DISPLAY="${DISPLAY:-}"
XVFB_PID=0
GUI_LOG="target/logs/gui-tests.log"
GUI_RECEIPT="target/logs/gui-receipt.txt"

mkdir -p target/logs
: > "$GUI_LOG"

cleanup_xvfb() {
    local rc=$?
    # Unset the virtual display; restore the original one if it was set.
    unset DISPLAY
    if [ -n "$ORIG_DISPLAY" ]; then
        export DISPLAY="$ORIG_DISPLAY"
    fi
    # Kill any Xvfb server we spawned directly (xvfb-run manages its own).
    if [ "$XVFB_PID" -ne 0 ] && kill -0 "$XVFB_PID" 2>/dev/null; then
        kill "$XVFB_PID" 2>/dev/null || true
        wait "$XVFB_PID" 2>/dev/null || true
    fi
    return $rc
}

trap 'rc=$?; cleanup_xvfb; if [ $rc -ne 0 ]; then echo -e "\n${RED}${BOLD}❌ GUI tests interrupted at line $LINENO (rc=$rc)${NC}"; fi; exit $rc' INT TERM EXIT ERR

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
echo -e "\n${BLUE}→${NC} Pre-compiling test artifacts (--no-run)..."
if [ "$DRY_RUN" = "0" ]; then
    cargo test --features testing --release --no-run --lib --tests \
        2>&1 | tee -a "$GUI_LOG"
    echo -e "  ${GREEN}✓${NC} Compile done."
else
    echo -e "  ${YELLOW}[dry-run]${NC} cargo test --features testing --release --no-run --lib --tests"
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
    ""

run_gui_test \
    "gui_lifecycle_hide_after_show_with_real_window" \
    "gui_lifecycle_hide_after_show_with_real_window" \
    ""

run_gui_test \
    "test_headless_clipboard_works" \
    "test_headless_clipboard_works" \
    ""

run_gui_test \
    "test_headless_gui_floating_window_lifecycle" \
    "test_headless_gui_floating_window_lifecycle" \
    ""

# ── Receipt ─────────────────────────────────────────────────────────────────
TIMESTAMP="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
GUI_STATUS="PASSED"
[ "$OVERALL_RC" -ne 0 ] && GUI_STATUS="FAILED"
[ "$DRY_RUN" = "1" ]    && GUI_STATUS="DRY_RUN"

{
    echo "gui_test_suite status=$GUI_STATUS timestamp=$TIMESTAMP"
    echo "  log=$GUI_LOG"
    echo "  tests=gui_embedded_x11_open_close_cycles_no_leak,gui_lifecycle_hide_after_show_with_real_window,test_headless_clipboard_works,test_headless_gui_floating_window_lifecycle"
} | tee "$GUI_RECEIPT"

echo -e "\n${BLUE}${BOLD}================== GUI TEST SUMMARY ==================${NC}"
if [ "$GUI_STATUS" = "PASSED" ]; then
    echo -e "  ${GREEN}${BOLD}✓ All GUI tests PASSED${NC}"
elif [ "$GUI_STATUS" = "DRY_RUN" ]; then
    echo -e "  ${YELLOW}Dry-run completed — no tests executed.${NC}"
else
    echo -e "  ${RED}${BOLD}❌ GUI tests FAILED — see $GUI_LOG${NC}"
fi
echo -e "  Log:     ${CYAN}$GUI_LOG${NC}"
echo -e "  Receipt: ${CYAN}$GUI_RECEIPT${NC}"
echo -e "${BLUE}${BOLD}======================================================${NC}"

exit "$OVERALL_RC"

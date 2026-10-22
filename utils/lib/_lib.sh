# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

# _lib.sh — Common bash utilities for NAM-Plug scripts.
#
# Source with:
#   PHASE_TOTAL=<N>; source "$(dirname "$0")/lib/_lib.sh"
# or for scripts that manage their own working directory (e.g. build-release.sh):
#   NAM_LIB_NO_CD=1 PHASE_TOTAL=<N>; source "$(dirname "$0")/lib/_lib.sh"
#
# The project-neutral helpers (ANSI colors, phase/ok/warn/die,
# format_duration_ms/phase_elapsed_str, emit/emit_to, assert_ran_tests/
# assert_subphase_ran, pick_bench_core) live in the canonical, byte-identical
# `lib/_lib_core.sh` block (see utils/test-lib-drift.sh). This file adds only
# the repository-specific glue on top.
#
# Then call:
#   phase "Description of the current step"
#   die  "Fatal error message"    # prints to stderr and exits 1
#   ok   "Success message"        # prints indented green OK line
#   warn "Warning message"        # prints indented yellow notice

# Canonical project-neutral core (drift-pinned; see utils/test-lib-drift.sh).
source "$(dirname "${BASH_SOURCE[0]}")/_lib_core.sh"

# ---------------------------------------------------------------------------
# Resolve project root dynamically relative to this helper script.
# ---------------------------------------------------------------------------
LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UTILS_DIR="$(dirname "$LIB_DIR")"
PROJECT_DIR="$(dirname "$UTILS_DIR")"

if [ -z "$PROJECT_DIR" ]; then
    echo -e "${RED}${BOLD}[FATAL]${NC} _lib.sh: could not resolve PROJECT_DIR." >&2
    exit 1
fi

# Automatically enter the project root directory, unless the caller has set
# NAM_LIB_NO_CD=1 (used by scripts that manage their own working directory,
# such as build-release.sh which sets up its own cd before sourcing this lib).
if [ "${NAM_LIB_NO_CD:-0}" != "1" ]; then
    cd "$PROJECT_DIR" || {
        echo -e "${RED}${BOLD}[FATAL]${NC} _lib.sh: failed to cd into project root: $PROJECT_DIR" >&2
        exit 1
    }
fi

# ---------------------------------------------------------------------------
# Release-receipt status derivation (shared by build-release and finalizer)
# ---------------------------------------------------------------------------

# receipt_derive_status <git_dirty> <core_git_dirty> <perf_status>
#                       <quick_passed> <heap_passed> <pgo_applied>
#                       <bolt_applied> <non_artifact_skip_count>
#   Applies the single certification rule used by both the build pipeline and
#   the post-long-audit finalizer. `non_artifact_skip_count` deliberately
#   excludes `artifact_consistency:*` gaps: those are capped separately by
#   `receipt_apply_artifact_cap`, so a missing long audit can degrade a receipt
#   to FUNCTIONALLY_CERTIFIED (non-strict) without pretending other gaps do not
#   exist. Prints CERTIFIED, FUNCTIONALLY_CERTIFIED or INCOMPLETE.
receipt_derive_status() {
    local git_dirty="$1" core_dirty="$2" perf_status="$3"
    local quick_passed="$4" heap_passed="$5" pgo_applied="$6"
    local bolt_applied="$7" skip_count="$8"
    local status="CERTIFIED"

    if [ "$git_dirty" = "true" ] || [ "$core_dirty" = "true" ]; then
        status="INCOMPLETE"
    fi

    if [ "${skip_count:-0}" -gt 0 ] \
        || [ "$perf_status" = "FAILED" ] \
        || [ "$quick_passed" != "true" ] \
        || [ "$heap_passed" != "true" ]; then
        status="INCOMPLETE"
    elif [ "$perf_status" = "INCONCLUSIVE" ] \
        || [ "$pgo_applied" != "true" ] \
        || [ "$bolt_applied" != "true" ]; then
        if [ "$status" = "CERTIFIED" ]; then
            status="FUNCTIONALLY_CERTIFIED"
        fi
    fi

    printf '%s\n' "$status"
}

# receipt_artifact_gap <consistency>
#   Prints the typed gap token for a non-match artifact verdict, or nothing.
receipt_artifact_gap() {
    case "$1" in
        absent|incomplete|mismatch) printf 'artifact_consistency:%s\n' "$1" ;;
        *) : ;;
    esac
}

# receipt_apply_artifact_cap <status> <consistency> <strict_mode>
#   Caps the derived status according to the long-audit provenance verdict:
#   `match` keeps it; `absent`/`incomplete` cap at FUNCTIONALLY_CERTIFIED in
#   non-strict mode and become INCOMPLETE in strict mode; `mismatch` is always
#   INCOMPLETE. An operator waiver is expressed by not adding the gap and by
#   passing `match` here.
receipt_apply_artifact_cap() {
    local status="$1" consistency="$2" strict="${3:-0}"
    case "$consistency" in
        match|"") printf '%s\n' "$status" ;;
        mismatch) printf 'INCOMPLETE\n' ;;
        absent|incomplete)
            if [ "$strict" = "1" ] || [ "$strict" = "true" ]; then
                printf 'INCOMPLETE\n'
            elif [ "$status" = "CERTIFIED" ]; then
                printf 'FUNCTIONALLY_CERTIFIED\n'
            else
                printf '%s\n' "$status"
            fi
            ;;
        *) printf 'INCOMPLETE\n' ;;
    esac
}

# ---------------------------------------------------------------------------
# Mandatory test-target execution gate
# ---------------------------------------------------------------------------

# assert_ran_target <log_file> <target_name> [min_count]
#   Verifies that a specific mandatory test target was actually executed in the
#   log and that its nominal executed-test count (passed + measured) reached
#   min_count. <target_name> is the text after the "Running " banner produced
#   by cargo test, e.g. "unittests src/lib.rs", "tests/clap.rs" or
#   "tests/processor_bypass_test.rs".
#   Fail-closed against removed/renamed targets (no "Running ..." section),
#   empty filters and full skips (executed count below min_count).
assert_ran_target() {
    local log_file="$1"
    local target_name="$2"
    local min_count="${3:-1}"

    if [ ! -f "$log_file" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: log file not found: $log_file${NC}"
        return 1
    fi
    if [ -z "$target_name" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: assert_ran_target called without a target name.${NC}"
        return 1
    fi

    local run_line run_lineno result_line passed measured executed
    run_line=$(grep -n -m1 -F "Running ${target_name} " "$log_file" 2>/dev/null || true)
    if [ -z "$run_line" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: target '${target_name}' was not executed (no 'Running ...' section in $log_file). Target removed, renamed or filtered out?${NC}"
        return 1
    fi
    run_lineno="${run_line%%:*}"

    result_line=$(sed -n "$((run_lineno + 1)),\$p" "$log_file" | grep -m1 -E 'test result:' 2>/dev/null || true)
    if [ -z "$result_line" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: target '${target_name}' has no 'test result:' summary in $log_file.${NC}"
        return 1
    fi

    passed=$(printf '%s\n' "$result_line" | grep -oE '[0-9]+ passed' | grep -oE '^[0-9]+' | head -n1 || true)
    passed="${passed:-0}"
    measured=$(printf '%s\n' "$result_line" | grep -oE '[0-9]+ measured' | grep -oE '^[0-9]+' | head -n1 || true)
    measured="${measured:-0}"
    executed=$((passed + measured))

    if [ "$executed" -lt "$min_count" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: target '${target_name}' executed $executed test(s)/benchmark(s) < $min_count (empty filter or full skip?).${NC}"
        return 1
    fi
    echo -e "  Gate: target '${target_name}' executed $executed test(s)/benchmark(s) ≥ $min_count  ✓"
    return 0
}

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

CEREMONY_FAIL_EXIT=3

# ceremony_exit_code <receipt_status> <release_ceremony>
#   Prints the exit code of the release gate. Outside ceremony mode the
#   preliminary run always exits 0 (INCOMPLETE is reported, not fatal). In
#   ceremony mode only CERTIFIED passes: any other status, including an empty
#   one, prints CEREMONY_FAIL_EXIT (3, distinct from die=1).
ceremony_exit_code() {
    local status="$1" ceremony="${2:-false}"
    if [ "$ceremony" = "true" ] && [ "$status" != "CERTIFIED" ]; then
        printf '%s\n' "$CEREMONY_FAIL_EXIT"
    else
        printf '0\n'
    fi
}

# codegen_compare_verdict <pre_bolt_json> <post_bolt_json>
#   Prints the shared pre/post-link codegen coverage verdict used by the normal
#   build path and by the ceremony reuse path. Kept here (instead of duplicated
#   in both branches of build-release.sh) so the two can never drift.
codegen_compare_verdict() {
    python3 - "$1" "$2" <<'PY'
import json
import sys


def load(path):
    try:
        with open(path, "r", encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, json.JSONDecodeError):
        return None


pre = load(sys.argv[1])
post = load(sys.argv[2])
if pre is None and post is None:
    print("absent")
elif pre is None:
    print("post_only")
elif post is None:
    print("pre_only")
else:
    fewer_targets = int(post.get("targets_matched", 0)) < int(pre.get("targets_matched", 0))
    fewer_symbols = int(post.get("symbols", 0)) < int(pre.get("symbols", 0))
    if fewer_targets and fewer_symbols:
        print("post_has_fewer_symbols_and_targets")
    elif fewer_targets:
        print("post_has_fewer_targets")
    elif fewer_symbols:
        print("post_has_fewer_symbols")
    else:
        print("match")
PY
}

# ceremony_certified_verdict <certified_dir> <installed_path> <long_receipt_sha>
#   F-CER-01 identity verdict for the release ceremony. The certified copy
#   (`<certified_dir>/nam_plug.clap` + `.sha256`), the installed binary and the
#   SHA declared by the long-audit receipt must all be the same bytes. Prints a
#   typed token; the caller decides how to report/fail.
ceremony_certified_verdict() {
    local certified_dir="$1" installed="$2" long_receipt_sha="$3"
    local certified="$certified_dir/nam_plug.clap"
    local sha_file="$certified_dir/nam_plug.clap.sha256"
    local recorded actual installed_sha

    if [ ! -f "$certified" ] || [ ! -f "$sha_file" ]; then
        printf 'missing_certified_binary\n'
        return 0
    fi

    recorded=$(awk 'NR==1 {print $1}' "$sha_file" 2>/dev/null || true)
    actual=$(sha256sum "$certified" 2>/dev/null | cut -d' ' -f1)
    if [ -z "$recorded" ] || [ -z "$actual" ] || [ "$recorded" != "$actual" ]; then
        printf 'certified_copy_tampered\n'
        return 0
    fi

    if [ ! -f "$installed" ]; then
        printf 'installed_missing\n'
        return 0
    fi
    installed_sha=$(sha256sum "$installed" 2>/dev/null | cut -d' ' -f1)
    if [ "$installed_sha" != "$actual" ]; then
        printf 'installed_mismatch\n'
        return 0
    fi

    if [ -z "$long_receipt_sha" ]; then
        printf 'long_receipt_missing\n'
        return 0
    fi
    if [ "$long_receipt_sha" != "$actual" ]; then
        printf 'long_receipt_mismatch\n'
        return 0
    fi

    printf 'ok\n'
}

# codegen_post_sha <post_bolt_json>
#   Prints the SHA-256 recorded in a codegen receipt, or nothing when the field
#   is absent/unparseable. Single owner of that parsing rule.
codegen_post_sha() {
    python3 -c '
import json, sys
try:
    with open(sys.argv[1], "r", encoding="utf-8") as handle:
        print(json.load(handle).get("sha256") or "")
except Exception:
    pass
' "$1" 2>/dev/null || true
}

# ceremony_codegen_verdict <pre_bolt_json> <post_bolt_json>
#   The ceremony does not rebuild, so it reuses the codegen evidence produced by
#   the preliminary invocation. Both receipts must exist, be parseable and keep
#   the post-BOLT coverage; a shrinking or missing pair is fail-closed.
ceremony_codegen_verdict() {
    local pre="$1" post="$2" verdict post_sha

    if [ ! -f "$pre" ] || [ ! -f "$post" ]; then
        printf 'codegen_metrics_missing\n'
        return 0
    fi

    verdict=$(codegen_compare_verdict "$pre" "$post")
    case "$verdict" in
        match) : ;;
        absent) printf 'codegen_metrics_invalid\n'; return 0 ;;
        *) printf 'codegen_coverage_%s\n' "$verdict"; return 0 ;;
    esac

    post_sha=$(codegen_post_sha "$post")
    if [ -z "$post_sha" ]; then
        printf 'codegen_post_sha_missing\n'
        return 0
    fi
    if ! [[ "$post_sha" =~ ^[0-9a-f]{64}$ ]]; then
        printf 'codegen_post_sha_invalid\n'
        return 0
    fi

    printf 'ok\n'
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

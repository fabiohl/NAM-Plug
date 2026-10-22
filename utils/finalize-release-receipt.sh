#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Finalize the release receipt after the long-audit suite has run.
#
# `utils/build-release.sh` writes `target/release-receipt.json` before
# `utils/tests-long.sh` produces `target/logs/long-audit-receipt.jsonl`, so the
# build-time provenance verdict can only be pessimistic. This script reconciles
# both receipts:
#
#   1. every long-audit line must declare the same `artifact_sha256`;
#   2. that hash must equal `artifacts.clap_installed_sha256`;
#   3. `artifact_consistency` becomes match | mismatch | absent | incomplete;
#   4. the certification status is recomputed with the SAME rule used by
#      `utils/build-release.sh` (lib/_lib.sh), never a second copy;
#   5. the receipt is rewritten atomically.
#
# The operation is idempotent: finalizing twice produces byte-identical output
# while the inputs are unchanged. It never modifies `clap_installed_sha256`.
#
# Usage:
#   utils/finalize-release-receipt.sh [--receipt PATH] [--long-receipt PATH]
#                                     [--quick-receipt PATH]
#                                     [--allow-absent-long-audit]
#
# `--allow-absent-long-audit` records an explicit `operator_waiver` instead of
# the `artifact_consistency:absent` typed gap (and keeps the artifact cap from
# downgrading the status). The waiver is visible in the receipt.
#
# Environment:
#   PLUG_LONG_RECEIPT_BIN  existing verifier binary (otherwise cargo run).
#
# Exit codes: 0 finalized (including typed gaps); 1 unreadable/malformed receipt.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NAM_LIB_NO_CD=1 PHASE_TOTAL=1 source "$SCRIPT_DIR/lib/_lib.sh"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

RECEIPT_PATH="target/release-receipt.json"
LONG_RECEIPT_PATH="target/logs/long-audit-receipt.jsonl"
QUICK_RECEIPT_PATH="target/logs/quick-receipt.txt"
ALLOW_ABSENT=0
if [ "${NAM_ALLOW_ABSENT_LONG_AUDIT:-0}" = "1" ]; then
    ALLOW_ABSENT=1
fi

while [ "$#" -gt 0 ]; do
    case "$1" in
        --receipt)
            [ "$#" -ge 2 ] || die "--receipt requires a path"
            RECEIPT_PATH="$2"
            shift
            ;;
        --long-receipt)
            [ "$#" -ge 2 ] || die "--long-receipt requires a path"
            LONG_RECEIPT_PATH="$2"
            shift
            ;;
        --quick-receipt)
            [ "$#" -ge 2 ] || die "--quick-receipt requires a path"
            QUICK_RECEIPT_PATH="$2"
            shift
            ;;
        --allow-absent-long-audit)
            ALLOW_ABSENT=1
            ;;
        -h|--help)
            sed -n 's/^# \{0,1\}//p' "$0" | sed -n '1,45p'
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
    shift
done

if [ ! -f "$RECEIPT_PATH" ]; then
    die "release receipt not found: $RECEIPT_PATH (run utils/build-release.sh first)"
fi

if ! python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$RECEIPT_PATH" 2>/dev/null; then
    die "release receipt is not valid JSON: $RECEIPT_PATH"
fi

json_get() {
    python3 - "$RECEIPT_PATH" "$1" <<'PY'
import json
import sys

path, dotted = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as handle:
    value = json.load(handle)
for key in dotted.split("."):
    if not isinstance(value, dict) or key not in value:
        sys.exit(0)
    value = value[key]
if value is None:
    sys.exit(0)
if isinstance(value, bool):
    print("true" if value else "false")
else:
    print(value)
PY
}

INSTALLED_SHA="$(json_get artifacts.clap_installed_sha256)"
STRICT_MODE="$(json_get strict_mode)"
[ -n "$STRICT_MODE" ] || STRICT_MODE=false

if [ -n "${PLUG_LONG_RECEIPT_BIN:-}" ]; then
    RECEIPT_BIN=("$PLUG_LONG_RECEIPT_BIN")
elif [ -x target/debug/plug_long_receipt ]; then
    RECEIPT_BIN=(target/debug/plug_long_receipt)
else
    RECEIPT_BIN=(cargo run --quiet --locked --features testing --bin plug_long_receipt --)
fi

CONSISTENCY="absent"
DECLARED_SHA=""
if [ -f "$LONG_RECEIPT_PATH" ]; then
    EXPECTED_SHA="${INSTALLED_SHA:-none}"
    set +e
    VERDICT_OUT=$("${RECEIPT_BIN[@]}" artifact-status \
        --out "$LONG_RECEIPT_PATH" --expected-sha256 "$EXPECTED_SHA" 2>/dev/null)
    VERDICT_RC=$?
    set -e
    DECLARED_SHA=$(printf '%s\n' "$VERDICT_OUT" | awk '/^ARTIFACT_SHA256: /{print $2; exit}')
    [ "$DECLARED_SHA" = "-" ] && DECLARED_SHA=""
    case "$VERDICT_RC" in
        0) CONSISTENCY="match" ;;
        1) CONSISTENCY="mismatch" ;;
        3) CONSISTENCY="absent" ;;
        4) CONSISTENCY="incomplete" ;;
        *)
            CONSISTENCY="mismatch"
            warn "artifact-status failed (rc=$VERDICT_RC) — treating provenance as mismatch."
            ;;
    esac
fi

LONG_RECEIPT_SESSION="none"
if [ "$CONSISTENCY" = "match" ]; then
    LONG_RECEIPT_SESSION="current"
elif [ -n "$DECLARED_SHA" ]; then
    LONG_RECEIPT_SESSION="stale"
fi

ARTIFACT_GAP=$(receipt_artifact_gap "$CONSISTENCY")
WAIVER=false
if [ "$CONSISTENCY" = "absent" ] && [ "$ALLOW_ABSENT" = "1" ]; then
    ARTIFACT_GAP=""
    WAIVER=true
fi

QUICK_RECEIPT_SHA=""
if [ -f "$QUICK_RECEIPT_PATH" ]; then
    QUICK_RECEIPT_SHA=$(sha256sum "$QUICK_RECEIPT_PATH" | cut -d' ' -f1)
else
    warn "quick receipt absent: $QUICK_RECEIPT_PATH (not used for provenance reconciliation)."
fi

RECEIPT_DIR="$(dirname "$RECEIPT_PATH")"
mkdir -p "$RECEIPT_DIR"
RECEIPT_TMP=$(mktemp "$RECEIPT_DIR/.release-receipt.XXXXXX")

# First pass: reconcile provenance fields and skipped gates, and emit the
# inputs of the shared status rule for the shell helper.
STATUS_INPUTS=$(python3 - "$RECEIPT_PATH" "$RECEIPT_TMP" "$LONG_RECEIPT_PATH" \
    "$CONSISTENCY" "$DECLARED_SHA" "$LONG_RECEIPT_SESSION" "$ARTIFACT_GAP" \
    "$WAIVER" "$QUICK_RECEIPT_SHA" <<'PY'
import json
import sys
(
    receipt_path,
    out_path,
    long_path,
    consistency,
    declared_sha,
    long_session,
    artifact_gap,
    waiver,
    quick_sha,
) = sys.argv[1:10]

with open(receipt_path, encoding="utf-8") as handle:
    receipt = json.load(handle)

long_audit_session = None
if long_path:
    try:
        with open(long_path, encoding="utf-8") as handle:
            entries = [json.loads(line) for line in handle if line.strip()]
        for entry in reversed(entries):
            if entry.get("phase_id") == "overall":
                long_audit_session = entry.get("timestamp")
                break
        if long_audit_session is None and entries:
            long_audit_session = entries[-1].get("timestamp")
    except (OSError, json.JSONDecodeError):
        long_audit_session = None

artifacts = receipt.setdefault("artifacts", {})
artifacts["artifact_consistency"] = consistency
artifacts["long_receipt_artifact_sha256"] = declared_sha or None
artifacts["long_receipt_session"] = long_session
artifacts["long_audit_session"] = long_audit_session
if quick_sha:
    artifacts["quick_receipt_sha256"] = quick_sha

skipped = [
    gate
    for gate in receipt.get("skipped_gates", [])
    if not str(gate).startswith("artifact_consistency:")
]
if artifact_gap and artifact_gap not in skipped:
    skipped.append(artifact_gap)
receipt["skipped_gates"] = skipped

if waiver == "true":
    existing = receipt.get("operator_waiver") or {}
    recorded = existing.get("recorded_utc")
    if existing.get("type") != "allow_absent_long_audit" or not recorded:
        recorded = receipt.get("timestamp_utc") or "unknown"
    receipt["operator_waiver"] = {
        "type": "allow_absent_long_audit",
        "recorded_utc": recorded,
    }
else:
    receipt.pop("operator_waiver", None)

with open(out_path, "w", encoding="utf-8") as handle:
    json.dump(receipt, handle, indent=2)
    handle.write("\n")

provenance = receipt.get("provenance", {}) or {}
engine = receipt.get("engine_provenance", {}) or {}
quick = receipt.get("quick_qa_audit", {}) or {}
optimizations = receipt.get("optimizations", {}) or {}
performance = receipt.get("performance_certification", {}) or {}

non_artifact_skips = sum(
    1 for gate in skipped if not str(gate).startswith("artifact_consistency:")
)

def as_bool(value):
    return "true" if value is True else "false"

print(f"git_dirty={as_bool(provenance.get('git_dirty'))}")
print(f"core_dirty={as_bool(engine.get('git_dirty'))}")
print(f"perf_status={performance.get('gate_status') or 'UNKNOWN'}")
print(f"quick_passed={'true' if quick.get('status') == 'PASSED' else 'false'}")
print(f"heap_passed={as_bool(quick.get('heap_audit_passed'))}")
print(f"pgo_applied={as_bool(optimizations.get('pgo_applied'))}")
print(f"bolt_applied={as_bool(optimizations.get('bolt_applied'))}")
print(f"non_artifact_skip_count={non_artifact_skips}")
PY
)

GIT_DIRTY=false
CORE_DIRTY=false
PERF_STATUS=UNKNOWN
QUICK_PASSED=false
HEAP_PASSED=false
PGO_APPLIED=false
BOLT_APPLIED=false
NON_ARTIFACT_SKIP_COUNT=0
while IFS='=' read -r key value; do
    [ -n "$key" ] || continue
    case "$key" in
        git_dirty) GIT_DIRTY="$value" ;;
        core_dirty) CORE_DIRTY="$value" ;;
        perf_status) PERF_STATUS="$value" ;;
        quick_passed) QUICK_PASSED="$value" ;;
        heap_passed) HEAP_PASSED="$value" ;;
        pgo_applied) PGO_APPLIED="$value" ;;
        bolt_applied) BOLT_APPLIED="$value" ;;
        non_artifact_skip_count) NON_ARTIFACT_SKIP_COUNT="$value" ;;
    esac
done <<< "$STATUS_INPUTS"

RECEIPT_STATUS=$(receipt_derive_status \
    "$GIT_DIRTY" "$CORE_DIRTY" "$PERF_STATUS" "$QUICK_PASSED" "$HEAP_PASSED" \
    "$PGO_APPLIED" "$BOLT_APPLIED" "$NON_ARTIFACT_SKIP_COUNT")

CAP_CONSISTENCY="$CONSISTENCY"
if [ "$WAIVER" = "true" ]; then
    CAP_CONSISTENCY="match"
fi
RECEIPT_STATUS=$(receipt_apply_artifact_cap \
    "$RECEIPT_STATUS" "$CAP_CONSISTENCY" "$STRICT_MODE")

# Second pass: write the derived status. The original receipt is only replaced
# by a completed, valid file (atomic rename on the same filesystem).
python3 - "$RECEIPT_TMP" "$RECEIPT_STATUS" <<'PY'
import json
import sys

path, status = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as handle:
    receipt = json.load(handle)
receipt["status"] = status
with open(path, "w", encoding="utf-8") as handle:
    json.dump(receipt, handle, indent=2)
    handle.write("\n")
PY

mv -f "$RECEIPT_TMP" "$RECEIPT_PATH"

echo "RELEASE_RECEIPT: $RECEIPT_PATH"
echo "ARTIFACT_CONSISTENCY: $CONSISTENCY"
echo "ARTIFACT_SHA256: ${DECLARED_SHA:--}"
echo "LONG_AUDIT_SESSION: ${LONG_RECEIPT_SESSION}"
echo "STATUS: $RECEIPT_STATUS"
[ "$WAIVER" = "true" ] && echo "OPERATOR_WAIVER: allow_absent_long_audit"
exit 0

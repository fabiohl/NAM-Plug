#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Regression test for utils/finalize-release-receipt.sh (S2).
#
# The finalizer must bind the release receipt to the long-audit JSONL of the
# SAME session, distinguish match/mismatch/absent/incomplete, be idempotent, and
# never promote an absent long audit to CERTIFIED without an explicit waiver.
#
# Everything runs in a temp dir; the real receipt and installed plugin are never
# touched.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_DIR"

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD}  test_finalize_release_receipt: long-audit reconciliation     ${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

cargo build --quiet --features testing --bin plug_long_receipt
export PLUG_LONG_RECEIPT_BIN="$PROJECT_DIR/target/debug/plug_long_receipt"

ARTIFACT="$TMP_DIR/nam_plug.clap"
printf 'finalizer-artifact-bytes' > "$ARTIFACT"
ARTIFACT_SHA=$(sha256sum "$ARTIFACT" | cut -d' ' -f1)
BAD_SHA="deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"

write_receipt() {
    local path="$1" strict="$2"
    cat > "$path" <<JSON
{
  "schema_version": "1.1",
  "timestamp_utc": "2026-10-22T12:00:00Z",
  "status": "CERTIFIED",
  "strict_mode": $strict,
  "skipped_gates": [],
  "provenance": {"git_dirty": false},
  "engine_provenance": {"git_dirty": false},
  "quick_qa_audit": {"status": "PASSED", "heap_audit_passed": true},
  "optimizations": {"pgo_applied": true, "bolt_applied": true},
  "performance_certification": {"gate_status": "PASSED"},
  "artifacts": {
    "clap_installed_path": "$ARTIFACT",
    "clap_installed_sha256": "$ARTIFACT_SHA",
    "artifact_consistency": "absent",
    "long_receipt_artifact_sha256": null,
    "long_receipt_session": "none",
    "long_audit_session": null
  }
}
JSON
}

write_long() {
    local path="$1" sha="$2" timestamp="$3"
    : > "$path"
    local phase
    for phase in phase1 phase2 phase3 phase4 overall; do
        local exercised=false
        [ "$phase" = "phase3" ] && exercised=true
        printf '{"phase_id":"%s","name":"phase","status":"PASSED","duration_ms":1,"tests_executed":1,"gaps":[],"timestamp":"%s","artifact_path":"%s","artifact_sha256":"%s","artifact_exercised":%s}\n' \
            "$phase" "$timestamp" "$ARTIFACT" "$sha" "$exercised" >> "$path"
    done
}

finalize() {
    utils/finalize-release-receipt.sh --receipt "$TMP_DIR/receipt.json" \
        --long-receipt "$TMP_DIR/long.jsonl" --quick-receipt "$TMP_DIR/quick-receipt.txt" "$@"
}

field() { jq -r "$1" "$TMP_DIR/receipt.json"; }

assert_eq() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" != "$actual" ]; then
        echo -e "${RED}[FAIL] $label: expected '$expected', got '$actual'${NC}" >&2
        exit 1
    fi
    echo -e "  ${GREEN}✓${NC} $label = $actual"
}

printf 'OVERALL: PASSED\n' > "$TMP_DIR/quick-receipt.txt"

# ── 1. Match + idempotency ─────────────────────────────────────────────────
write_receipt "$TMP_DIR/receipt.json" false
write_long "$TMP_DIR/long.jsonl" "$ARTIFACT_SHA" "2026-10-22T12:30:00Z"
finalize >/dev/null
assert_eq "match status" "CERTIFIED" "$(field '.status')"
assert_eq "match consistency" "match" "$(field '.artifacts.artifact_consistency')"
assert_eq "declared sha bound" "$ARTIFACT_SHA" "$(field '.artifacts.long_receipt_artifact_sha256')"
assert_eq "long audit session" "2026-10-22T12:30:00Z" "$(field '.artifacts.long_audit_session')"
assert_eq "no artifact gap" "0" "$(jq '[.skipped_gates[] | select(startswith("artifact_consistency:"))] | length' "$TMP_DIR/receipt.json")"

BEFORE=$(sha256sum "$TMP_DIR/receipt.json" | cut -d' ' -f1)
finalize >/dev/null
AFTER=$(sha256sum "$TMP_DIR/receipt.json" | cut -d' ' -f1)
assert_eq "idempotent rewrite" "$BEFORE" "$AFTER"

# ── 2. Mismatch (one JSONL line cites another binary) ──────────────────────
write_receipt "$TMP_DIR/receipt.json" false
write_long "$TMP_DIR/long.jsonl" "$ARTIFACT_SHA" "2026-10-22T12:30:00Z"
sed -i '2s/"artifact_sha256":"[^"]*"/"artifact_sha256":"'"$BAD_SHA"'"/' "$TMP_DIR/long.jsonl"
finalize >/dev/null
assert_eq "mismatch consistency" "mismatch" "$(field '.artifacts.artifact_consistency')"
assert_eq "mismatch status" "INCOMPLETE" "$(field '.status')"
assert_eq "mismatch gap" "1" "$(jq '[.skipped_gates[] | select(. == "artifact_consistency:mismatch")] | length' "$TMP_DIR/receipt.json")"

# ── 3. Absent (no long receipt) ────────────────────────────────────────────
write_receipt "$TMP_DIR/receipt.json" false
rm -f "$TMP_DIR/long.jsonl"
finalize >/dev/null
assert_eq "absent consistency" "absent" "$(field '.artifacts.artifact_consistency')"
assert_eq "absent non-strict status" "FUNCTIONALLY_CERTIFIED" "$(field '.status')"
assert_eq "absent gap" "1" "$(jq '[.skipped_gates[] | select(. == "artifact_consistency:absent")] | length' "$TMP_DIR/receipt.json")"

# ── 4. Absent + strict ─────────────────────────────────────────────────────
write_receipt "$TMP_DIR/receipt.json" true
rm -f "$TMP_DIR/long.jsonl"
finalize >/dev/null
assert_eq "absent strict status" "INCOMPLETE" "$(field '.status')"

# ── 5. Incomplete (only some lines declare a hash) ─────────────────────────
write_receipt "$TMP_DIR/receipt.json" false
write_long "$TMP_DIR/long.jsonl" "$ARTIFACT_SHA" "2026-10-22T12:30:00Z"
sed -i '2s/,"artifact_sha256":"[^"]*"//' "$TMP_DIR/long.jsonl"
finalize >/dev/null
assert_eq "incomplete consistency" "incomplete" "$(field '.artifacts.artifact_consistency')"
assert_eq "incomplete status" "FUNCTIONALLY_CERTIFIED" "$(field '.status')"
assert_eq "incomplete gap" "1" "$(jq '[.skipped_gates[] | select(. == "artifact_consistency:incomplete")] | length' "$TMP_DIR/receipt.json")"

# ── 6. Explicit operator waiver ────────────────────────────────────────────
write_receipt "$TMP_DIR/receipt.json" true
rm -f "$TMP_DIR/long.jsonl"
finalize --allow-absent-long-audit >/dev/null
assert_eq "waiver status" "CERTIFIED" "$(field '.status')"
assert_eq "waiver recorded" "allow_absent_long_audit" "$(field '.operator_waiver.type')"
assert_eq "waiver has no artifact gap" "0" "$(jq '[.skipped_gates[] | select(startswith("artifact_consistency:"))] | length' "$TMP_DIR/receipt.json")"
BEFORE=$(sha256sum "$TMP_DIR/receipt.json" | cut -d' ' -f1)
finalize --allow-absent-long-audit >/dev/null
AFTER=$(sha256sum "$TMP_DIR/receipt.json" | cut -d' ' -f1)
assert_eq "waiver idempotent" "$BEFORE" "$AFTER"

# ── 7. artifact_exercised rendering ────────────────────────────────────────
"$PLUG_LONG_RECEIPT_BIN" append --phase-id phase1 --name "lib phase" --status PASSED \
    --duration-ms 1 --out "$TMP_DIR/exercised.jsonl" \
    --artifact-path "$ARTIFACT" --artifact-sha256 "$ARTIFACT_SHA" \
    --artifact-exercised false >/dev/null 2>&1
"$PLUG_LONG_RECEIPT_BIN" append --phase-id phase3 --name "dso phase" --status PASSED \
    --duration-ms 1 --out "$TMP_DIR/exercised.jsonl" \
    --artifact-path "$ARTIFACT" --artifact-sha256 "$ARTIFACT_SHA" \
    --artifact-exercised true >/dev/null 2>&1
assert_eq "phase1 artifact_exercised" "false" "$(jq -r 'select(.phase_id=="phase1") | .artifact_exercised' "$TMP_DIR/exercised.jsonl")"
assert_eq "phase3 artifact_exercised" "true" "$(jq -r 'select(.phase_id=="phase3") | .artifact_exercised' "$TMP_DIR/exercised.jsonl")"

echo -e "\n${GREEN}${BOLD}✓ Release-receipt finalization reconciles and never silently certifies.${NC}"

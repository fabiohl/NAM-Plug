#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Regression test for the long-audit artifact provenance gate (E-A).
#
# The release receipt must never certify phase evidence that cites a different
# binary than the one installed. This test drives the real verifier binary:
#   (a) receipt SHA == installed artifact       -> exit 0, "match"
#   (b) receipt SHA != installed artifact       -> exit 1, "mismatch"
#   (c) receipt without artifact provenance      -> exit 3, "absent"
#   (d) incompletely stamped receipt             -> exit 4, "incomplete"
#
# Never touches the real receipt or installed plugin (everything in a temp dir).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

ARTIFACT="$TMP_DIR/nam_plug.clap"
printf 'certified-binary-bytes' > "$ARTIFACT"
ARTIFACT_SHA=$(sha256sum "$ARTIFACT" | cut -d' ' -f1)

write_receipt() {
    local path="$1" sha="$2"
    if [ -n "$sha" ]; then
        printf '{"phase_id":"phase1","name":"GC","status":"PASSED","duration_ms":1,"tests_executed":1,"gaps":[],"timestamp":"2026-01-01T00:00:00Z","artifact_path":"%s","artifact_sha256":"%s"}\n' \
            "$ARTIFACT" "$sha" > "$path"
        printf '{"phase_id":"overall","name":"Long Audit Suite","status":"PASSED","duration_ms":1,"tests_executed":1,"gaps":[],"timestamp":"2026-01-01T00:00:00Z","artifact_path":"%s","artifact_sha256":"%s"}\n' \
            "$ARTIFACT" "$sha" >> "$path"
    else
        printf '{"phase_id":"phase1","name":"GC","status":"PASSED","duration_ms":1,"tests_executed":1,"gaps":[],"timestamp":"2026-01-01T00:00:00Z"}\n' > "$path"
    fi
}

run_verifier() {
    local receipt="$1"
    ( cd "$PROJECT_DIR" && cargo run --quiet --locked --features testing --bin plug_long_receipt -- \
        verify-artifact --out "$receipt" --artifact "$ARTIFACT" )
}

expect_case() {
    local label="$1" expected_rc="$2" expected_token="$3" receipt="$4"
    set +e
    OUT=$(run_verifier "$receipt" 2>&1)
    RC=$?
    set -e
    if [ "$RC" -ne "$expected_rc" ]; then
        echo -e "${RED}[FAIL] $label: expected exit $expected_rc, got $RC\n$OUT${NC}" >&2
        exit 1
    fi
    if ! printf '%s\n' "$OUT" | grep -q "^ARTIFACT_CONSISTENCY: $expected_token$"; then
        echo -e "${RED}[FAIL] $label: expected ARTIFACT_CONSISTENCY=$expected_token\n$OUT${NC}" >&2
        exit 1
    fi
    echo -e "  ${GREEN}✓${NC} $label ($expected_token, rc=$RC)"
}

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD}  test_receipt_artifact_consistency: long-audit SHA gate (E-A) ${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

write_receipt "$TMP_DIR/match.jsonl" "$ARTIFACT_SHA"
expect_case "matching receipt" 0 "match" "$TMP_DIR/match.jsonl"

write_receipt "$TMP_DIR/mismatch.jsonl" "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
expect_case "divergent receipt" 1 "mismatch" "$TMP_DIR/mismatch.jsonl"

write_receipt "$TMP_DIR/absent.jsonl" ""
expect_case "legacy receipt without provenance" 3 "absent" "$TMP_DIR/absent.jsonl"

write_receipt "$TMP_DIR/incomplete.jsonl" "$ARTIFACT_SHA"
printf '{"phase_id":"phase2","name":"Teardown","status":"PASSED","duration_ms":1,"tests_executed":1,"gaps":[],"timestamp":"2026-01-01T00:00:00Z"}\n' >> "$TMP_DIR/incomplete.jsonl"
set +e
OUT=$(run_verifier "$TMP_DIR/incomplete.jsonl" 2>&1)
RC=$?
set -e
if [ "$RC" -ne 4 ] || ! printf '%s\n' "$OUT" | grep -q "^ARTIFACT_CONSISTENCY: incomplete$"; then
    echo -e "${RED}[FAIL] incomplete receipt: expected incomplete/exit 4, got rc=$RC\n$OUT${NC}" >&2
    exit 1
fi
echo -e "  ${GREEN}✓${NC} incomplete receipt (incomplete, rc=$RC)"

echo -e "\n${GREEN}${BOLD}✓ Artifact provenance gate rejects divergent receipts.${NC}"

#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Contract test for the build-release.sh cryptographic receipt generator block.
#
# Verifies that the embedded Python receipt generator in utils/build-release.sh:
#   1. Does not trigger bash command substitution errors (e.g. unescaped backticks or double quotes in comments)
#   2. Produces structurally valid, parseable JSON
#   3. Correctly handles strict mode, skipped gates, provenance, and optimization proof fields

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

cd "$PROJECT_DIR"

BUILD_RELEASE="$PROJECT_DIR/utils/build-release.sh"

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD} test_build_release_receipt_gen: receipt generator block       ${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

# 1. Verify no dangerous backticks or double quotes exist inside the python3 -c block
echo "Checking build-release.sh receipt block for unescaped backticks and quotes..."
python3 - <<'PY'
import sys

with open("utils/build-release.sh", "r", encoding="utf-8") as f:
    lines = f.readlines()

in_block = False
block_lines = []
receipt_start = [idx for idx, line in enumerate(lines, start=1) if 'python3 -c "' in line][-1]
for idx, line in enumerate(lines, start=1):
    if idx == receipt_start:
        in_block = True
        continue
    if in_block:
        if line.strip().startswith('" "$GATE_SKIPS_JOINED"'):
            in_block = False
            break
        # Comments in a double-quoted bash string cannot have backticks or unescaped double quotes
        stripped = line.strip()
        if stripped.startswith('#'):
            if '`' in stripped:
                print(f"FAIL: backtick found in comment at line {idx}: {stripped}", file=sys.stderr)
                sys.exit(1)
            if '"' in stripped:
                print(f"FAIL: double quote found in comment at line {idx}: {stripped}", file=sys.stderr)
                sys.exit(1)

print("  ✓ No unescaped backticks or double quotes found in comments within receipt generator block.")
PY

# 2. Extract and run the receipt generator block in a subshell with simulated variables
RECEIPT_OUT="$TMP_DIR/test-receipt.json"

export TIMESTAMP="2026-10-22T20:00:00Z"
export RECEIPT_STATUS="CERTIFIED"
export STRICT_MODE=1
export VERSION="1.0.0"
export GIT_COMMIT="abcdef1234567890"
export GIT_BRANCH="main"
export GIT_DIRTY="false"
export CARGO_LOCK_SHA="e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
export RUSTC_VER="rustc 1.85.0"
export CLAP_RUSTFLAGS="-C target-cpu=x86-64-v3"
export CORE_PATH=""
export CORE_VERSION="1.0.1"
export CORE_GIT_COMMIT=""
export CORE_GIT_BRANCH=""
export CORE_GIT_DIRTY="false"
export CORE_TREE_SHA=""
export QUICK_QA_PASSED="true"
export HEAP_AUDIT_PASSED="true"
export QUICK_RECEIPT_SHA="aaaabbbbccccddddeeeeffff0000111122223333444455556666777788889999"
export HEAP_AUDIT_LOG_SHA="bbbbccccddddeeeeffff0000111122223333444455556666777788889999aaaa"
export QUICK_RECEIPT_ARTIFACT_SHA="ccccddddeeeeffff0000111122223333444455556666777788889999aaaabbbb"
export QUICK_RECEIPT_STRICT="1"
export FINAL_CLAP_SHA="dddeeeeffff0000111122223333444455556666777788889999aaaabbbbcccc"
export CERTIFIED_CLAP="/tmp/nam-plug-test/target/certified/nam_plug.clap"
export CERTIFIED_COPY_SHA="dddeeeeffff0000111122223333444455556666777788889999aaaabbbbcccc"
export REUSED_CERTIFIED_BINARY="true"
export PRE_STRIP_ARTIFACT_SHA="9999888877776666555544443333222211110000ffffeeeeddddccccbbbbaaaa"
export USE_PGO="true"
export CLAP_BOLT_APPLIED="true"
export BOLT_PROFILE_SHA="eeeeffff0000111122223333444455556666777788889999aaaabbbbccccdddd"
export BOLT_VERSION="19.1.0"
export BOLT_PROOF="ab-opt-ceremony:test-proof-id"
export BOLT_LBR_SAMPLES="500000"
export BOLT_LBR_FDATA_SHA256="ffff0000111122223333444455556666777788889999aaaabbbbccccddddeeee"
export CODEGEN_COMPARISON="identical"
export CODEGEN_PRE_JSON=""
export CODEGEN_POST_JSON=""
export PGO_WORKLOAD_STATUS="valid"
export PGO_WORKLOAD_GAIN="2.5"
export PGO_WORKLOAD_SCENARIOS="6"
export PGO_WORKLOAD_CALLBACKS="12000"
export PGO_CLIPPING_PHASE1="0"
export PGO_CLIPPING_BOLT="0"
export PGO_WORKLOAD_LOG=""
export PGO_WORKLOAD_LOG_SHA=""
export PGO_WORKLOAD_RECEIPT_SHA="abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
export PACKAGING_ALLOWED="true"
export PARITY_OVERSAMPLE_STATUS="PASS"
export PARITY_OVERSAMPLE_REASON=""
export PARITY_CABSIM_STATUS="PASS"
export PARITY_CABSIM_REASON=""
export CLAP_VALIDATOR_STATUS="PASSED"
export CLAP_VALIDATOR_TESTS_RUN="21"
export CLAP_VALIDATOR_PASSED="19"
export CLAP_VALIDATOR_FAILED="0"
export CLAP_VALIDATOR_SKIPPED="2"
export CLAP_VALIDATOR_SKIP_REASONS="The plugin does not implement the 'clap.note-ports' extension."
export CLAP_VALIDATOR_LOG=""
export PERF_ENVIRONMENT_DEGRADED="true"
export PERF_ENVIRONMENT_DEGRADED_REASONS="bench_core_not_isolated:8;irq_isolation_shared:8:35"
export PERF_GATE_STATUS="PASS"
export PERF_REPORT_PATH=""
export PERF_REPORT_SHA=""
export CLAP_TARGET="/home/fabio/.clap/nam_plug.clap"
export LONG_RECEIPT_PATH=""
export LONG_RECEIPT_ARTIFACT_SHA=""
export LONG_RECEIPT_SESSION=""
export ARTIFACT_CONSISTENCY="match"
export INSTALLED_AT_START_SHA=""
export TARBALL=""
export BUILD_TARBALL="false"
export TARBALL_SHA=""
export FLATPAK_BUNDLE=""
export BUILD_FLATPAK="false"
export FLATPAK_SHA=""
export ORACLE_BIN=""
export ORACLE_SHA=""
export model_fixture=""
export FIXTURE_SHA=""
export RECEIPT_TMP="$RECEIPT_OUT"
export GATE_SKIPS_JOINED="gui_tests:unverified|artifact_consistency:absent"
export NAM_CEREMONY_SCRIPT_SHA256="5f0c2a1b9d8e7f6a5b4c3d2e1f00112233445566778899aabbccddeeff001122"
export NAM_CEREMONY_PHASES="build,long,final"

# Extract receipt python execution block directly from build-release.sh
read -r START_LINE END_LINE < <(python3 - <<'PY'
with open("utils/build-release.sh", "r", encoding="utf-8") as f:
    lines = f.readlines()
start = [i + 1 for i, l in enumerate(lines) if 'python3 -c "' in l][-1]
end = [i + 1 for i, l in enumerate(lines) if '" "$GATE_SKIPS_JOINED"' in l][-1]
print(f"{start} {end}")
PY
)


SUB_OUTPUT=$(sed -n "${START_LINE},${END_LINE}p" "$BUILD_RELEASE" | bash -s -- "$GATE_SKIPS_JOINED" 2>&1) || {
    echo -e "${RED}FAIL: Receipt generation block returned non-zero exit code.${NC}" >&2
    echo "$SUB_OUTPUT" >&2
    exit 1
}

if [ -n "$SUB_OUTPUT" ]; then
    echo -e "${RED}FAIL: Receipt generation block produced unexpected output/warnings:${NC}" >&2
    echo "$SUB_OUTPUT" >&2
    exit 1
fi

[ -f "$RECEIPT_OUT" ] || {
    echo -e "${RED}FAIL: Expected receipt file $RECEIPT_OUT was not created.${NC}" >&2
    exit 1
}

# 3. Validate generated JSON schema
python3 - "$RECEIPT_OUT" <<'PY'
import json, sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    receipt = json.load(f)

assert receipt["schema_version"] == "1.1"
assert receipt["status"] == "CERTIFIED"
assert receipt["strict_mode"] is True
assert receipt["skipped_gates"] == ["gui_tests:unverified", "artifact_consistency:absent"]
assert receipt["package"]["name"] == "nam-plug"
assert receipt["package"]["version"] == "1.0.0"
assert receipt["quick_qa_audit"]["status"] == "PASSED"
assert receipt["quick_qa_audit"]["strict"] is True
assert receipt["gates"]["parity_oversample"]["status"] == "PASS"
assert receipt["gates"]["parity_cabsim"]["status"] == "PASS"
assert receipt["clap_validator"]["status"] == "PASSED"
assert receipt["clap_validator"]["tests_run"] == 21
assert receipt["clap_validator"]["skipped"] == 2
assert receipt["clap_validator"]["skip_reasons"] == [
    "The plugin does not implement the 'clap.note-ports' extension."
]
assert receipt["clap_validator"]["log_sha256"] is None
assert receipt["performance_certification"]["environment_degraded"] is True
assert receipt["performance_certification"]["environment_degraded_reasons"] == [
    "bench_core_not_isolated:8",
    "irq_isolation_shared:8:35",
]
assert receipt["optimizations"]["bolt_proof"] == "ab-opt-ceremony:test-proof-id"
assert receipt["optimizations"]["unproven_claims"] == []
assert receipt["pgo_workload"]["gain"] == 2.5
assert receipt["pgo_workload"]["scenarios"] == 6
assert receipt["pgo_workload"]["receipt_sha256"] == "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
assert receipt["artifacts"]["reused_certified_binary"] is True
assert receipt["artifacts"]["certified_copy_sha256"] == "dddeeeeffff0000111122223333444455556666777788889999aaaabbbbcccc"
assert receipt["artifacts"]["pre_strip_artifact_sha256"] == "9999888877776666555544443333222211110000ffffeeeeddddccccbbbbaaaa"
assert receipt["artifacts"]["certified_copy_path"] == "/tmp/nam-plug-test/target/certified/nam_plug.clap"
assert receipt["provenance"]["ceremony"] == {
    "script_sha256": "5f0c2a1b9d8e7f6a5b4c3d2e1f00112233445566778899aabbccddeeff001122",
    "phases": ["build", "long", "final"],
}

print("  ✓ Receipt JSON generated and verified successfully.")
PY

# 4. T4.1: provenance.ceremony is null for manual runs (env absent) and for a
#    malformed SHA; it never fails the generator.
run_receipt_block() {
    local out="$1"
    shift
    sed -n "${START_LINE},${END_LINE}p" "$BUILD_RELEASE" \
        | env "$@" RECEIPT_TMP="$out" bash -s -- "$GATE_SKIPS_JOINED" 2>&1
}

for case_name in absent malformed; do
    CASE_OUT="$TMP_DIR/receipt-$case_name.json"
    if [ "$case_name" = "absent" ]; then
        CASE_ENV=(-u NAM_CEREMONY_SCRIPT_SHA256 -u NAM_CEREMONY_PHASES)
    else
        CASE_ENV=(NAM_CEREMONY_SCRIPT_SHA256=not-a-sha256 NAM_CEREMONY_PHASES=build)
    fi
    CASE_OUTPUT=$(run_receipt_block "$CASE_OUT" "${CASE_ENV[@]}") || {
        echo -e "${RED}FAIL: ceremony provenance ($case_name) generator returned non-zero.${NC}" >&2
        echo "$CASE_OUTPUT" >&2
        exit 1
    }
    [ -z "$CASE_OUTPUT" ] || {
        echo -e "${RED}FAIL: ceremony provenance ($case_name) produced unexpected output.${NC}" >&2
        echo "$CASE_OUTPUT" >&2
        exit 1
    }
    python3 - "$CASE_OUT" <<'PY'
import json, sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    receipt = json.load(f)

assert receipt["schema_version"] == "1.1"
assert receipt["provenance"]["ceremony"] is None
PY
done
echo "  ✓ provenance.ceremony is null when absent or malformed."

# 5. gate-pass-only must name the unproven claim instead of implying a BOLT win.
GATE_ONLY_OUT="$TMP_DIR/receipt-gate-only.json"
GATE_ONLY_OUTPUT=$(run_receipt_block "$GATE_ONLY_OUT" BOLT_PROOF=gate-pass-only) || {
    echo -e "${RED}FAIL: gate-pass-only receipt generator returned non-zero.${NC}" >&2
    echo "$GATE_ONLY_OUTPUT" >&2
    exit 1
}
[ -z "$GATE_ONLY_OUTPUT" ] || {
    echo -e "${RED}FAIL: gate-pass-only receipt produced unexpected output.${NC}" >&2
    echo "$GATE_ONLY_OUTPUT" >&2
    exit 1
}
python3 - "$GATE_ONLY_OUT" <<'PYCHECK'
import json, sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    receipt = json.load(f)

assert receipt["optimizations"]["bolt_proof"] == "gate-pass-only"
assert receipt["optimizations"]["unproven_claims"] == ["bolt_win"]
PYCHECK
echo "  ✓ gate-pass-only records unproven_claims=[bolt_win]."

echo -e "${GREEN}${BOLD}PASS: test_build_release_receipt_gen${NC}"

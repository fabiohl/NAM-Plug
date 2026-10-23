#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Contract test for the certified-artifact identity chain (Sprint S3, F-CER-01).
#
# The release ceremony must not rebuild after the long audit. Instead it reuses
# the exact bytes captured by the preliminary build and verifies:
#   sha256(long-audit receipt) == sha256(~/.clap/nam_plug.clap)
#                              == sha256(target/certified/nam_plug.clap)
#
# Stub cases (no PGO/BOLT, no real ~/.clap, no long suite):
#   (a) coherent SHAs                              -> ok
#   (b) tampered certified copy                    -> certified_copy_tampered
#   (c) installed binary differs from certificate   -> installed_mismatch
#   (d) long receipt declares another artifact      -> long_receipt_mismatch
#   (e) target/certified absent                     -> missing_certified_binary
#   (f) installed artifact absent                   -> installed_missing
#   (g) long receipt without artifact SHA           -> long_receipt_missing
#
# The test also executes the real verdict guard and asserts preflight ordering,
# so removing the fail-closed die or moving the preflight behind a rebuild fails.

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

LIB_FILE="$PROJECT_DIR/utils/lib/_lib.sh"
BUILD_RELEASE="$PROJECT_DIR/utils/build-release.sh"

NAM_LIB_NO_CD=1 source "$LIB_FILE"

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD}  test_ceremony_reuse_binary: F-CER-01 identity chain         ${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

fail() {
    echo -e "${RED}FAIL: $1${NC}" >&2
    exit 1
}

expect_verdict() {
    local label="$1" expected="$2" dir="$3" installed="$4" long_sha="$5"
    local got
    got=$(ceremony_certified_verdict "$dir" "$installed" "$long_sha")
    if [ "$got" != "$expected" ]; then
        fail "certified verdict [$label]: expected $expected, got $got"
    fi
    echo -e "  ${GREEN}✓${NC} certified verdict: $label -> $got"
}

expect_codegen() {
    local label="$1" expected="$2" pre="$3" post="$4"
    local got
    got=$(ceremony_codegen_verdict "$pre" "$post")
    if [ "$got" != "$expected" ]; then
        fail "codegen verdict [$label]: expected $expected, got $got"
    fi
    echo -e "  ${GREEN}✓${NC} codegen verdict: $label -> $got"
}

# ── Certified-artifact chain ────────────────────────────────────────────────
CERT_DIR="$TMP_DIR/certified"
mkdir -p "$CERT_DIR" "$TMP_DIR/install"
printf 'certified-clap-bytes' > "$CERT_DIR/nam_plug.clap"
CERT_SHA=$(sha256sum "$CERT_DIR/nam_plug.clap" | cut -d' ' -f1)
printf '%s\n' "$CERT_SHA" > "$CERT_DIR/nam_plug.clap.sha256"
INSTALLED="$TMP_DIR/install/nam_plug.clap"
cp "$CERT_DIR/nam_plug.clap" "$INSTALLED"
OTHER_SHA="1111111111111111111111111111111111111111111111111111111111111111"

expect_verdict "(a) coherent SHAs" ok "$CERT_DIR" "$INSTALLED" "$CERT_SHA"
expect_verdict "(d) long receipt declares another artifact" long_receipt_mismatch "$CERT_DIR" "$INSTALLED" "$OTHER_SHA"
expect_verdict "(g) long receipt without artifact SHA" long_receipt_missing "$CERT_DIR" "$INSTALLED" ""

printf 'tampered-bytes' >> "$CERT_DIR/nam_plug.clap"
expect_verdict "(b) tampered certified copy" certified_copy_tampered "$CERT_DIR" "$INSTALLED" "$CERT_SHA"

# Restore a clean certificate, then diverge the installed binary.
printf 'certified-clap-bytes' > "$CERT_DIR/nam_plug.clap"
cp "$CERT_DIR/nam_plug.clap" "$INSTALLED"
expect_verdict "(a') restored coherent chain" ok "$CERT_DIR" "$INSTALLED" "$CERT_SHA"

printf 'installed-other-bytes' > "$INSTALLED"
expect_verdict "(c) installed differs from certificate" installed_mismatch "$CERT_DIR" "$INSTALLED" "$CERT_SHA"

rm -f "$INSTALLED"
expect_verdict "(f) installed artifact absent" installed_missing "$CERT_DIR" "$INSTALLED" "$CERT_SHA"

expect_verdict "(e) target/certified absent" missing_certified_binary "$TMP_DIR/absent" "$INSTALLED" "$CERT_SHA"

# ── Reused codegen evidence ─────────────────────────────────────────────────
PRE_JSON="$TMP_DIR/codegen-pre-bolt.json"
POST_JSON="$TMP_DIR/codegen-post-bolt.json"
cat > "$PRE_JSON" <<'EOF'
{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","symbols":7,"targets_matched":7}
EOF
cat > "$POST_JSON" <<'EOF'
{"sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","symbols":7,"targets_matched":7}
EOF

expect_codegen "(a) matching pre/post coverage" ok "$PRE_JSON" "$POST_JSON"
expect_codegen "missing evidence" codegen_metrics_missing "$TMP_DIR/none-pre.json" "$POST_JSON"

cat > "$TMP_DIR/codegen-shrunk.json" <<'EOF'
{"sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","symbols":6,"targets_matched":5}
EOF
expect_codegen "post-BOLT coverage shrank" codegen_coverage_post_has_fewer_symbols_and_targets "$PRE_JSON" "$TMP_DIR/codegen-shrunk.json"

cat > "$TMP_DIR/codegen-no-sha.json" <<'EOF'
{"symbols":7,"targets_matched":7}
EOF
expect_codegen "post-BOLT SHA absent" codegen_post_sha_missing "$PRE_JSON" "$TMP_DIR/codegen-no-sha.json"

cat > "$TMP_DIR/codegen-bad-sha.json" <<'EOF'
{"sha256":"not-a-hex-digest'; import os; os.system('id') #","symbols":7,"targets_matched":7}
EOF
expect_codegen "post-BOLT SHA is not a digest" codegen_post_sha_invalid "$PRE_JSON" "$TMP_DIR/codegen-bad-sha.json"

printf 'not-json' > "$TMP_DIR/codegen-invalid.json"
expect_codegen "unparseable evidence" codegen_metrics_invalid "$TMP_DIR/codegen-invalid.json" "$TMP_DIR/codegen-invalid.json"

# ── Real call sites must consume the helpers fail-closed ────────────────────
# Verify ordering structurally: the identity chain is admitted before any
# rebuild phase, and the Phase 5 reuse branch re-checks the installed bytes.
python3 - "$BUILD_RELEASE" <<'PY'
import re
import sys

text = open(sys.argv[1], "r", encoding="utf-8").read()
pre = text.index("REUSE_VERDICT=$(ceremony_certified_verdict")
phase2 = text.index("[Phase 2/7]")
phase5 = text.index("[Phase 5/7]")
assert pre < phase2 < phase5, "ceremony preflight is not before the rebuild/packaging phases"
guard = text[pre:pre + 700]
assert re.search(r'if \[ "\$REUSE_VERDICT" != "ok" \]; then\s*\n\s*die ', guard), \
    "identity verdict is not followed by a fail-closed die"
reuse5 = text[phase5:phase5 + 1400]
assert re.search(r'if ! cmp -s "\$CLAP_TARGET" "\$CERTIFIED_CLAP"; then\s*\n\s*die ', reuse5), \
    "Phase 5 reuse branch does not reject a diverged installed artifact"
PY

# Execute the real verdict guard with a coherent and with a tampered chain.
read -r GUARD_START GUARD_END < <(python3 - "$BUILD_RELEASE" <<'PY'
import sys

lines = open(sys.argv[1], "r", encoding="utf-8").readlines()
start = next(i for i, line in enumerate(lines) if "REUSE_VERDICT=$(ceremony_certified_verdict" in line)
end = next(i for i in range(start, len(lines)) if lines[i].strip() == "fi")
print(start + 1, end + 1)
PY
)
GUARD_BLOCK="$TMP_DIR/verdict-guard.sh"
sed -n "${GUARD_START},${GUARD_END}p" "$BUILD_RELEASE" > "$GUARD_BLOCK"
if ! grep -q 'die "F-CER-01' "$GUARD_BLOCK"; then
    fail "extracted identity guard does not contain the fail-closed die"
fi

run_guard() {
    local label="$1" expected_rc="$2" expected_token="$3" harness="$TMP_DIR/guard-harness.sh" rc out
    {
        echo '#!/bin/bash'
        echo 'set -euo pipefail'
        printf 'PROJECT_DIR=%q\n' "$PROJECT_DIR"
        echo 'NAM_LIB_NO_CD=1 source "$PROJECT_DIR/utils/lib/_lib.sh"'
        printf 'CERTIFIED_DIR=%q\n' "$CERT_DIR"
        printf 'CLAP_TARGET=%q\n' "$INSTALLED"
        printf 'CERTIFIED_LONG_SHA=%q\n' "$CERT_SHA"
        echo 'die() { echo "DIED: $*" >&2; exit 1; }'
        cat "$GUARD_BLOCK"
        echo 'echo GUARD_SURVIVED'
    } > "$harness"
    set +e
    out=$(bash "$harness" 2>&1)
    rc=$?
    set -e
    if [ "$rc" -ne "$expected_rc" ]; then
        fail "$label: expected rc $expected_rc, got $rc
$out"
    fi
    if ! printf '%s\n' "$out" | grep -q "$expected_token"; then
        fail "$label: missing token $expected_token
$out"
    fi
    echo -e "  ${GREEN}✓${NC} $label"
}

printf 'certified-clap-bytes' > "$CERT_DIR/nam_plug.clap"
printf '%s\n' "$CERT_SHA" > "$CERT_DIR/nam_plug.clap.sha256"
cp "$CERT_DIR/nam_plug.clap" "$INSTALLED"
run_guard "real guard accepts a coherent chain" 0 GUARD_SURVIVED

printf 'tampered-bytes' >> "$CERT_DIR/nam_plug.clap"
run_guard "real guard rejects a tampered certified copy" 1 DIED

echo -e "\n${GREEN}${BOLD}PASS: test_ceremony_reuse_binary${NC}"

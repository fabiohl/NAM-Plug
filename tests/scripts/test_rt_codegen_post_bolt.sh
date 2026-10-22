#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Regression tests for `utils/verify_rt_codegen.sh` post-link coverage:
#   1. A profile target present in the binary passes and emits JSON metrics.
#   2. A required profile target that disappears fails closed.
#   3. When a real pre/post-BOLT pair exists, post-BOLT coverage must not shrink.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
VERIFY="$PROJECT_DIR/utils/verify_rt_codegen.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[0;33m'
BOLD='\033[1m'
NC='\033[0m'

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

cd "$PROJECT_DIR"

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD} test_rt_codegen_post_bolt: bounded symbol coverage & fail-closed${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

if [ ! -x "$VERIFY" ]; then
    echo -e "${RED}FAIL: verifier not executable: $VERIFY${NC}" >&2
    exit 1
fi

TINY_BIN="$TMP_DIR/tiny_probe"
if command -v cc >/dev/null 2>&1; then
    cat > "$TMP_DIR/tiny_probe.c" <<'EOF'
int nam_probe_fn(int x) { return x + 1; }
int main(void) { return nam_probe_fn(0) == 1 ? 0 : 1; }
EOF
    cc -O2 -o "$TINY_BIN" "$TMP_DIR/tiny_probe.c"
else
    echo -e "${YELLOW}SKIP: no C compiler; using an existing plugin artifact for the positive case.${NC}"
    TINY_BIN=""
fi

if [ -z "$TINY_BIN" ]; then
    for candidate in "$PROJECT_DIR/target/release/libnam_plug.so" "$PROJECT_DIR/target/debug/libnam_plug.so"; do
        if [ -f "$candidate" ]; then
            TINY_BIN="$candidate"
            break
        fi
    done
fi

if [ -n "$TINY_BIN" ]; then
    cat > "$TMP_DIR/profile-present.json" <<'EOF'
{
  "sections": [".text", ".text.cold"],
  "targets": [
    { "id": "main", "pattern": "^main$", "required": true, "gates": { "alloc": 0, "div": 0, "tls": 0 } }
  ]
}
EOF
    echo -e "\n${BLUE}[1/4] Present required target passes and emits metrics...${NC}"
    if ! "$VERIFY" "$TINY_BIN" --profile "$TMP_DIR/profile-present.json" \
        --json-out "$TMP_DIR/present.json" > "$TMP_DIR/present.log" 2>&1; then
        cat "$TMP_DIR/present.log" >&2
        echo -e "${RED}FAIL: verifier rejected a present required target${NC}" >&2
        exit 1
    fi
    if ! grep -q '^RT_CODEGEN_METRICS: symbols=1 ' "$TMP_DIR/present.log"; then
        cat "$TMP_DIR/present.log" >&2
        echo -e "${RED}FAIL: verifier did not emit a stable RT_CODEGEN_METRICS line${NC}" >&2
        exit 1
    fi
    if ! python3 - "$TMP_DIR/present.json" <<'PY'
import json
import sys

doc = json.load(open(sys.argv[1], encoding="utf-8"))
assert doc["symbols"] == 1, doc
assert doc["targets_matched"] == 1, doc
assert doc["required_matched"] == 1, doc
assert doc["sha256"], doc
PY
    then
        echo -e "${RED}FAIL: JSON metrics are incomplete${NC}" >&2
        exit 1
    fi
    echo -e "  ${GREEN}✓ Present target verified; metrics: symbols=1 targets=1/1 required=1/1${NC}"
else
    echo -e "\n${YELLOW}[1/4] SKIP: no binary available for the positive case.${NC}"
fi

echo -e "\n${BLUE}[2/4] Missing required target fails closed...${NC}"
cat > "$TMP_DIR/profile-missing.json" <<'EOF'
{
  "sections": [".text", ".text.cold"],
  "targets": [
    { "id": "nam_absent_symbol", "pattern": "^nam_absent_symbol$", "required": true, "gates": { "alloc": 0 } }
  ]
}
EOF
MISSING_BIN="${TINY_BIN:-/bin/true}"
set +e
"$VERIFY" "$MISSING_BIN" --profile "$TMP_DIR/profile-missing.json" \
    --json-out "$TMP_DIR/missing.json" > "$TMP_DIR/missing.log" 2>&1
missing_rc=$?
set -e
if [ "$missing_rc" -eq 0 ]; then
    cat "$TMP_DIR/missing.log" >&2
    echo -e "${RED}FAIL: verifier accepted a required target that is absent${NC}" >&2
    exit 1
fi
if ! grep -q "nam_absent_symbol" "$TMP_DIR/missing.log"; then
    cat "$TMP_DIR/missing.log" >&2
    echo -e "${RED}FAIL: failure did not name the missing required target${NC}" >&2
    exit 1
fi
echo -e "  ${GREEN}✓ Missing required target failed closed (exit $missing_rc)${NC}"

echo -e "\n${BLUE}[3/4] Real pre/post-BOLT coverage does not shrink...${NC}"
PRE="$PROJECT_DIR/target/pgo-clap/dist/libnam_plug.so"
POST="$PROJECT_DIR/target/pgo-clap/dist/libnam_plug.bolt.so"
if [ -f "$PRE" ] && [ -f "$POST" ]; then
    "$VERIFY" "$PRE" --json-out "$TMP_DIR/pre.json" > "$TMP_DIR/pre.log" 2>&1
    "$VERIFY" "$POST" --json-out "$TMP_DIR/post.json" > "$TMP_DIR/post.log" 2>&1
    if ! python3 - "$TMP_DIR/pre.json" "$TMP_DIR/post.json" <<'PY'
import json
import sys

pre = json.load(open(sys.argv[1], encoding="utf-8"))
post = json.load(open(sys.argv[2], encoding="utf-8"))
assert post["symbols"] >= pre["symbols"], (pre, post)
assert post["targets_matched"] >= pre["targets_matched"], (pre, post)
assert post["required_matched"] == post["required_total"], (pre, post)
PY
    then
        echo -e "${RED}FAIL: post-BOLT coverage is smaller than pre-BOLT coverage${NC}" >&2
        cat "$TMP_DIR/pre.log" "$TMP_DIR/post.log" >&2
        exit 1
    fi
    echo -e "  ${GREEN}✓ Coverage preserved: pre $(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print("%d symbols/%d targets" % (d["symbols"], d["targets_matched"]))' "$TMP_DIR/pre.json"), post $(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print("%d symbols/%d targets" % (d["symbols"], d["targets_matched"]))' "$TMP_DIR/post.json")${NC}"
else
    echo -e "  ${YELLOW}SKIP: bolt_not_applied (pre/post-BOLT artifacts not present under target/pgo-clap/dist/).${NC}"
fi

echo -e "\n${BLUE}[4/4] Required target removed from a simulated .bolt.so fails closed...${NC}"
if [ -n "${TINY_BIN:-}" ] && command -v cc >/dev/null 2>&1; then
    TINY_BOLT="$TMP_DIR/libnam_plug.bolt.so"
    cat > "$TMP_DIR/tiny_probe_post_bolt.c" <<'EOF'
int main(void) { return 0; }
EOF
    cc -O2 -o "$TINY_BOLT" "$TMP_DIR/tiny_probe_post_bolt.c"
    cat > "$TMP_DIR/profile-required-fn.json" <<'EOF'
{
  "sections": [".text", ".text.cold"],
  "targets": [
    { "id": "nam_probe_fn", "pattern": "^nam_probe_fn$", "required": true, "gates": { "alloc": 0 } }
  ]
}
EOF
    "$VERIFY" "$TINY_BIN" --profile "$TMP_DIR/profile-required-fn.json" \
        --json-out "$TMP_DIR/pre-synthetic.json" > "$TMP_DIR/pre-synthetic.log" 2>&1
    set +e
    "$VERIFY" "$TINY_BOLT" --profile "$TMP_DIR/profile-required-fn.json" \
        --json-out "$TMP_DIR/post-synthetic.json" > "$TMP_DIR/post-synthetic.log" 2>&1
    synthetic_rc=$?
    set -e
    if [ "$synthetic_rc" -eq 0 ]; then
        cat "$TMP_DIR/post-synthetic.log" >&2
        echo -e "${RED}FAIL: shipped-binary verifier accepted a required target removed from the simulated .bolt.so${NC}" >&2
        exit 1
    fi
    if ! grep -q "nam_probe_fn" "$TMP_DIR/post-synthetic.log"; then
        cat "$TMP_DIR/post-synthetic.log" >&2
        echo -e "${RED}FAIL: synthetic post-BOLT failure did not name the missing required target${NC}" >&2
        exit 1
    fi
    echo -e "  ${GREEN}✓ Required target present pre-BOLT and removed post-BOLT failed closed (exit $synthetic_rc)${NC}"
else
    echo -e "  ${YELLOW}SKIP: no C compiler for the simulated .bolt.so case.${NC}"
fi

echo -e "\n${GREEN}${BOLD}test_rt_codegen_post_bolt: PASS${NC}"

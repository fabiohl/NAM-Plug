#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Standard quality control and static analysis script for NAM-Plug.
# Runs all cargo checks first (fmt, check, clippy) covering the maximum
# feature spectrum dynamically, followed by static textual checks.
#
# Dynamic feature matrix (broad and resilient to Cargo.toml changes):
#   All Features (catch-all) : --all-targets --all-features
#   Base CDylib              : --lib --no-default-features
#   No Default Features      : --all-targets --no-default-features

set -euo pipefail

PHASE_TOTAL=5
source "$(dirname "$0")/_lib.sh"

echo -e "${BLUE}${BOLD}========================================${NC}"
echo -e "${BLUE}${BOLD}    NAM-Plug Linting & Quality Suite    ${NC}"
echo -e "${BLUE}${BOLD}========================================${NC}"

# ---------------------------------------------------------------------------
# [1/5] Code formatting (cargo fmt)
# ---------------------------------------------------------------------------
phase "Applying code formatting (cargo fmt)..."
cargo fmt --all

# ---------------------------------------------------------------------------
# [2/5] Compilation checks (cargo check) — broad feature matrix
# ---------------------------------------------------------------------------
phase "Executing compilation checks (cargo check)..."

echo -e "  ${YELLOW}${BOLD}Checking: All Targets + All Features (broad catch-all)...${NC}"
cargo check --all-targets --all-features

echo -e "  ${YELLOW}${BOLD}Checking: Base CDylib (no default features)...${NC}"
cargo check --lib --no-default-features

echo -e "  ${YELLOW}${BOLD}Checking: All Targets (no default features)...${NC}"
cargo check --all-targets --no-default-features

# ---------------------------------------------------------------------------
# [3/5] Static analysis (cargo clippy) — strict, broad feature matrix
# ---------------------------------------------------------------------------
phase "Executing strict static analysis (cargo clippy)..."

echo -e "  ${YELLOW}${BOLD}Clippy: All Targets + All Features (broad catch-all)...${NC}"
cargo clippy --all-targets --all-features -- -D warnings

echo -e "  ${YELLOW}${BOLD}Clippy: Base CDylib (no default features)...${NC}"
cargo clippy --lib --no-default-features -- -D warnings

echo -e "  ${YELLOW}${BOLD}Clippy: All Targets (no default features)...${NC}"
cargo clippy --all-targets --no-default-features -- -D warnings

# ---------------------------------------------------------------------------
# [4/5] SPDX license header validation (deterministic, no external tooling)
# ---------------------------------------------------------------------------
phase "Validating SPDX license headers..."
spdx_scope=$(
    {
        find src $([ -d benches ] && echo benches) $([ -d tests ] && echo tests) -type f -name '*.rs'
        find utils -maxdepth 1 -type f -name '*.sh'
        test -f build.rs && echo build.rs
        test -f Cargo.toml && echo Cargo.toml
    } || true
)
missing=$(printf '%s\n' "$spdx_scope" | xargs grep -L "SPDX-License-Identifier" 2>/dev/null || true)
if [ -n "$missing" ]; then
    echo -e "  ${RED}${BOLD}Missing SPDX header in files:${NC}"
    echo "$missing" | sed 's/^/    /'
    exit 1
fi
invalid=$(printf '%s\n' "$spdx_scope" \
    | xargs grep -l "SPDX-License-Identifier" 2>/dev/null \
    | xargs grep -LE "SPDX-License-Identifier: (GPL-3\.0-or-later|MIT)" 2>/dev/null || true)
if [ -n "$invalid" ]; then
    echo -e "  ${RED}${BOLD}Invalid SPDX identifier (expected GPL-3.0-or-later or MIT):${NC}"
    echo "$invalid" | sed 's/^/    /'
    exit 1
fi
echo -e "  ${GREEN}OK${NC} — all files have valid SPDX headers (GPL-3.0-or-later, MIT)."

# ---------------------------------------------------------------------------
# [5/5] Anti-pattern check: #[test] in tests/common/
# ---------------------------------------------------------------------------
phase "Checking anti-pattern #[test] in tests/common/..."
if [ -d "tests/common" ] && grep -rnF "#[test]" tests/common/ >/dev/null 2>&1; then
    echo -e "  ${RED}${BOLD}ERROR: '#[test]' found in tests/common/ (redundant executions):${NC}"
    grep -rnF "#[test]" tests/common/ | sed 's/^/    /'
    exit 1
fi
echo -e "  ${GREEN}OK${NC} — no '#[test]' in tests/common/."

echo -e "${GREEN}${BOLD}========================================${NC}"
echo -e "${GREEN}${BOLD} Quality suite completed successfully!  ${NC}"
echo -e "${GREEN}${BOLD}========================================${NC}"

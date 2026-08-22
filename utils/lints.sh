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

PHASE_TOTAL=7
source "$(dirname "$0")/_lib.sh"

echo -e "${BLUE}${BOLD}========================================${NC}"
echo -e "${BLUE}${BOLD}    NAM-Plug Linting & Quality Suite    ${NC}"
echo -e "${BLUE}${BOLD}========================================${NC}"

# ---------------------------------------------------------------------------
# [1/6] Code formatting (cargo fmt)
# ---------------------------------------------------------------------------
phase "Applying code formatting (cargo fmt)..."
cargo fmt --all

# ---------------------------------------------------------------------------
# [2/6] Compilation checks (cargo check) — broad feature matrix
# ---------------------------------------------------------------------------
phase "Executing compilation checks (cargo check)..."

echo -e "  ${YELLOW}${BOLD}Checking: All Targets + All Features (broad catch-all)...${NC}"
cargo check --all-targets --all-features

echo -e "  ${YELLOW}${BOLD}Checking: Base CDylib (no default features)...${NC}"
cargo check --lib --no-default-features

echo -e "  ${YELLOW}${BOLD}Checking: All Targets (no default features)...${NC}"
cargo check --all-targets --no-default-features

# ---------------------------------------------------------------------------
# [3/6] Static analysis (cargo clippy) — strict, broad feature matrix
# ---------------------------------------------------------------------------
phase "Executing strict static analysis (cargo clippy)..."

echo -e "  ${YELLOW}${BOLD}Clippy: All Targets + All Features (broad catch-all)...${NC}"
cargo clippy --all-targets --all-features -- -D warnings

echo -e "  ${YELLOW}${BOLD}Clippy: Base CDylib (no default features)...${NC}"
cargo clippy --lib --no-default-features -- -D warnings

echo -e "  ${YELLOW}${BOLD}Clippy: All Targets (no default features)...${NC}"
cargo clippy --all-targets --no-default-features -- -D warnings

# ---------------------------------------------------------------------------
# [4/6] SPDX license header validation (deterministic, no external tooling)
# ---------------------------------------------------------------------------
phase "Validating SPDX license headers..."

# Build the list of directories to search as an array to avoid fragile word
# splitting inside command substitution when benches/ is absent.
rs_dirs=( src tests )
[ -d benches ] && rs_dirs+=( benches )

spdx_scope=$(
    {
        find "${rs_dirs[@]}" -type f -name '*.rs'
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
ok "All files have valid SPDX headers (GPL-3.0-or-later, MIT)."

# ---------------------------------------------------------------------------
# [5/6] Anti-pattern check: #[test] in tests/common/
# ---------------------------------------------------------------------------
phase "Checking anti-pattern #[test] in tests/common/..."
if [ -d "tests/common" ] && grep -rnF "#[test]" tests/common/ > /dev/null 2>&1; then
    echo -e "  ${RED}${BOLD}ERROR: '#[test]' found in tests/common/ (redundant executions):${NC}"
    grep -rnF "#[test]" tests/common/ | sed 's/^/    /'
    exit 1
fi
ok "No '#[test]' in tests/common/."

# ---------------------------------------------------------------------------
# [6/6] Undocumented #[allow(clippy::)] check (enforce allow_attributes policy)
#
# The project sets `allow_attributes = "warn"` in [lints.clippy], meaning every
# #[allow(clippy::...)] must carry a justification comment immediately above it
# (using the standard `// REASON:` or `// #[allow]` comment convention).
# A bare #[allow(clippy::...)] with no preceding comment is flagged here as a
# policy violation to keep lint suppressions auditable.
# ---------------------------------------------------------------------------
phase "Checking for undocumented #[allow(clippy::)] suppressions..."

undocumented_allows=""
while IFS= read -r rs_file; do
    # Read the file line by line, tracking whether the previous non-blank line
    # was a comment. Flag any #[allow(clippy:: line whose preceding non-blank
    # line is not a comment (// or #).
    prev_was_comment=false
    while IFS= read -r line; do
        trimmed="${line#"${line%%[! ]*}"}"   # lstrip whitespace
        if [[ "$trimmed" =~ ^\#\[allow\(clippy:: ]]; then
            if ! $prev_was_comment; then
                undocumented_allows+="$rs_file: $trimmed"$'\n'
            fi
            prev_was_comment=false
        elif [[ "$trimmed" =~ ^//|^# ]]; then
            prev_was_comment=true
        elif [ -n "$trimmed" ]; then
            prev_was_comment=false
        fi
        # blank lines do not reset the comment flag (allow blank separator between
        # comment and attribute)
    done < "$rs_file"
done < <(printf '%s\n' "$spdx_scope" | grep '\.rs$')

if [ -n "$undocumented_allows" ]; then
    echo -e "  ${RED}${BOLD}ERROR: Undocumented #[allow(clippy::)] found (add a justification comment above):${NC}"
    echo "$undocumented_allows" | sed 's/^/    /'
    exit 1
fi
ok "All #[allow(clippy::)] suppressions are documented."

# ---------------------------------------------------------------------------
# [7/7] Binary scan: zero EVEX/ZMM and zero AVX-512 symbols in default release
# ---------------------------------------------------------------------------
phase "Validating CLAP binary artifact (zero AVX-512 in default release build)..."
"$(dirname "$0")/verify_no_avx512_release.sh"
ok "CLAP binary artifact is clean of AVX-512 symbols and EVEX instructions."

echo -e "${GREEN}${BOLD}========================================${NC}"
echo -e "${GREEN}${BOLD} Quality suite completed successfully!  ${NC}"
echo -e "${GREEN}${BOLD}========================================${NC}"

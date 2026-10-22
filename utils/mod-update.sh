#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Supply chain update utility for NAM-Plug.
# Updates the Rust toolchain, Cargo package indexes, and dependencies in Cargo.toml/Cargo.lock.

set -euo pipefail

PHASE_TOTAL=3
source "$(dirname "$0")/lib/_lib.sh"

echo -e "${BLUE}${BOLD}================================================================${NC}"
echo -e "${BLUE}${BOLD}           NAM-Plug Supply Chain Update Pipeline                ${NC}"
echo -e "${BLUE}${BOLD}================================================================${NC}"

# Every stage tracks a typed status (UPDATED, SKIPPED_TOOL_MISSING or FAILED)
# so a partial/failed update can never be falsely declared as fully updated.
STATUS_RUSTUP="PENDING"
STATUS_CARGO_UPGRADE="PENDING"
STATUS_CARGO_UPDATE="PENDING"

# 1. Update Rust Toolchain
phase "Updating active Rust toolchain (rustup)..."
if command -v rustup &>/dev/null; then
    if rustup update; then
        STATUS_RUSTUP="UPDATED"
        ok "rustup toolchain updated."
    else
        STATUS_RUSTUP="FAILED"
        echo -e "${RED}${BOLD}Error: rustup update failed.${NC}" >&2
    fi
else
    STATUS_RUSTUP="SKIPPED_TOOL_MISSING"
    echo -e "${YELLOW}Notice: rustup not found. Skipping toolchain update.${NC}"
fi

# 2. Upgrade dependencies in Cargo.toml
phase "Updating dependency specifications (Cargo.toml)..."
if cargo --list | grep -q "upgrade"; then
    if cargo upgrade --verbose; then
        STATUS_CARGO_UPGRADE="UPDATED"
        ok "Cargo.toml updated via cargo-upgrade."
    else
        STATUS_CARGO_UPGRADE="FAILED"
        echo -e "${RED}${BOLD}Error: cargo upgrade failed.${NC}" >&2
    fi
else
    STATUS_CARGO_UPGRADE="SKIPPED_TOOL_MISSING"
    echo -e "${YELLOW}Notice: cargo-edit (cargo-upgrade) not found.${NC}"
    echo -e "${YELLOW}Install with: cargo install cargo-edit${NC}"
fi

# 3. Update Cargo.lock
phase "Updating resolved versions in Cargo.lock..."
LOCK_SNAPSHOT=""
if [ -f Cargo.lock ]; then
    LOCK_SNAPSHOT="$(mktemp -t nam-plug-cargo-lock.XXXXXX)"
    cp Cargo.lock "$LOCK_SNAPSHOT"
fi
if cargo update --verbose; then
    STATUS_CARGO_UPDATE="UPDATED"
    ok "Cargo.lock updated."
else
    STATUS_CARGO_UPDATE="FAILED"
    echo -e "${RED}${BOLD}Error: cargo update failed.${NC}" >&2
fi

# Record exactly which packages changed in the lockfile. A rewritten lock is a
# deliberate dependency change: the release receipt must not report an
# anonymous `git_dirty` with no trace of what moved.
DEP_LOCK_CHANGED=0
if [ -n "$LOCK_SNAPSHOT" ] && [ -f "$LOCK_SNAPSHOT" ]; then
    mkdir -p target/logs
    if python3 - "$LOCK_SNAPSHOT" Cargo.lock target/logs/dep-lock-change.txt <<'PY'
import sys
import tomllib

old_path, new_path, out_path = sys.argv[1:4]
try:
    with open(old_path, "rb") as handle:
        old = tomllib.load(handle)
    with open(new_path, "rb") as handle:
        new = tomllib.load(handle)
except (OSError, tomllib.TOMLDecodeError) as exc:
    print(f"cannot compare Cargo.lock snapshots: {exc}", file=sys.stderr)
    sys.exit(2)

def versions(lock):
    return {
        pkg.get("name", "?"): pkg.get("version", "?")
        for pkg in lock.get("package", [])
    }

old_versions = versions(old)
new_versions = versions(new)
lines = []
for name in sorted(set(old_versions) | set(new_versions)):
    before = old_versions.get(name)
    after = new_versions.get(name)
    if before != after:
        lines.append(f"{name}: {before or '-'} -> {after or '-'}")
with open(out_path, "w", encoding="utf-8") as handle:
    handle.write("\n".join(lines))
    if lines:
        handle.write("\n")
print(len(lines))
PY
    then
        DEP_LOCK_CHANGED=$(python3 - <<'PY'
from pathlib import Path
lines = Path('target/logs/dep-lock-change.txt').read_text(encoding='utf-8').splitlines()
print(sum(1 for line in lines if '->' in line))
PY
)
        if [ "$DEP_LOCK_CHANGED" -gt 0 ]; then
            warn "DEP_LOCK_CHANGED: ${DEP_LOCK_CHANGED} pacotes — see target/logs/dep-lock-change.txt; commit Cargo.lock before certification or the receipt will be INCOMPLETE (git_dirty:Cargo.lock)."
        else
            rm -f target/logs/dep-lock-change.txt
        fi
    else
        warn "Could not compare Cargo.lock snapshots; dependency changes are not itemized."
    fi
    rm -f "$LOCK_SNAPSHOT"
fi

# ---------------------------------------------------------------------------
# Typed summary + fail-closed exit policy
# ---------------------------------------------------------------------------
echo ""
echo -e "${BLUE}${BOLD}Supply chain update summary:${NC}"
printf '  %-22s %s\n' "Stage" "Status"
printf '  %-22s %s\n' "-----" "------"
printf '  %-22s %s\n' "rustup" "$STATUS_RUSTUP"
printf '  %-22s %s\n' "cargo-upgrade" "$STATUS_CARGO_UPGRADE"
printf '  %-22s %s\n' "cargo-update" "$STATUS_CARGO_UPDATE"

if [ "$STATUS_RUSTUP" = "FAILED" ] \
    || [ "$STATUS_CARGO_UPGRADE" = "FAILED" ] \
    || [ "$STATUS_CARGO_UPDATE" = "FAILED" ]; then
    echo -e "${RED}${BOLD}================================================================${NC}"
    echo -e "${RED}${BOLD}       Supply chain update failed (see summary above)           ${NC}"
    echo -e "${RED}${BOLD}================================================================${NC}"
    exit 1
fi

if [ "$STATUS_RUSTUP" != "UPDATED" ] \
    || [ "$STATUS_CARGO_UPGRADE" != "UPDATED" ] \
    || [ "$STATUS_CARGO_UPDATE" != "UPDATED" ]; then
    echo -e "${YELLOW}${BOLD}================================================================${NC}"
    echo -e "${YELLOW}${BOLD}  Supply chain partially updated (some steps were skipped)      ${NC}"
    echo -e "${YELLOW}${BOLD}================================================================${NC}"
    exit 0
fi

echo -e "${GREEN}${BOLD}================================================================${NC}"
echo -e "${GREEN}${BOLD}          Entire supply chain was successfully updated!         ${NC}"
echo -e "${GREEN}${BOLD}================================================================${NC}"

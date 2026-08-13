#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Supply chain update utility for NAM-Plug.
# Updates the Rust toolchain, Cargo package indexes, and dependencies in Cargo.toml/Cargo.lock.

set -euo pipefail

PHASE_TOTAL=3
source "$(dirname "$0")/_lib.sh"

echo -e "${BLUE}${BOLD}================================================================${NC}"
echo -e "${BLUE}${BOLD}           NAM-Plug Supply Chain Update Pipeline                ${NC}"
echo -e "${BLUE}${BOLD}================================================================${NC}"

# 1. Update Rust Toolchain
phase "Updating active Rust toolchain (rustup)..."
if command -v rustup &>/dev/null; then
    rustup update
else
    warn "rustup not found. Skipping toolchain update."
fi

# 2. Upgrade dependencies in Cargo.toml
phase "Upgrading dependency definitions (Cargo.toml)..."
if cargo --list | grep -q "upgrade"; then
    cargo upgrade --verbose
else
    warn "cargo-edit (cargo-upgrade) not found."
    warn "Install with: cargo install cargo-edit"
fi

# 3. Update Cargo.lock
phase "Updating resolved versions in Cargo.lock..."
cargo update --verbose

echo -e "${GREEN}${BOLD}================================================================${NC}"
echo -e "${GREEN}${BOLD}          Supply chain updated successfully!                    ${NC}"
echo -e "${GREEN}${BOLD}================================================================${NC}"

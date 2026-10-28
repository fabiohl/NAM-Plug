#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# mod-update.sh — Utilitário de atualização de dependências. Classe D.
# Uso: utils/mod-update.sh
# Orçamento: 60
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

[ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ] && { echo "Uso: utils/mod-update.sh"; exit 0; }

# 1. Update Rust Toolchain
echo "Updating active Rust toolchain (rustup)..."
if command -v rustup &>/dev/null; then
    rustup update
else
    echo "Warning: rustup not found. Skipping toolchain update."
fi

# 2. Upgrade dependencies in Cargo.toml
echo "Upgrading dependency definitions (Cargo.toml)..."
if cargo --list | grep -q "upgrade"; then
    cargo upgrade --verbose
else
    echo "Warning: cargo-edit (cargo upgrade) not found."
    echo "Install with: cargo install cargo-edit"
fi

# 3. Update Cargo.lock
echo "Updating resolved versions in Cargo.lock..."
cargo update --verbose

ok "supply chain update complete"

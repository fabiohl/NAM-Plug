#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# mod-update.sh — Utilitário de atualização de dependências. Classe D.
# Uso: utils/mod-update.sh [-h|--help]
# Orçamento: 60
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

case "${1:-}" in "") ;; -h|--help) echo "Uso: utils/mod-update.sh"; exit 0 ;; *) usage_fail "mod-update.sh aceita apenas -h/--help" ;; esac
lib_init
trap 'trap_fail $?' ERR

# 1. Update Rust Toolchain
PHASE_NAME=rustup; phase rustup
if command -v rustup &>/dev/null; then
    rustup update || FAILS+=(rustup)
else
    warn "rustup not found. Skipping toolchain update."; gap "rustup-missing"
fi

# 2. Upgrade dependencies in Cargo.toml
PHASE_NAME=cargo-upgrade; phase cargo-upgrade
if command -v cargo-upgrade >/dev/null 2>&1; then
    cargo upgrade --verbose || FAILS+=(cargo-upgrade)
else
    warn "cargo-edit (cargo upgrade) not found. Install with: cargo install cargo-edit"; gap "cargo-upgrade-missing"
fi

# 3. Update Cargo.lock
PHASE_NAME=cargo-update; phase cargo-update
cargo update --verbose || FAILS+=(cargo-update)

ok "supply chain update complete"
finish || exit 1

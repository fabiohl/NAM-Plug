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
phase "Atualizando a toolchain ativa do Rust (rustup)..."
if command -v rustup &>/dev/null; then
    rustup update
else
    echo -e "${YELLOW}Aviso: rustup não encontrado. Pulando atualização da toolchain.${NC}"
fi

# 2. Upgrade dependencies in Cargo.toml
phase "Atualizando definições de dependências (Cargo.toml)..."
if cargo --list | grep -q "upgrade"; then
    cargo upgrade --verbose
else
    echo -e "${YELLOW}Aviso: cargo-edit (cargo-upgrade) não encontrado.${NC}"
    echo -e "${YELLOW}Instale com: cargo install cargo-edit${NC}"
fi

# 3. Update Cargo.lock
phase "Atualizando versões resolvidas no Cargo.lock..."
cargo update --verbose

echo -e "${GREEN}${BOLD}================================================================${NC}"
echo -e "${GREEN}${BOLD}          Toda a cadeia de suprimentos foi atualizada!          ${NC}"
echo -e "${GREEN}${BOLD}================================================================${NC}"

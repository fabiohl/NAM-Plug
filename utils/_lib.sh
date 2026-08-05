# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

# _lib.sh — Common bash utilities for NAM-Plug scripts.
#
# Source with:
#   PHASE_TOTAL=<N>; source "$(dirname "$0")/_lib.sh"
# or for scripts not in utils/:
#   PHASE_TOTAL=<N>; source "$PROJECT_ROOT/utils/_lib.sh"
#
# Then call:
#   phase "Description of the current step"

# ANSI style helpers
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

PHASE_NUM=0

phase() {
    PHASE_NUM=$((PHASE_NUM + 1))
    echo -e "\n${BLUE}${BOLD}[${PHASE_NUM}/${PHASE_TOTAL:-?}]${NC} $*"
}

# Resolve project root dynamically relative to this helper script
LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$LIB_DIR")"

# Automatically enter the project root directory
cd "$PROJECT_DIR"




# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# rt_alloc_scan.awk — fail-closed static scanner for RT-path allocations.
#
# Usage: awk -f utils/lib/rt_alloc_scan.awk <file.rs>...
#
# Flags any RT-hostile allocation pattern found on the real-time processing
# path of NAM-Plug's `src/clap/processor/`, excluding the documented off-RT
# sites:
#   - test-only modules (`#[cfg(test)]` blocks); sibling `*_test.rs` files are
#     excluded by the caller (repo convention: test modules included via
#     `#[cfg(test)] #[path = "..."] mod`).
#   - the off-RT lifecycle/helper functions in `processor/mod.rs`
#     (`buffer_prealloc_error`, `panic_to_error`, `activate`, `deactivate`,
#     `build_cab_sim_from_raw_samples`). `activate` is the documented ONLY
#     allocation site (main thread, CLAP lifecycle); the panic helper only
#     runs during exceptional unwinding (its `Box::leak` is the sole
#     sanctioned leak).
#   - `Arc::clone` is a refcount bump, never an allocation — not flagged.
#
# Exit status: 0 when the scanned code is clean, 1 when any pattern matches
# (fail-closed). This is a structural guard that complements the dynamic
# heap-audit CI lane (`--features heap-audit`); it is intentionally
# conservative: any new heap-allocation API on the audio thread must be
# added to the pattern list below.

BEGIN {
    # RT-hostile allocation patterns (POSIX ERE). Keep sorted by severity;
    # only the first match per line is reported.
    patterns[1]  = "Box::new"
    patterns[2]  = "Box::leak"
    patterns[3]  = "Box::into_raw"
    patterns[4]  = "into_boxed_(str|slice)"
    patterns[5]  = "Vec::new"
    patterns[6]  = "Vec::with_capacity"
    patterns[7]  = "vec!"
    patterns[8]  = "with_capacity"
    patterns[9]  = "AlignedVec::new"
    patterns[10] = "format!"
    patterns[11] = "String::new"
    patterns[12] = "to_string\\("
    patterns[13] = "to_owned\\("
    patterns[14] = "Arc::new"
    patterns[15] = "Rc::new"
    patterns[16] = "HashMap"
    patterns[17] = "BTreeMap"
    patterns[18] = "HashSet"
    patterns[19] = "BTreeSet"
    patterns[20] = "VecDeque"
    patterns[21] = "collect\\(\\)"
    patterns[22] = "Box<dyn"
    N = 22
    matches = ""
}

# Returns `line` with string literals, line comments and block comments
# replaced by blanks so brace counting is not corrupted by `{`/`}` inside
# format strings, doc comments, etc.
function stripped(line,    out, i, n, c, in_str, in_line_cm, in_block_cm) {
    out = ""
    n = length(line)
    in_str = 0
    in_line_cm = 0
    in_block_cm = 0
    for (i = 1; i <= n; i++) {
        c = substr(line, i, 1)
        if (in_block_cm) {
            if (c == "*" && substr(line, i + 1, 1) == "/") {
                in_block_cm = 0
                i++
            }
            out = out " "
            continue
        }
        if (in_line_cm) {
            out = out " "
            continue
        }
        if (in_str) {
            if (c == "\\") {
                i++
                out = out "  "
                continue
            }
            if (c == "\"") in_str = 0
            out = out " "
            continue
        }
        if (c == "\"") {
            in_str = 1
            out = out " "
            continue
        }
        if (c == "/" && substr(line, i + 1, 1) == "/") {
            in_line_cm = 1
            out = out " "
            continue
        }
        if (c == "/" && substr(line, i + 1, 1) == "*") {
            in_block_cm = 1
            i++
            out = out "  "
            continue
        }
        out = out c
    }
    return out
}

# Counts `{`/`}` of a stripped line into the global `region_depth`, marking
# `seen_open` once the region's opening brace has been observed.
function depth_count(s,    i, n, c) {
    n = length(s)
    for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "{") {
            region_depth++
            seen_open = 1
        } else if (c == "}" && seen_open) {
            region_depth--
        }
    }
}

function begin_region() {
    skipping = 1
    region_depth = 0
    seen_open = 0
}

# Closes a skip region once its opening brace has been seen and the depth
# returns to zero.
function maybe_close_region() {
    if (seen_open && region_depth <= 0) skipping = 0
}

function scan(line,    i) {
    for (i = 1; i <= N; i++) {
        if (line ~ patterns[i]) {
            matches = matches sprintf("%s:%d: %s\n            %s\n", FILENAME, FNR, patterns[i], line)
            return 1
        }
    }
    return 0
}

FNR == 1 {
    skipping = 0
    pending_test = 0
    region_depth = 0
    seen_open = 0
}

{
    s = stripped($0)

    # --- #[cfg(test)] module blocks: skip until the closing brace ---
    if (skipping == 0 && pending_test == 1) {
        if ($0 ~ /^[ \t]*mod[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*(\{|[ \t]*$)/) {
            begin_region()
            depth_count(s)
            maybe_close_region()
            next
        }
        pending_test = 0
    }
    if (skipping == 0 && $0 ~ /^[ \t]*#\[cfg\(test\)\]/) {
        pending_test = 1
        next
    }

    # --- Whitelisted off-RT functions in processor/mod.rs ---
    if (skipping == 0 &&
        $0 ~ /^[ \t]*(pub(\(crate\)|\(super\))? )?fn (buffer_prealloc_error|panic_to_error|activate|deactivate|build_cab_sim_from_raw_samples)[ \t]*\(/) {
        begin_region()
        depth_count(s)
        maybe_close_region()
        next
    }

    # --- Inside a skip region: only track braces ---
    if (skipping == 1) {
        depth_count(s)
        maybe_close_region()
        next
    }

    # --- Scan-eligible line ---
    scan($0)
}

END {
    if (matches != "") {
        printf "%s", matches
        exit 1
    }
}

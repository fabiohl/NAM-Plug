// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::preset_error_cstring;

#[test]
fn preset_error_message_with_nul_is_not_discarded() {
    let converted = preset_error_cstring("bad\0preset");
    assert_eq!(
        converted.to_string_lossy(),
        "bad\u{FFFD}preset",
        "an embedded NUL must be replaced, not truncate or discard the message"
    );
}

#[test]
fn preset_error_message_without_nul_is_preserved() {
    let converted = preset_error_cstring("could not load preset");
    assert_eq!(converted.to_string_lossy(), "could not load preset");
}

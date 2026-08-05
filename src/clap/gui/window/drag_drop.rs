// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use baseview::DropData;
use std::path::PathBuf;

/// Checks whether the drag-and-drop data contains a valid NAM model file.
///
/// Accepts files with `.nam` (JSON) or `.namb` (compact binary) extension.
/// Returns the `PathBuf` of the first valid file found, or `None`.
pub(crate) fn get_valid_model_file(data: &DropData) -> Option<PathBuf> {
    if let DropData::Files(files) = data {
        for file in files {
            if let Some(ext) = file.extension().and_then(|e| e.to_str()) {
                let ext_lower = ext.to_lowercase();
                if ext_lower == "nam" || ext_lower == "namb" {
                    return Some(file.clone());
                }
            }
        }
    }
    None
}

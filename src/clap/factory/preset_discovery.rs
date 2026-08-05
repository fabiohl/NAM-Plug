// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Preset Discovery Factory: exposes `.nam`/`.namb` model files as CLAP presets.
//!
//! The host's preset browser can index model directories and show metadata
//! extracted from the model files (name, author, gear).

use clack_extensions::preset_discovery::prelude::*;
use clack_plugin::prelude::PluginError;
use neural_amp_modeler_rs::loader::nam_json::NamMetadata;
use neural_amp_modeler_rs::loader::namb::NambHeader;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The unique provider ID for NAM model presets.
const NAM_PROVIDER_ID: &str = "br.eti.fabiolima.nam-plug.nam-models";

/// File extensions for NAM models.
const NAM_FILE_EXT_NAM: &CStr = c"nam";
const NAM_FILE_EXT_NAMB: &CStr = c"namb";

/// Default search directory relative to user home.
const DEFAULT_MODEL_DIR: &str = ".nam/models";

/// Returns the provider descriptor (singleton).
fn provider_descriptor() -> &'static ProviderDescriptor {
    static DESCRIPTOR: OnceLock<ProviderDescriptor> = OnceLock::new();
    DESCRIPTOR.get_or_init(|| {
        ProviderDescriptor::new(NAM_PROVIDER_ID, "NAM Models").with_vendor("Fabio Lima")
    })
}

/// Preset discovery factory for NAM model files (`.nam`/`.namb`).
///
/// Allows hosts to discover and index NAM model files as presets in the host's preset browser.
pub struct NamPresetDiscoveryFactory;

impl PresetDiscoveryFactoryImpl for NamPresetDiscoveryFactory {
    fn provider_count(&self) -> u32 {
        1
    }

    fn provider_descriptor(&self, index: u32) -> Option<&ProviderDescriptor> {
        match index {
            0 => Some(provider_descriptor()),
            _ => None,
        }
    }

    fn create_provider<'a>(
        &'a self,
        host_info: IndexerInfo<'a>,
        provider_id: &CStr,
    ) -> Option<ProviderInstance<'a>> {
        if provider_id.to_bytes() != NAM_PROVIDER_ID.as_bytes() {
            return None;
        }

        Some(ProviderInstance::new(
            host_info,
            provider_descriptor(),
            |mut indexer| {
                // Declare file types supported by NAM.
                let _ = indexer.declare_filetype(FileType {
                    name: c"NAM Model (.nam)",
                    description: Some(c"Neural Amp Modeler JSON format"),
                    file_extension: Some(NAM_FILE_EXT_NAM),
                });
                let _ = indexer.declare_filetype(FileType {
                    name: c"NAM Binary (.namb)",
                    description: Some(c"Neural Amp Modeler binary format"),
                    file_extension: Some(NAM_FILE_EXT_NAMB),
                });

                // Declare the default model directory (~/.nam/models) if it exists.
                if let Some(home) = resolve_home_model_dir() {
                    let home_cstr = CString::new(home.to_string_lossy().as_bytes().to_vec())
                        .unwrap_or_default();
                    let location = LocationInfo {
                        name: c"NAM User Models",
                        flags: Flags::IS_FACTORY_CONTENT,
                        location: Location::File { path: &home_cstr },
                    };
                    let _ = indexer.declare_location(location);
                }

                Ok(NamPresetProvider)
            },
        ))
    }
}

/// Resolves `~/.nam/models` to an absolute path if the directory exists.
fn resolve_home_model_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = Path::new(&home).join(DEFAULT_MODEL_DIR);
    if dir.is_dir() { Some(dir) } else { None }
}

struct NamPresetProvider;

impl ProviderImpl<'_> for NamPresetProvider {
    fn get_metadata(
        &mut self,
        location: Location,
        receiver: &mut MetadataReceiver,
    ) -> Result<(), PluginError> {
        let Location::File { path } = location else {
            return Ok(());
        };

        let file_path = Path::new(
            std::str::from_utf8(path.to_bytes())
                .map_err(|_| PluginError::Message("Invalid UTF-8 in preset file path"))?,
        );

        let name = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown Model");
        let name_cstr =
            CString::new(name).map_err(|_| PluginError::Message("Invalid preset name"))?;

        let load_key_cstr =
            CString::new(file_path.to_string_lossy().as_bytes().to_vec()).unwrap_or_default();
        let load_key_cstr_ref: &CStr = &load_key_cstr;

        receiver
            .begin_preset(Some(&name_cstr), Some(load_key_cstr_ref))
            .map_err(|_| PluginError::Message("Failed to begin preset"))?;

        // Try to extract metadata from the model file.
        if let Some(meta) = extract_model_metadata(file_path) {
            if let Some(modeled_by) = meta.modeled_by.as_deref()
                && let Ok(creator) = CString::new(modeled_by)
            {
                receiver.add_creator(&creator);
            }

            if let Some(gear_model) = meta.gear_model.as_deref()
                && let Ok(gm) = CString::new(gear_model)
            {
                receiver.set_description(&gm);
            }

            if let Some(gear_make) = meta.gear_make.as_deref()
                && let Ok(gmake) = CString::new(gear_make)
            {
                receiver.add_extra_info(c"gear_make", &gmake);
            }

            if let Some(gear_type) = meta.gear_type.as_deref()
                && let Ok(gtype) = CString::new(gear_type)
            {
                receiver.add_extra_info(c"gear_type", &gtype);
            }

            if let Some(tone_type) = &meta.tone_type {
                let feature = format!("tone:{}", tone_type);
                if let Ok(fc) = CString::new(feature) {
                    receiver.add_feature(&fc);
                }
            }
        }

        Ok(())
    }
}

/// Maximum bytes read from any model file for metadata-only extraction (S6-E6-T04).
const MAX_METADATA_BYTES: u64 = 1_048_576; // 1 MiB

/// NAMB header size in bytes.
const NAMB_HEADER_SIZE: usize = 80;

/// Lightweight metadata extraction from `.nam` (JSON) and `.namb` (binary) files.
///
/// For `.nam`: reads up to `MAX_METADATA_BYTES` to find and parse the `metadata` key.
/// For `.namb`: reads the 80-byte header, extracts the JSON metadata section between
/// the header and `weights_offset`, bounded by `MAX_METADATA_BYTES`.
fn extract_model_metadata(path: &Path) -> Option<NamMetadata> {
    let ext = path.extension()?.to_str()?;
    match ext.to_lowercase().as_str() {
        "nam" => extract_nam_json_metadata(path),
        "namb" => extract_namb_metadata(path),
        _ => None,
    }
}

/// Reads up to `max_bytes` from a file. Returns empty vec if the file cannot be opened.
fn read_file_bounded(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    let read_len = file_len.min(max_bytes) as usize;
    let mut buf = Vec::with_capacity(read_len);
    file.take(max_bytes).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Parses just the `metadata` field from a `.nam` JSON file.
/// Only reads the first `MAX_METADATA_BYTES` to avoid loading weight tensors (S6-E6-T04).
fn extract_nam_json_metadata(path: &Path) -> Option<NamMetadata> {
    let bytes = read_file_bounded(path, MAX_METADATA_BYTES).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let text = std::str::from_utf8(&bytes).ok()?;

    // Locate the "metadata" key in the JSON.
    let marker = "\"metadata\"";
    let pos = text.find(marker)?;

    // Find the opening brace after the marker.
    let after_key = &text[pos + marker.len()..];
    let brace_start = after_key.find('{')?;
    let metadata_slice = &after_key[brace_start..];

    // Extract the matching JSON object by counting braces.
    let json_str = extract_balanced_json(metadata_slice)?;

    // Parse just the metadata object.
    parse_metadata(json_str)
}

/// Extracts metadata from a `.namb` binary container (S6-E6-T04).
///
/// Reads the 80-byte NAMB header, determines `weights_offset`, then parses the
/// JSON metadata section between the header and the weights block.  All I/O is
/// bounded by `MAX_METADATA_BYTES`.
fn extract_namb_metadata(path: &Path) -> Option<NamMetadata> {
    let bytes = read_file_bounded(path, MAX_METADATA_BYTES).ok()?;
    if bytes.len() < NAMB_HEADER_SIZE {
        return None;
    }

    // Parse header from raw bytes (packed struct, safe pointer cast for fixed-size slice)
    let header_slice: &[u8; NAMB_HEADER_SIZE] = bytes[..NAMB_HEADER_SIZE].try_into().ok()?;
    let header: &NambHeader = unsafe { &*(header_slice.as_ptr() as *const _) };

    // Validate magic number
    if header.magic != 0x4E414D42 {
        return None;
    }

    let weights_offset = header.weights_offset as usize;
    // The JSON metadata section is between the header and the weights block
    if weights_offset <= NAMB_HEADER_SIZE || weights_offset > bytes.len() {
        return None;
    }

    let json_bytes = &bytes[NAMB_HEADER_SIZE..weights_offset];
    let text = std::str::from_utf8(json_bytes).ok()?;
    let text = text.trim_end_matches('\0');

    // The NAMB JSON section contains a full NamModelData serialization.
    // Extract just the metadata sub-object.
    let marker = "\"metadata\"";
    let pos = text.find(marker)?;

    let after_key = &text[pos + marker.len()..];
    let brace_start = after_key.find('{')?;
    let metadata_slice = &after_key[brace_start..];
    let json_str = extract_balanced_json(metadata_slice)?;
    parse_metadata(json_str)
}

/// Parses a JSON metadata object string into `NamMetadata`.
/// Returns `None` if parsing fails or the metadata has no useful fields.
fn parse_metadata(json_str: &str) -> Option<NamMetadata> {
    let meta: NamMetadata = serde_json::from_str(json_str).ok()?;
    let has_content = meta.name.is_some()
        || meta.modeled_by.is_some()
        || meta.gear_make.is_some()
        || meta.gear_model.is_some();
    has_content.then_some(meta)
}

/// Extracts a balanced JSON object/array from the start of the string.
///
/// Uses `char_indices()` to correctly track byte positions in the presence of
/// multibyte UTF-8 characters (S6-E6-T04).
fn extract_balanced_json(s: &str) -> Option<&str> {
    let mut iter = s.char_indices();
    let (_, first) = iter.next()?;
    let (open, close) = match first {
        '{' => ('{', '}'),
        '[' => ('[', ']'),
        _ => return None,
    };

    let mut depth = 1u32;
    let mut in_string = false;
    let mut prev_was_backslash = false;

    for (end_idx, ch) in iter {
        let end_idx = end_idx + ch.len_utf8();
        if in_string {
            if prev_was_backslash {
                prev_was_backslash = false;
            } else if ch == '\\' {
                prev_was_backslash = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                '"' => in_string = true,
                c if c == open => depth += 1,
                c if c == close => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&s[..end_idx]);
                    }
                }
                _ => {}
            }
        }
    }

    None
}

#[cfg(test)]
#[path = "preset_discovery_test.rs"]
mod preset_discovery_test;

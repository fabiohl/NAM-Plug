// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Preset Discovery Factory: exposes `.nam`/`.namb` model files as CLAP presets.
//!
//! The host's preset browser can index model directories and show metadata
//! extracted from the model files (name, author, gear). Extraction itself is
//! delegated to the engine's host-neutral `loader::peek_metadata`, which streams
//! past the weight tensors without materializing them.

use clack_extensions::preset_discovery::prelude::*;
use clack_plugin::prelude::PluginError;
use neural_amp_modeler_rs::loader::nam_json::NamMetadata;
use neural_amp_modeler_rs::loader::{DEFAULT_METADATA_PEEK_BYTES, peek_metadata};
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

/// Reads the metadata of a `.nam`/`.namb` model file, if it carries any useful
/// field.
///
/// Thin wrapper over the engine's [`peek_metadata`]: the format dispatch, the
/// streaming parse over the weight tensors and the `.namb` header handling all
/// live in the engine. Any error (unsupported extension, missing file, malformed
/// input, absent metadata) is surfaced as `None`, matching the preset browser's
/// best-effort behavior.
fn extract_model_metadata(path: &Path) -> Option<NamMetadata> {
    peek_metadata(path, DEFAULT_METADATA_PEEK_BYTES)
        .ok()
        .filter(|meta| {
            meta.name.is_some()
                || meta.modeled_by.is_some()
                || meta.gear_make.is_some()
                || meta.gear_model.is_some()
        })
}

#[cfg(test)]
#[path = "preset_discovery_test.rs"]
mod preset_discovery_test;

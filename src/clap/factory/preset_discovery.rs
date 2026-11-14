// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Preset Discovery Factory: exposes `.nam`/`.namb` model files as CLAP presets.
//!
//! The host's preset browser can index model directories and show metadata
//! extracted from the model files (name, author, gear). Extraction itself is
//! delegated to the engine's host-neutral `loader::peek_metadata`, which streams
//! past the weight tensors without materializing them.

use clack_common::utils::UniversalPluginId;
use clack_extensions::preset_discovery::prelude::*;
use clack_plugin::prelude::PluginError;
use neural_amp_modeler_rs::loader::nam_json::NamMetadata;
use neural_amp_modeler_rs::loader::{DEFAULT_METADATA_PEEK_BYTES, peek_metadata};
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The unique provider ID for NAM model presets.
const NAM_PROVIDER_ID: &str = "br.eti.fabiolima.nam-plug.nam-models";

/// The CLAP plugin ID supported by these presets.
const NAM_PLUGIN_ID: &CStr = c"br.eti.fabiolima.nam-plug";

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

/// Selects `CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN` for hosts whose preset
/// indexer does not crawl `CLAP_PRESET_DISCOVERY_LOCATION_FILE`, while
/// preserving `Location::File` for hosts that index file-system directories.
pub(crate) fn use_plugin_location(info: &IndexerInfo<'_>) -> bool {
    let vendor_is_presonus = info
        .vendor()
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("PreSonus") || v.eq_ignore_ascii_case("Fender"));
    let name_is_presonus = info
        .name()
        .to_str()
        .ok()
        .is_some_and(|n| n.contains("PreSonus") || n.contains("Fender"));
    vendor_is_presonus || name_is_presonus
}

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
                let info = indexer.info();
                let name = info.name().to_string_lossy();
                let vendor = info.vendor().map(|v| v.to_string_lossy());
                let version = info.version().map(|v| v.to_string_lossy());
                let url = info.url().map(|v| v.to_string_lossy());
                let clap_version = info.clap_version();
                let plugin_location = use_plugin_location(&info);
                log::debug!(
                    "NAM-Plug preset discovery create_provider: name={name:?}, vendor={vendor:?}, version={version:?}, url={url:?}, clap_version={clap_version:?}, use_plugin_location={plugin_location}"
                );

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
                    if plugin_location {
                        let location = LocationInfo {
                            name: c"NAM User Models",
                            flags: Flags::IS_USER_CONTENT,
                            location: Location::Plugin,
                        };
                        let _ = indexer.declare_location(location);
                    } else if let Ok(home_cstr) =
                        CString::new(home.to_string_lossy().as_bytes().to_vec())
                    {
                        let location = LocationInfo {
                            name: c"NAM User Models",
                            flags: Flags::IS_USER_CONTENT,
                            location: Location::File { path: &home_cstr },
                        };
                        let _ = indexer.declare_location(location);
                    }
                }

                Ok(NamPresetProvider)
            },
        ))
    }
}

/// Resolves `~/.nam/models` to an absolute path if the directory exists.
pub(crate) fn resolve_home_model_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = Path::new(&home).join(DEFAULT_MODEL_DIR);
    if dir.is_dir() { Some(dir) } else { None }
}

/// Returns `true` if `path` has a supported NAM model extension (`.nam` or `.namb`, case-insensitive).
pub(crate) fn is_supported_model_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("nam") || ext.eq_ignore_ascii_case("namb"))
}

/// Collects regular `.nam`/`.namb` files inside `dir` in deterministic lexicographical order.
pub(crate) fn collect_sorted_model_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.is_file()
                && is_supported_model_extension(path)
                && path.to_str().is_some_and(|s| !s.contains('\0'))
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| !s.is_empty() && !s.contains('\0'))
        })
        .collect();

    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()).then_with(|| a.cmp(b)));
    files
}

/// Abstraction over CLAP `MetadataReceiver` to allow deterministic unit testing.
pub(crate) trait PresetMetadataSink {
    fn begin_preset(
        &mut self,
        name: Option<&CStr>,
        load_key: Option<&CStr>,
    ) -> Result<(), PluginError>;
    fn add_plugin_id(&mut self, plugin_id: UniversalPluginId<'_>);
    fn add_creator(&mut self, creator: &CStr);
    fn set_description(&mut self, description: &CStr);
    fn add_extra_info(&mut self, key: &CStr, value: &CStr);
    fn add_feature(&mut self, feature: &CStr);
}

impl PresetMetadataSink for MetadataReceiver<'_> {
    fn begin_preset(
        &mut self,
        name: Option<&CStr>,
        load_key: Option<&CStr>,
    ) -> Result<(), PluginError> {
        MetadataReceiver::begin_preset(self, name, load_key)
            .map(|_| ())
            .map_err(|_| PluginError::Message("Failed to begin preset"))
    }

    fn add_plugin_id(&mut self, plugin_id: UniversalPluginId<'_>) {
        MetadataReceiver::add_plugin_id(self, plugin_id);
    }

    fn add_creator(&mut self, creator: &CStr) {
        MetadataReceiver::add_creator(self, creator);
    }

    fn set_description(&mut self, description: &CStr) {
        MetadataReceiver::set_description(self, description);
    }

    fn add_extra_info(&mut self, key: &CStr, value: &CStr) {
        MetadataReceiver::add_extra_info(self, key, value);
    }

    fn add_feature(&mut self, feature: &CStr) {
        MetadataReceiver::add_feature(self, feature);
    }
}

/// Emits CLAP preset metadata for a single model file into `receiver`.
pub(crate) fn emit_preset_for_file(
    file_path: &Path,
    receiver: &mut impl PresetMetadataSink,
) -> Result<(), PluginError> {
    let name = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Unknown Model");
    let name_cstr = CString::new(name).map_err(|_| PluginError::Message("Invalid preset name"))?;

    let load_key_str = file_path
        .to_str()
        .ok_or(PluginError::Message("Invalid UTF-8 in preset file path"))?;
    let load_key_cstr = CString::new(load_key_str)
        .map_err(|_| PluginError::Message("Invalid NUL byte in preset file path"))?;

    receiver.begin_preset(Some(&name_cstr), Some(&load_key_cstr))?;
    receiver.add_plugin_id(UniversalPluginId::clap(NAM_PLUGIN_ID));

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

/// Emits CLAP preset metadata for every supported model file in `dir` (if present).
pub(crate) fn emit_plugin_location_presets_from_dir(
    dir: Option<&Path>,
    receiver: &mut impl PresetMetadataSink,
) -> Result<(), PluginError> {
    let Some(dir) = dir else {
        return Ok(());
    };
    for file_path in collect_sorted_model_files(dir) {
        emit_preset_for_file(&file_path, receiver)?;
    }
    Ok(())
}

struct NamPresetProvider;

impl ProviderImpl<'_> for NamPresetProvider {
    fn get_metadata(
        &mut self,
        location: Location,
        receiver: &mut MetadataReceiver,
    ) -> Result<(), PluginError> {
        match location {
            Location::File { path } => {
                let file_path = Path::new(
                    std::str::from_utf8(path.to_bytes())
                        .map_err(|_| PluginError::Message("Invalid UTF-8 in preset file path"))?,
                );
                emit_preset_for_file(file_path, receiver)
            }
            Location::Plugin => {
                emit_plugin_location_presets_from_dir(resolve_home_model_dir().as_deref(), receiver)
            }
        }
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

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::factory::preset_discovery::*;

    #[test]
    fn test_metadata_extraction_from_nam_file() {
        let model_path = crate::clap::test_util::model_path("lstm.nam");

        if model_path.exists() {
            let meta = extract_model_metadata(&model_path);
            assert!(meta.is_some(), "Should extract metadata from lstm.nam");
            let m = meta.unwrap();
            assert!(
                m.name.is_some() || m.modeled_by.is_some(),
                "Metadata should contain at least name or author"
            );
        }
    }

    #[test]
    fn test_extract_model_metadata_unsupported_extension() {
        let meta = extract_model_metadata(std::path::Path::new("/tmp/test.txt"));
        assert!(meta.is_none());
    }

    #[test]
    fn test_extract_model_metadata_missing_file() {
        let meta = extract_model_metadata(std::path::Path::new("/nonexistent/model.nam"));
        assert!(meta.is_none());
    }

    /// A `"metadata"` literal embedded in a string value must not be mistaken
    /// for the real metadata object (the engine's streaming peer resolves the
    /// top-level key structurally).
    #[test]
    fn test_extract_model_metadata_ignores_metadata_literal_in_string() {
        let dir = std::env::temp_dir().join(format!(
            "nam-plug-preset-discovery-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("trap.nam");
        let json = r#"{"version":"0.6.0","config":{"note":"\"metadata\":{\"name\":\"WRONG\"}"},"metadata":{"name":"RIGHT"}}"#;
        std::fs::write(&path, json).expect("write model");

        let meta = extract_model_metadata(&path).expect("metadata resolved");
        assert_eq!(meta.name.as_deref(), Some("RIGHT"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_factory_returns_one_provider() {
        let factory = NamPresetDiscoveryFactory;
        assert_eq!(factory.provider_count(), 1);
    }

    #[test]
    fn test_factory_descriptor_is_valid() {
        let factory = NamPresetDiscoveryFactory;
        let desc = factory.provider_descriptor(0).unwrap();
        assert!(desc.id().is_some());
        assert!(desc.name().is_some());
        assert_eq!(desc.id().unwrap().to_bytes(), NAM_PROVIDER_ID.as_bytes());
    }

    #[test]
    fn test_preset_discovery_plugin_id_matches_descriptor() {
        let desc = crate::clap::descriptor::nam_descriptor();
        assert_eq!(
            NAM_PLUGIN_ID.to_bytes(),
            desc.id()
                .expect("Plugin descriptor must have an id")
                .to_bytes(),
            "Preset discovery plugin ID must match the plugin descriptor ID"
        );
    }

    #[derive(Default)]
    struct RecordedPreset {
        name: Option<String>,
        load_key: Option<String>,
        plugin_ids: Vec<String>,
        creators: Vec<String>,
        description: Option<String>,
        extra_info: Vec<(String, String)>,
        features: Vec<String>,
    }

    #[derive(Default)]
    struct MockMetadataReceiver {
        presets: Vec<RecordedPreset>,
    }

    impl PresetMetadataSink for MockMetadataReceiver {
        fn begin_preset(
            &mut self,
            name: Option<&CStr>,
            load_key: Option<&CStr>,
        ) -> Result<(), PluginError> {
            self.presets.push(RecordedPreset {
                name: name.map(|s| s.to_string_lossy().into_owned()),
                load_key: load_key.map(|s| s.to_string_lossy().into_owned()),
                ..Default::default()
            });
            Ok(())
        }

        fn add_plugin_id(&mut self, plugin_id: UniversalPluginId<'_>) {
            if let Some(current) = self.presets.last_mut() {
                current
                    .plugin_ids
                    .push(plugin_id.id.to_string_lossy().into_owned());
            }
        }

        fn add_creator(&mut self, creator: &CStr) {
            if let Some(current) = self.presets.last_mut() {
                current
                    .creators
                    .push(creator.to_string_lossy().into_owned());
            }
        }

        fn set_description(&mut self, description: &CStr) {
            if let Some(current) = self.presets.last_mut() {
                current.description = Some(description.to_string_lossy().into_owned());
            }
        }

        fn add_extra_info(&mut self, key: &CStr, value: &CStr) {
            if let Some(current) = self.presets.last_mut() {
                current.extra_info.push((
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                ));
            }
        }

        fn add_feature(&mut self, feature: &CStr) {
            if let Some(current) = self.presets.last_mut() {
                current
                    .features
                    .push(feature.to_string_lossy().into_owned());
            }
        }
    }

    struct TestTempDir(std::path::PathBuf);

    impl TestTempDir {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "nam-plug-{label}-{}-{seq}-{nanos:x}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_collect_sorted_model_files_filters_and_sorts_deterministically() {
        let dir = TestTempDir::new("sorted");
        let root = dir.path();

        std::fs::write(root.join("z_last.NAMB"), b"NAMB").expect("write z_last.NAMB");
        std::fs::write(root.join("a_first.nam"), b"{}").expect("write a_first.nam");
        std::fs::write(root.join("m_mid.nam"), b"{}").expect("write m_mid.nam");
        std::fs::write(root.join("ir.wav"), b"RIFF").expect("write ir.wav");
        std::fs::write(root.join("readme.txt"), b"notes").expect("write readme.txt");
        std::fs::create_dir(root.join("nested.nam")).expect("create subdir named *.nam");

        let files = collect_sorted_model_files(root);
        let names: Vec<String> = files
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();

        assert_eq!(
            names,
            vec![
                "a_first.nam".to_string(),
                "m_mid.nam".to_string(),
                "z_last.NAMB".to_string()
            ]
        );
    }

    #[test]
    fn test_collect_sorted_model_files_missing_or_empty_dir() {
        let missing = std::path::Path::new("/nonexistent/nam_plug_missing_preset_dir");
        assert!(collect_sorted_model_files(missing).is_empty());

        let empty_dir = TestTempDir::new("empty");
        assert!(collect_sorted_model_files(empty_dir.path()).is_empty());

        let mut receiver = MockMetadataReceiver::default();
        emit_plugin_location_presets_from_dir(None, &mut receiver)
            .expect("None directory should succeed with zero presets");
        assert!(receiver.presets.is_empty());

        emit_plugin_location_presets_from_dir(Some(empty_dir.path()), &mut receiver)
            .expect("Empty directory should succeed with zero presets");
        assert!(receiver.presets.is_empty());
    }

    #[test]
    fn test_plugin_location_emits_presets_and_metadata() {
        let dir = TestTempDir::new("emit");
        let root = dir.path();

        let alpha_path = root.join("alpha.nam");
        let alpha_json = r#"{
            "version": "0.5.4",
            "metadata": {
                "name": "Alpha Amp",
                "modeled_by": "Jane Doe",
                "gear_make": "Fender",
                "gear_model": "Twin Reverb",
                "gear_type": "Amp",
                "tone_type": "Clean"
            }
        }"#;
        std::fs::write(&alpha_path, alpha_json).expect("write alpha.nam");

        let beta_path = root.join("beta.nam");
        let beta_json = r#"{
            "version": "0.5.4",
            "metadata": {
                "modeled_by": "John Smith",
                "gear_model": "5150"
            }
        }"#;
        std::fs::write(&beta_path, beta_json).expect("write beta.nam");

        let mut receiver = MockMetadataReceiver::default();
        emit_plugin_location_presets_from_dir(Some(root), &mut receiver)
            .expect("emit_plugin_location_presets_from_dir should succeed");

        assert_eq!(receiver.presets.len(), 2);

        let first = &receiver.presets[0];
        assert_eq!(first.name.as_deref(), Some("alpha"));
        assert_eq!(
            first.load_key.as_deref(),
            Some(alpha_path.to_str().unwrap())
        );
        assert_eq!(first.plugin_ids, vec!["br.eti.fabiolima.nam-plug"]);
        assert_eq!(first.creators, vec!["Jane Doe"]);
        assert_eq!(first.description.as_deref(), Some("Twin Reverb"));
        assert!(
            first
                .extra_info
                .contains(&("gear_make".to_string(), "Fender".to_string()))
        );
        assert!(
            first
                .extra_info
                .contains(&("gear_type".to_string(), "Amp".to_string()))
        );
        assert_eq!(first.features, vec!["tone:Clean"]);

        let second = &receiver.presets[1];
        assert_eq!(second.name.as_deref(), Some("beta"));
        assert_eq!(
            second.load_key.as_deref(),
            Some(beta_path.to_str().unwrap())
        );
        assert_eq!(second.plugin_ids, vec!["br.eti.fabiolima.nam-plug"]);
        assert_eq!(second.creators, vec!["John Smith"]);
        assert_eq!(second.description.as_deref(), Some("5150"));
    }

    #[test]
    fn test_create_provider_selects_location_by_indexer_info() {
        use crate::clap::entry::NamEntry;
        use clack_extensions::preset_discovery::prelude::{IndexerImpl, Provider};
        use clack_host::prelude::{HostError, HostInfo, PluginEntry};

        #[derive(Default)]
        struct RecordingIndexer {
            filetypes: usize,
            declared_is_plugin: Vec<bool>,
        }

        impl IndexerImpl for RecordingIndexer {
            fn declare_filetype(&mut self, _file_type: FileType) -> Result<(), HostError> {
                self.filetypes += 1;
                Ok(())
            }

            fn declare_location(&mut self, location: LocationInfo) -> Result<(), HostError> {
                self.declared_is_plugin
                    .push(matches!(location.location, Location::Plugin));
                Ok(())
            }

            fn declare_soundpack(&mut self, _soundpack: Soundpack) -> Result<(), HostError> {
                Ok(())
            }
        }

        let entry =
            PluginEntry::load_from_clack::<NamEntry>(c"/test").expect("load NamEntry for test");
        let provider_id = CString::new(NAM_PROVIDER_ID).unwrap();
        let has_home_models = resolve_home_model_dir().is_some();

        let presonus_host = HostInfo::new(
            "PreSonus CLAP Indexer",
            "PreSonus",
            "https://www.presonus.software",
            "5.2.0",
        )
        .expect("HostInfo for PreSonus");
        let presonus_provider = Provider::instantiate(
            RecordingIndexer::default(),
            &entry,
            &provider_id,
            &presonus_host,
        )
        .expect("instantiate provider for PreSonus");
        assert_eq!(presonus_provider.indexer().filetypes, 2);
        if has_home_models {
            assert_eq!(presonus_provider.indexer().declared_is_plugin, vec![true]);
        }

        let bitwig_host = HostInfo::new(
            "Bitwig Studio",
            "Bitwig GmbH",
            "https://www.bitwig.com",
            "6.1.7",
        )
        .expect("HostInfo for Bitwig");
        let bitwig_provider = Provider::instantiate(
            RecordingIndexer::default(),
            &entry,
            &provider_id,
            &bitwig_host,
        )
        .expect("instantiate provider for Bitwig");
        assert_eq!(bitwig_provider.indexer().filetypes, 2);
        if has_home_models {
            assert_eq!(bitwig_provider.indexer().declared_is_plugin, vec![false]);
        }
    }
}

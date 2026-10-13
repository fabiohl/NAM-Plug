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
}

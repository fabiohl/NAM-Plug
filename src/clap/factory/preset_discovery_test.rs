// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::factory::preset_discovery::*;

    #[test]
    fn test_extract_balanced_json_object() {
        let s = r#"{"name": "test", "nested": {"a": 1}}"#;
        let result = extract_balanced_json(s).unwrap();
        assert_eq!(result, s);
    }

    #[test]
    fn test_extract_balanced_json_nested_braces_in_strings() {
        let s = r#"{"data": "{\"key\": \"val\"}", "x": 1}extra"#;
        let result = extract_balanced_json(s).unwrap();
        assert_eq!(result, r#"{"data": "{\"key\": \"val\"}", "x": 1}"#);
    }

    #[test]
    fn test_extract_balanced_json_array() {
        let s = r#"[1, 2, {"a": 3}, [4, 5]]trailing"#;
        let result = extract_balanced_json(s).unwrap();
        assert_eq!(result, r#"[1, 2, {"a": 3}, [4, 5]]"#);
    }

    // ── S6-E6-T04: Unicode tests ──

    #[test]
    fn test_extract_balanced_json_with_unicode_in_strings() {
        let s = r#"{"name": "café", "modeled_by": "田中太郎"}"#;
        let result = extract_balanced_json(s).unwrap();
        assert_eq!(result, s);
    }

    #[test]
    fn test_extract_balanced_json_with_japanese_metadata() {
        let s = r#"{"name": "チューブスクリーマー", "make": "BOSS", "model": "SD-1"}"#;
        let result = extract_balanced_json(s).unwrap();
        assert_eq!(result, s);
    }

    #[test]
    fn test_extract_balanced_json_with_emoji_in_strings() {
        let s = r#"{"tone": "🔥🔥🔥", "genre": "rock 🎸"}"#;
        let result = extract_balanced_json(s).unwrap();
        assert_eq!(result, s);
    }

    #[test]
    fn test_extract_balanced_json_with_escaped_unicode() {
        let s = r#"{"name": "caf\u00e9", "x": 1}"#;
        let result = extract_balanced_json(s).unwrap();
        assert_eq!(result, r#"{"name": "caf\u00e9", "x": 1}"#);
    }

    #[test]
    fn test_extract_balanced_json_unicode_byte_index_correct() {
        // This previously would panic or produce wrong results because the
        // byte-index counter in (1usize..) would mismatch char_indices().
        // Now with char_indices() the byte index is always valid.
        let s = r#"{"名前": "value"}"#; // "名" = 3 bytes U+540D
        let result = extract_balanced_json(s).unwrap();
        assert_eq!(result, s);
    }

    // ── S6-E6-T04: Malformed/corrupted input tests ──

    #[test]
    fn test_extract_balanced_json_empty_string() {
        assert!(extract_balanced_json("").is_none());
    }

    #[test]
    fn test_extract_balanced_json_plain_value() {
        assert!(extract_balanced_json("\"hello\"").is_none());
    }

    #[test]
    fn test_extract_balanced_json_unclosed_object() {
        assert!(extract_balanced_json("{\"a\": 1").is_none());
    }

    #[test]
    fn test_extract_balanced_json_unclosed_string() {
        assert!(extract_balanced_json("{\"a\": \"unclosed}").is_none());
    }

    // ── S6-E6-T04: parse_metadata tests ──

    #[test]
    fn test_parse_metadata_with_unicode_name() {
        let json = r#"{"name": "café modèl", "modeled_by": "José García"}"#;
        let meta = parse_metadata(json).expect("should parse unicode metadata");
        assert_eq!(meta.name.as_deref(), Some("café modèl"));
        assert_eq!(meta.modeled_by.as_deref(), Some("José García"));
    }

    #[test]
    fn test_parse_metadata_empty_object() {
        // Empty metadata object (no useful fields) returns None
        let meta = parse_metadata("{}");
        assert!(meta.is_none());
    }

    #[test]
    fn test_parse_metadata_malformed_json() {
        assert!(parse_metadata("{broken").is_none());
    }

    #[test]
    fn test_parse_metadata_truncated_utf8() {
        // Incomplete UTF-8 byte sequence at the end
        let truncated = unsafe { std::str::from_utf8_unchecked(b"{\"name\": \"t\xC3\xA9") };
        assert!(extract_balanced_json(truncated).is_none());
    }

    // ── Existing integration tests ──

    #[test]
    fn test_metadata_extraction_from_nam_file() {
        let model_path =
            neural_amp_modeler_rs::testing::fixtures::model_path("BossWN-standard.nam");

        if model_path.exists() {
            let meta = extract_model_metadata(&model_path);
            assert!(
                meta.is_some(),
                "Should extract metadata from BossWN-standard.nam"
            );
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

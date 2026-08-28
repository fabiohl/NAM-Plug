// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use std::fs::File;
use std::io::Write;

fn make_model_resources(mult_adj: f32) -> ModelResources {
    ModelResources {
        model_l: None,
        new_resampler: Box::new(NamResampler::new(48000, 48000, 0).unwrap()),
        new_stream: crate::clap::plugin::build_stream_adapter(48000, 48000, 64).unwrap(),
        input_mult_adj: mult_adj,
        output_mult_adj: mult_adj,
        model_rate: 48000,
        model_metadata: NamModelMetadata::default(),
        model_info: neural_amp_modeler_rs::common::diagnostics::ModelInfo::default(),
        model_hash: "deadbeef".to_string(),
    }
}

fn make_validated(params: ProcessingParams) -> ValidatedRestore {
    ValidatedRestore {
        params,
        model: None,
        model_path_on_disk: None,
        model_basename: None,
        model_search_path_to_add: None,
        model_hash: None,
        ir: None,
        ir_path_on_disk: None,
        ir_hash: None,
    }
}

#[test]
fn test_canonical_search_dirs_structure() {
    let dirs = canonical_search_dirs();
    // Verification of return invariant: vector of PathBufs
    for dir in &dirs {
        assert!(dir.is_absolute(), "Canonical search path must be absolute");
    }
}

#[test]
fn test_compute_file_hash_known_content() {
    let temp_dir = std::env::temp_dir();
    let file_path = temp_dir.join("nam_state_tx_hash_test.tmp");
    let content = b"NeuralAmpModeler-rs-state-transaction-test-payload";

    {
        let mut file = File::create(&file_path).expect("Failed to create temporary test file");
        file.write_all(content)
            .expect("Failed to write to test file");
    }

    let hash = compute_file_hash(&file_path).expect("compute_file_hash failed");
    let _ = std::fs::remove_file(&file_path);

    assert_eq!(
        hash.len(),
        64,
        "SHA-256 hex digest must be 64 characters long"
    );

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content);
    let hash_bytes = hasher.finalize();
    let expected: String = hash_bytes.iter().map(|b| format!("{b:02x}")).collect();

    assert_eq!(
        hash, expected,
        "Computed file hash must match expected SHA-256 digest"
    );
}

#[test]
fn test_compute_file_hash_nonexistent_file() {
    let non_existent = Path::new("/tmp/nonexistent_nam_model_file_xyz_12345.nam");
    let result = compute_file_hash(non_existent);
    assert!(
        result.is_err(),
        "compute_file_hash must return error for non-existent file"
    );
}

#[test]
fn test_sanitize_basename_valid() {
    assert_eq!(sanitize_basename("CleanModel.nam"), Some("CleanModel.nam"));
    assert_eq!(
        sanitize_basename("jcm800_lead_v2.namb"),
        Some("jcm800_lead_v2.namb")
    );
}

#[test]
fn test_sanitize_basename_traversal_rejection() {
    assert_eq!(sanitize_basename("../secret.nam"), None);
    assert_eq!(sanitize_basename("../../etc/passwd"), None);
    assert_eq!(sanitize_basename("foo/../../bar.nam"), None);
    assert_eq!(sanitize_basename(".."), None);
    assert_eq!(sanitize_basename(""), None);
}

#[test]
fn test_sanitize_basename_path_separator_rejection() {
    assert_eq!(sanitize_basename("/etc/passwd"), None);
    assert_eq!(sanitize_basename("subdir/model.nam"), None);
    assert_eq!(sanitize_basename("C:\\Windows\\system32\\model.nam"), None);
    assert_eq!(sanitize_basename("subdir\\model.nam"), None);
}

#[test]
fn test_resolve_confined_candidate_within_root() {
    let temp_dir = std::env::temp_dir();
    let root = temp_dir.join("nam_confinement_test_root");
    let _ = std::fs::create_dir_all(&root);
    let model_file = root.join("valid_model.nam");
    {
        let mut f = File::create(&model_file).expect("create test file");
        let _ = f.write_all(b"test-model-content");
    }

    let resolved = resolve_confined_candidate(&root, "valid_model.nam");
    assert!(
        resolved.is_some(),
        "Valid candidate within root must resolve"
    );
    let resolved_path = resolved.unwrap();
    assert!(
        resolved_path.starts_with(root.canonicalize().unwrap()),
        "Resolved candidate must start with canonical root"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[cfg(unix)]
fn test_resolve_confined_candidate_rejects_escaping_symlink() {
    let temp_dir = std::env::temp_dir();
    let root = temp_dir.join("nam_confinement_symlink_root");
    let outside_dir = temp_dir.join("nam_confinement_outside");
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::create_dir_all(&outside_dir);

    let outside_file = outside_dir.join("secret_outside.nam");
    {
        let mut f = File::create(&outside_file).expect("create outside file");
        let _ = f.write_all(b"secret-content");
    }

    let symlink_path = root.join("symlink_to_outside.nam");
    let _ = std::os::unix::fs::symlink(&outside_file, &symlink_path);

    let resolved = resolve_confined_candidate(&root, "symlink_to_outside.nam");
    assert!(
        resolved.is_none(),
        "Symlink escaping the search root must be rejected by resolve_confined_candidate"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside_dir);
}

#[test]
fn test_validate_ir_hash_mismatch() {
    let temp_dir = std::env::temp_dir();
    let ir_file = temp_dir.join("nam_test_ir_hash_mismatch.wav");
    {
        let mut f = File::create(&ir_file).expect("create test ir");
        let _ = f.write_all(b"fake-ir-data");
    }

    let params = ProcessingParams {
        ir_path: Some(ir_file.clone()),
        ir_hash: Some(
            "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        ),
        ..Default::default()
    };

    let sys = neural_amp_modeler_rs::common::diagnostics::SystemSnapshot::capture();
    let result = validate_ir(&params, 48000, 256, &sys);
    assert!(
        result.is_err(),
        "validate_ir must fail with error when IR hash mismatches expected hash"
    );

    let _ = std::fs::remove_file(&ir_file);
}

// ── Mandatory SHA-256 asset identity validation ──────────────────────────────

fn fixture_model_path(name: &str) -> std::path::PathBuf {
    let p = crate::clap::test_util::model_path(name);
    assert!(p.exists(), "fixture {name} missing");
    p
}

fn sys_snapshot() -> neural_amp_modeler_rs::common::diagnostics::SystemSnapshot {
    neural_amp_modeler_rs::common::diagnostics::SystemSnapshot::capture()
}

#[test]
fn test_is_valid_sha256_hex() {
    assert!(
        is_valid_sha256_hex(&"a".repeat(64)),
        "lowercase hex accepted"
    );
    assert!(
        is_valid_sha256_hex(&("A1b2C3".to_string() + &"d".repeat(58))),
        "mixed-case hex accepted"
    );
    assert!(!is_valid_sha256_hex(""), "empty digest rejected");
    assert!(
        !is_valid_sha256_hex(&"a".repeat(63)),
        "short digest rejected"
    );
    assert!(
        !is_valid_sha256_hex(&"a".repeat(65)),
        "long digest rejected"
    );
    assert!(
        !is_valid_sha256_hex(&"g".repeat(64)),
        "non-hex chars rejected"
    );
    assert!(
        !is_valid_sha256_hex(&("a".repeat(32) + "-" + &"a".repeat(31))),
        "separator rejected"
    );
}

#[test]
fn test_validate_model_full_omitted_hash_rejected() {
    let model = fixture_model_path("lstm.nam");
    let params = ProcessingParams {
        model_path: Some(model),
        model_basename: Some("lstm.nam".to_string()),
        model_hash: None,
        ..Default::default()
    };
    let result = validate_model_full(&params, 48000, 256, &sys_snapshot());
    assert!(
        result.is_err(),
        "Full restore with existing path but omitted hash must be rejected"
    );
}

#[test]
fn test_validate_model_full_malformed_hash_rejected() {
    let model = fixture_model_path("lstm.nam");
    let params = ProcessingParams {
        model_path: Some(model),
        model_basename: Some("lstm.nam".to_string()),
        model_hash: Some("not-a-sha256".to_string()),
        ..Default::default()
    };
    let result = validate_model_full(&params, 48000, 256, &sys_snapshot());
    assert!(
        result.is_err(),
        "Full restore with malformed hash must be rejected"
    );
}

#[test]
fn test_validate_model_full_wrong_hash_no_fallback_match_rejected() {
    let model = fixture_model_path("lstm.nam");
    // Well-formed but wrong digest; no search dir is provided, so the basename
    // candidate cannot be resolved → the restore must be rejected.
    let params = ProcessingParams {
        model_path: Some(model),
        model_basename: Some("lstm.nam".to_string()),
        model_hash: Some(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
        ),
        ..Default::default()
    };
    let result = validate_model_full(&params, 48000, 256, &sys_snapshot());
    assert!(
        result.is_err(),
        "Full restore with wrong hash and no fallback must be rejected"
    );
}

#[test]
fn test_validate_model_full_wrong_path_falls_back_to_matching_basename_hash() {
    // Direct path is a *different* existing model whose hash diverges from the
    // expected digest; the basename fallback finds lstm.nam (whose digest
    // matches) in the search dirs → portable resolution succeeds.
    let other = fixture_model_path("wavenet_a1_standard.nam");
    let target = fixture_model_path("lstm.nam");
    let target_dir = target.parent().unwrap().to_path_buf();
    let expected_hash = compute_file_hash(&target).expect("hash target fixture");

    let params = ProcessingParams {
        model_path: Some(other),
        model_basename: Some("lstm.nam".to_string()),
        model_hash: Some(expected_hash.clone()),
        model_search_paths: vec![target_dir],
        ..Default::default()
    };
    let result = validate_model_full(&params, 48000, 256, &sys_snapshot());
    assert!(
        result.is_ok(),
        "basename+hash fallback must adopt the matching candidate"
    );
    let (resources, path_on_disk, basename, _search, hash) = result.unwrap();
    assert!(resources.is_some(), "fallback must build model resources");
    assert_eq!(basename.as_deref(), Some("lstm.nam"));
    assert_eq!(hash.as_deref(), Some(expected_hash.as_str()));
    assert!(
        path_on_disk
            .as_deref()
            .is_some_and(|p| p.ends_with("lstm.nam")),
        "adopted path must be the basename-resolved candidate"
    );
}

#[test]
fn test_validate_model_from_basename_omitted_hash_rejected() {
    let model_dir = fixture_model_path("lstm.nam")
        .parent()
        .unwrap()
        .to_path_buf();
    let params = ProcessingParams {
        model_basename: Some("lstm.nam".to_string()),
        model_hash: None,
        model_search_paths: vec![model_dir],
        ..Default::default()
    };
    let result = validate_model_from_basename(&params, 48000, 256, &sys_snapshot());
    assert!(
        result.is_err(),
        "basename search without a saved hash must be rejected"
    );
}

#[test]
fn test_validate_model_from_basename_malformed_hash_rejected() {
    let model_dir = fixture_model_path("lstm.nam")
        .parent()
        .unwrap()
        .to_path_buf();
    let params = ProcessingParams {
        model_basename: Some("lstm.nam".to_string()),
        model_hash: Some("zz".to_string()),
        model_search_paths: vec![model_dir],
        ..Default::default()
    };
    let result = validate_model_from_basename(&params, 48000, 256, &sys_snapshot());
    assert!(
        result.is_err(),
        "basename search with a malformed hash must be rejected"
    );
}

#[test]
fn test_validate_model_from_basename_valid_hash_adopts_matching_candidate() {
    let target = fixture_model_path("lstm.nam");
    let model_dir = target.parent().unwrap().to_path_buf();
    let expected_hash = compute_file_hash(&target).expect("hash target fixture");

    let params = ProcessingParams {
        model_basename: Some("lstm.nam".to_string()),
        model_hash: Some(expected_hash.clone()),
        model_search_paths: vec![model_dir],
        ..Default::default()
    };
    let result = validate_model_from_basename(&params, 48000, 256, &sys_snapshot());
    assert!(
        result.is_ok(),
        "basename search with matching hash must adopt the candidate"
    );
    let (resources, path_on_disk, basename, _search, hash) = result.unwrap();
    assert!(resources.is_some());
    assert_eq!(basename.as_deref(), Some("lstm.nam"));
    assert_eq!(hash.as_deref(), Some(expected_hash.as_str()));
    assert!(
        path_on_disk.as_deref().is_some(),
        "adopted path must be reported"
    );
}

#[test]
fn test_validate_model_from_basename_without_basename_is_no_model() {
    let params = ProcessingParams {
        model_basename: None,
        ..Default::default()
    };
    let result = validate_model_from_basename(&params, 48000, 256, &sys_snapshot());
    assert!(
        matches!(result, Ok((None, None, None, None, None))),
        "no basename ⇒ explicit no-model state (valid Full clear)"
    );
}

#[test]
fn test_validate_ir_omitted_hash_rejected() {
    let temp_dir = std::env::temp_dir();
    let ir_file = temp_dir.join("nam_test_ir_omitted_hash.wav");
    {
        let mut f = File::create(&ir_file).expect("create test ir");
        let _ = f.write_all(b"fake-ir-data");
    }

    let params = ProcessingParams {
        ir_path: Some(ir_file.clone()),
        ir_hash: None,
        ..Default::default()
    };
    let result = validate_ir(&params, 48000, 256, &sys_snapshot());
    assert!(
        result.is_err(),
        "IR without a saved hash must not load the WAV"
    );

    let _ = std::fs::remove_file(&ir_file);
}

#[test]
fn test_validate_ir_malformed_hash_rejected() {
    let temp_dir = std::env::temp_dir();
    let ir_file = temp_dir.join("nam_test_ir_malformed_hash.wav");
    {
        let mut f = File::create(&ir_file).expect("create test ir");
        let _ = f.write_all(b"fake-ir-data");
    }

    let params = ProcessingParams {
        ir_path: Some(ir_file.clone()),
        ir_hash: Some("malformed".to_string()),
        ..Default::default()
    };
    let result = validate_ir(&params, 48000, 256, &sys_snapshot());
    assert!(result.is_err(), "IR with a malformed hash must be rejected");

    let _ = std::fs::remove_file(&ir_file);
}

#[test]
fn test_validate_ir_valid_hash_accepts_real_wav() {
    let ir_file = std::env::temp_dir().join("nam_test_ir_valid_hash.wav");
    let samples: Vec<f32> = (0..64).map(|i| (i as f32 * 0.1).sin()).collect();
    neural_amp_modeler_rs::testing::wav::write_wav_f32(&ir_file, &samples, 48000)
        .expect("failed to write synthetic IR WAV");
    let expected_hash = compute_file_hash(&ir_file).expect("hash IR");

    let params = ProcessingParams {
        ir_path: Some(ir_file.clone()),
        ir_hash: Some(expected_hash.clone()),
        ..Default::default()
    };
    let result = validate_ir(&params, 48000, 256, &sys_snapshot());
    assert!(result.is_ok(), "IR with matching hash must load");
    let (resources, path_on_disk, hash) = result.unwrap();
    assert!(resources.is_some(), "IR resources must be built");
    assert!(path_on_disk.is_some());
    assert_eq!(hash.as_deref(), Some(expected_hash.as_str()));

    let _ = std::fs::remove_file(&ir_file);
}

#[test]
fn test_validate_model_full_no_model_path_without_basename_is_clear() {
    // Full restore with neither path nor basename = explicit no-model clear.
    let params = ProcessingParams::default();
    let result = validate_model_full(&params, 48000, 256, &sys_snapshot());
    assert!(
        matches!(result, Ok((None, None, None, None, None))),
        "Full without any model reference must be a valid clear"
    );
}

// ── Transactional restore package mapping ──────────────────────────────────

#[test]
fn test_next_restore_generation_monotonic() {
    let a = next_restore_generation();
    let b = next_restore_generation();
    assert!(b > a, "restore generations must be strictly monotonic");
}

#[test]
fn test_build_restore_package_full_with_model() {
    let mut validated = make_validated(ProcessingParams {
        input_gain_db: 3.0,
        output_gain_db: -6.0,
        model_path: Some(PathBuf::from("/tmp/a.nam")),
        model_basename: Some("a.nam".to_string()),
        model_hash: Some("deadbeef".to_string()),
        ..Default::default()
    });
    validated.model = Some(make_model_resources(1.0));
    validated.model_path_on_disk = Some(PathBuf::from("/tmp/a.nam"));
    validated.model_basename = Some("a.nam".to_string());
    validated.model_search_path_to_add = Some(PathBuf::from("/tmp"));
    validated.model_hash = Some("deadbeef".to_string());

    let shared = crate::clap::plugin::make_test_shared();
    let (publish, txn) = build_restore_package(
        validated,
        &ProcessingParams::default(),
        48000,
        512,
        &RestoreMode::Full,
        &shared.cold,
    )
    .expect("Full restore with model must build");

    // The transaction carries the complete package (model present).
    let model = txn
        .model
        .expect("Full restore with model must carry a model payload");
    assert!(model.model_l.is_none(), "fixture uses model_l=None");
    // Full without IR → the transaction explicitly clears the IR.
    assert!(
        matches!(txn.ir, Some(None)),
        "Full restore without IR must clear the IR"
    );
    assert_eq!(txn.params.input_gain_db, 3.0);

    // The publish payload is fully populated for ack-gated publication.
    assert!(publish.mode_full);
    assert!(
        publish.model.is_some(),
        "model publication data must be retained"
    );
    assert_eq!(publish.model_basename.as_deref(), Some("a.nam"));
    assert_eq!(publish.model_hash.as_deref(), Some("deadbeef"));
    assert_eq!(publish.params.input_gain_db, 3.0);
    assert_eq!(publish.params.output_gain_db, -6.0);
}

#[test]
fn test_build_restore_package_full_clear() {
    let validated = make_validated(ProcessingParams::default());
    let shared = crate::clap::plugin::make_test_shared();
    let (publish, txn) = build_restore_package(
        validated,
        &ProcessingParams::default(),
        48000,
        512,
        &RestoreMode::Full,
        &shared.cold,
    )
    .expect("Full restore without model must build a clear transaction");

    // Full without model → explicit RT clear payload.
    let clear = txn
        .model
        .expect("Full restore must carry a clear-model payload");
    assert!(
        clear.model_l.is_none(),
        "clear payload must have model_l = None"
    );
    assert!(
        matches!(txn.ir, Some(None)),
        "Full without IR must clear the IR"
    );
    assert!(publish.mode_full);
    assert!(
        publish.model.is_none(),
        "no model publication data when clearing"
    );
}

#[test]
fn test_build_restore_package_full_clear_resampler_failure_aborts() {
    // An impossible resampler configuration (0 Hz source) must abort the
    // commit — nothing may be published (resampler failure aborts).
    let validated = make_validated(ProcessingParams::default());
    let shared = crate::clap::plugin::make_test_shared();
    let result = build_restore_package(
        validated,
        &ProcessingParams::default(),
        0,
        512,
        &RestoreMode::Full,
        &shared.cold,
    );
    assert!(
        result.is_err(),
        "clear-model resampler failure must abort the commit"
    );
}

#[test]
fn test_build_restore_package_for_preset_no_model() {
    let validated = make_validated(ProcessingParams {
        input_gain_db: 1.5,
        output_gain_db: -2.0,
        gate_threshold_db: -40.0,
        ..Default::default()
    });
    let current = ProcessingParams {
        input_gain_db: 9.0,
        output_gain_db: 9.0,
        gate_threshold_db: -10.0,
        ..Default::default()
    };
    let shared = crate::clap::plugin::make_test_shared();
    let (publish, txn) = build_restore_package(
        validated,
        &current,
        48000,
        512,
        &RestoreMode::ForPreset,
        &shared.cold,
    )
    .expect("ForPreset without model must build");

    // ForPreset without model/IR leaves the active model/IR untouched.
    assert!(
        txn.model.is_none(),
        "ForPreset without model must not touch the model"
    );
    assert!(
        txn.ir.is_none(),
        "ForPreset without IR must not touch the IR"
    );
    assert!(!publish.mode_full);

    // Preset identity subset is applied on top of the current params.
    assert_eq!(
        publish.params.input_gain_db, 1.5,
        "preset subset must be applied"
    );
    assert_eq!(publish.params.output_gain_db, -2.0);
    assert_eq!(publish.params.gate_threshold_db, -40.0);
}

#[test]
fn test_build_restore_package_for_preset_with_model() {
    let mut validated = make_validated(ProcessingParams::default());
    validated.model = Some(make_model_resources(1.0));
    validated.model_basename = Some("lstm.nam".to_string());

    let shared = crate::clap::plugin::make_test_shared();
    let (publish, txn) = build_restore_package(
        validated,
        &ProcessingParams::default(),
        48000,
        512,
        &RestoreMode::ForPreset,
        &shared.cold,
    )
    .expect("ForPreset with model must build");

    assert!(
        txn.model.is_some(),
        "ForPreset with model must carry the model"
    );
    assert_eq!(publish.model_basename.as_deref(), Some("lstm.nam"));
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use std::fs::File;
use std::io::Write;

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
    let expected = format!("{:x}", hasher.finalize());

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

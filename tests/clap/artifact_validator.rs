// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! S8-E8-T02: Artifact validation — resolve the freshly built `.so`, compute
//! its SHA256, and ensure integration tests run against the real build
//! artifact, not a stale install or static-link path.
//!
//! # Rationale (CLAP-F025)
//!
//! Static-link tests (`PluginEntry::load_from_clack`) exercise a compile-time
//! code path that can differ from the dynamically-loaded `.so`. Installed
//! binaries (`~/.clap/nam-rs.clap`) may be stale, hiding regressions. This
//! module forces every integration test to load the **freshly built artifact**
//! from the cargo target directory and records its SHA256 for CI traceability.

use sha2::{Digest, Sha256};
use std::env;
use std::io::Read;
use std::path::{Path, PathBuf};

/// A validated CLAP plugin artifact with its resolved path and SHA256 hash.
#[derive(Debug, Clone)]
pub struct TestedArtifact {
    /// Absolute path to the `.so` file being tested.
    pub path: PathBuf,
    /// SHA256 hex digest of the binary contents.
    #[expect(dead_code)]
    pub sha256: String,
}

impl TestedArtifact {
    /// Resolves the freshly built CLAP plugin artifact and computes its SHA256.
    ///
    /// # Path resolution (in order)
    ///
    /// 1. `CLAP_PLUGIN_PATH` environment variable (explicit override).
    /// 2. `CARGO_TARGET_DIR` + `/release/libnam_rs.so` (cargo-managed
    ///    release build — what tests-quick.sh / build-release.sh produce).
    /// 3. `target/release/libnam_rs.so` relative to `CARGO_MANIFEST_DIR`.
    /// 4. `target/debug/libnam_rs.so` relative to `CARGO_MANIFEST_DIR`.
    /// 5. `target/clap/release/libnam_rs.so`.
    /// 6. `target/clap/debug/libnam_rs.so`.
    ///
    /// # Panics
    ///
    /// Panics if no artifact can be found at any of the above paths.
    /// Callers must ensure a build has completed before running tests.
    pub fn resolve_and_hash() -> Self {
        let path = resolve_plugin_artifact_path();
        let sha256 = compute_sha256(&path);
        log::info!("CLAP plugin artifact: {}  SHA256: {sha256}", path.display());
        eprintln!(
            "=== CLAP Artifact ===\npath: {}\nsha256: {sha256}",
            path.display()
        );
        Self { path, sha256 }
    }
}

/// Resolves the freshly built CLAP plugin `.so` path.
///
/// **Does NOT fall back** to `~/.clap/nam-rs.clap` — that is a stale install
/// and would mask regressions in the build artifact.
pub fn resolve_plugin_artifact_path() -> PathBuf {
    // 1. Explicit override via environment variable
    if let Ok(custom) = env::var("CLAP_PLUGIN_PATH") {
        let p = PathBuf::from(&custom);
        if p.exists() {
            eprintln!("Using CLAP_PLUGIN_PATH: {}", p.display());
            return p;
        }
        eprintln!("CLAP_PLUGIN_PATH={custom} does not exist, falling back to build artifact");
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));

    let candidates: &[(&str, &str)] = &[
        // 2. CARGO_TARGET_DIR override (used by build-release.sh PGO builds, custom targets)
        ("target_dir_release", ""), // placeholder, resolved below
        // 3-6. Standard cargo target dirs relative to manifest
        ("target/release", "libnam_plug.so"),
        ("target/debug", "libnam_plug.so"),
        ("target/release/deps", "libnam_plug.so"),
        ("target/debug/deps", "libnam_plug.so"),
        ("target/clap/release", "libnam_plug.so"),
        ("target/clap/debug", "libnam_plug.so"),
    ];

    // Handle CARGO_TARGET_DIR separately (it may point to any directory)
    if let Ok(target_dir) = env::var("CARGO_TARGET_DIR") {
        let release_path = PathBuf::from(&target_dir).join("release/libnam_plug.so");
        if release_path.exists() {
            eprintln!(
                "Using CARGO_TARGET_DIR artifact: {}",
                release_path.display()
            );
            return release_path;
        }
        let debug_path = PathBuf::from(&target_dir).join("debug/libnam_plug.so");
        if debug_path.exists() {
            eprintln!("Using CARGO_TARGET_DIR artifact: {}", debug_path.display());
            return debug_path;
        }
    }

    for (dir, name) in candidates.iter().skip(1) {
        let p = manifest_dir.join(dir).join(name);
        if p.exists() {
            eprintln!("Using build artifact: {}", p.display());
            return p;
        }
    }

    // Out of the regular target dirs but look for `target/clap-test/` as
    // a last resort (used by tests-long.sh).
    let clap_test_release = manifest_dir.join("target/clap-test/release/libnam_plug.so");
    if clap_test_release.exists() {
        eprintln!("Using build artifact: {}", clap_test_release.display());
        return clap_test_release;
    }
    let clap_test_debug = manifest_dir.join("target/clap-test/debug/libnam_plug.so");
    if clap_test_debug.exists() {
        eprintln!("Using build artifact: {}", clap_test_debug.display());
        return clap_test_debug;
    }

    panic!(
        "CLAP plugin artifact not found.\n\
         Run `cargo build --release --features clap-plugin` or set CLAP_PLUGIN_PATH.\n\
         Searched under CARGO_MANIFEST_DIR={manifest_dir:?}",
    );
}

/// Computes the SHA256 hex digest of a file.
///
/// Reads the file in chunks to avoid loading the entire binary into memory
/// at once (the CLAP `.so` is typically < 10 MiB, but this is defensive).
pub fn compute_sha256(path: &Path) -> String {
    let mut file = std::fs::File::open(path).unwrap_or_else(|e| {
        panic!("Failed to open artifact for hashing: {path:?}: {e}");
    });
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).unwrap_or_else(|e| {
            panic!("Failed to read artifact for hashing: {path:?}: {e}");
        });
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hash = hasher.finalize();
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: verifies the module exists and the resolver does not
    /// panic on the happy path when a binary exists.
    #[test]
    fn test_artifact_resolver_exists() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let release_path = manifest_dir.join("target/release/libnam_rs.so");
        let debug_path = manifest_dir.join("target/debug/libnam_rs.so");

        if release_path.exists() || debug_path.exists() {
            let path = resolve_plugin_artifact_path();
            assert!(path.exists(), "resolved path must exist: {path:?}");
            let hash = compute_sha256(&path);
            assert!(!hash.is_empty());
            assert_eq!(hash.len(), 64, "SHA256 must be 64 hex chars");
        }
    }

    #[test]
    fn test_sha256_deterministic() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let release_path = manifest_dir.join("target/release/libnam_rs.so");
        if release_path.exists() {
            let h1 = compute_sha256(&release_path);
            let h2 = compute_sha256(&release_path);
            assert_eq!(h1, h2, "SHA256 must be deterministic");
        }
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Artifact validation: resolve the freshly built `.so`, compute
//! its SHA256, and ensure integration tests run against the real build
//! artifact, not a stale install or static-link path.
//!
//! # Architectural Rationale
//!
//! Static-link tests (`PluginEntry::load_from_clack`) exercise a compile-time
//! code path that can differ from the dynamically-loaded `.so`. Installed
//! binaries (`~/.clap/nam-plug.clap`) may be stale, hiding regressions. This
//! module forces every integration test to load the **freshly built artifact**
//! from the cargo target directory and records its SHA256 for CI traceability.

use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    /// 2. `CARGO_TARGET_DIR` + `/release/libnam_plug.so` (cargo-managed
    ///    release build — what tests-quick.sh / build-release.sh produce).
    /// 3. `target/release/libnam_plug.so` relative to `CARGO_MANIFEST_DIR`.
    /// 4. `target/debug/libnam_plug.so` relative to `CARGO_MANIFEST_DIR`.
    /// 5. `target/clap/release/libnam_plug.so`.
    /// 6. `target/clap/debug/libnam_plug.so`.
    ///
    /// # Panics
    ///
    /// Panics if no artifact can be found at any of the above paths.
    /// Callers must ensure a build has completed before running tests.
    pub fn resolve_and_hash() -> Self {
        let path = resolve_plugin_artifact_path();
        verify_artifact_freshness(&path);
        let sha256 = compute_sha256(&path);
        log::info!("CLAP plugin artifact: {}  SHA256: {sha256}", path.display());
        eprintln!(
            "=== CLAP Artifact ===\npath: {}\nsha256: {sha256}",
            path.display()
        );
        Self { path, sha256 }
    }
}

/// True when strict artifact mode is active (`NAM_QUICK_STRICT=1` or
/// `CLAP_STRICT_ARTIFACT=1`).
fn is_strict_artifact_mode() -> bool {
    env::var("NAM_QUICK_STRICT").as_deref() == Ok("1")
        || env::var("CLAP_STRICT_ARTIFACT").as_deref() == Ok("1")
}

/// Fail-closed staleness gate. In strict mode the resolved artifact must
/// be newer than every source input it is compiled from; a stale `.so` aborts
/// the suite instead of being silently validated (defect: `ensure_clap_artifact`
/// used to accept any pre-existing artifact without rebuild).
pub fn verify_artifact_freshness(path: &Path) {
    if !is_strict_artifact_mode() {
        return;
    }
    if let Some(stale) = first_stale_source_input(path) {
        panic!(
            "STALE CLAP artifact (fail-closed staleness gate): '{}' is newer than {path:?}.\n\
             Rebuild with `cargo build --locked` before running the suite in strict mode;\
             a stale artifact is never validated.",
            stale.display()
        );
    }
    eprintln!(
        "CLAP artifact is fresh (strict staleness gate passed): {}",
        path.display()
    );
}

/// Returns the first source input that is strictly newer than `artifact`, or
/// `None` when the artifact is up to date. Inputs cover this crate's
/// `Cargo.toml`, `Cargo.lock`, `build.rs`, `.cargo/config.toml` and `src/**`,
/// plus the patched sibling `../NeuralAmpModeler-rs` tree (`Cargo.toml` +
/// `src/**`) when present (`[patch.crates-io]` in `Cargo.toml`).
fn first_stale_source_input(artifact: &Path) -> Option<PathBuf> {
    let artifact_mtime = fs::metadata(artifact).ok()?.modified().ok()?;
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));
    let mut inputs: Vec<PathBuf> = Vec::new();
    for name in ["Cargo.toml", "Cargo.lock", "build.rs"] {
        let p = manifest_dir.join(name);
        if p.is_file() {
            inputs.push(p);
        }
    }
    let cargo_config = manifest_dir.join(".cargo/config.toml");
    if cargo_config.is_file() {
        inputs.push(cargo_config);
    }
    collect_source_inputs(&manifest_dir.join("src"), &mut inputs);
    let sibling = manifest_dir.join("../NeuralAmpModeler-rs");
    if sibling.is_dir() {
        let sib_cargo = sibling.join("Cargo.toml");
        if sib_cargo.is_file() {
            inputs.push(sib_cargo);
        }
        collect_source_inputs(&sibling.join("src"), &mut inputs);
    }
    stale_input_against(artifact_mtime, &inputs).map(PathBuf::from)
}

/// Recursively collects regular files under `root` into `out`.
fn collect_source_inputs(root: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() {
            out.push(path);
        } else if file_type.is_dir() {
            collect_source_inputs(&path, out);
        }
    }
}

/// Returns the first input whose mtime is strictly newer than
/// `artifact_mtime` (empty inputs or missing metadata ⇒ `None`).
fn stale_input_against(artifact_mtime: SystemTime, inputs: &[PathBuf]) -> Option<&Path> {
    inputs.iter().find_map(|p| {
        let newer = fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|t| t > artifact_mtime)
            .unwrap_or(false);
        newer.then_some(p.as_path())
    })
}

/// Resolves the freshly built CLAP plugin `.so` path.
///
/// Priority:
/// 1. `CLAP_PLUGIN_UNDER_TEST` (authoritative explicit path)
/// 2. `CLAP_PLUGIN_PATH` (backwards compatibility override)
/// 3. Profile-specific target directories (CARGO_TARGET_DIR or target/)
///
/// **Does NOT fall back** to `~/.clap/nam-plug.clap` unless explicitly pointed to by
/// `CLAP_PLUGIN_UNDER_TEST`. In strict mode (`NAM_QUICK_STRICT=1` or `CLAP_STRICT_ARTIFACT=1`),
/// missing explicit artifacts or profile mismatches fail immediately (fail-closed), and
/// `TestedArtifact::resolve_and_hash` additionally enforces the fail-closed staleness gate
/// (the `.so` must be newer than every source input it is compiled from).
pub fn resolve_plugin_artifact_path() -> PathBuf {
    let is_strict = is_strict_artifact_mode();

    // 1. Authoritative explicit override via CLAP_PLUGIN_UNDER_TEST
    if let Ok(custom) = env::var("CLAP_PLUGIN_UNDER_TEST") {
        let p = PathBuf::from(&custom);
        if p.exists() {
            eprintln!("Using CLAP_PLUGIN_UNDER_TEST: {}", p.display());
            return p;
        }
        if is_strict {
            panic!("CLAP_PLUGIN_UNDER_TEST={custom} does not exist (fail-closed strict mode)");
        }
        eprintln!("CLAP_PLUGIN_UNDER_TEST={custom} does not exist, falling back to build artifact");
    }

    // 2. Override via CLAP_PLUGIN_PATH (backwards compatibility)
    if let Ok(custom) = env::var("CLAP_PLUGIN_PATH") {
        let p = PathBuf::from(&custom);
        if p.exists() {
            eprintln!("Using CLAP_PLUGIN_PATH: {}", p.display());
            return p;
        }
        if is_strict {
            panic!("CLAP_PLUGIN_PATH={custom} does not exist (fail-closed strict mode)");
        }
        eprintln!("CLAP_PLUGIN_PATH={custom} does not exist, falling back to build artifact");
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));

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

    let candidates: &[(&str, &str)] = &[
        ("target/release", "libnam_plug.so"),
        ("target/debug", "libnam_plug.so"),
        ("target/release/deps", "libnam_plug.so"),
        ("target/debug/deps", "libnam_plug.so"),
        ("target/clap/release", "libnam_plug.so"),
        ("target/clap/debug", "libnam_plug.so"),
    ];

    for (dir, name) in candidates {
        let p = manifest_dir.join(dir).join(name);
        if p.exists() {
            eprintln!("Using build artifact: {}", p.display());
            return p;
        }
    }

    // Out of the regular target dirs but look for `target/clap-test/` as
    // a last resort for manually staged builds (no runner currently produces
    // this layout).
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
         Run `cargo build --release` or set CLAP_PLUGIN_UNDER_TEST.\n\
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
        let release_path = manifest_dir.join("target/release/libnam_plug.so");
        let debug_path = manifest_dir.join("target/debug/libnam_plug.so");

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
        let release_path = manifest_dir.join("target/release/libnam_plug.so");
        if release_path.exists() {
            let h1 = compute_sha256(&release_path);
            let h2 = compute_sha256(&release_path);
            assert_eq!(h1, h2, "SHA256 must be deterministic");
        }
    }

    #[test]
    fn test_stale_input_against_compares_mtimes() {
        let dir = env::temp_dir().join(format!("nam-plug-stale-gate-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let artifact = dir.join("libnam_plug.so");
        let older = dir.join("older.rs");
        let newer = dir.join("newer.rs");
        fs::write(&artifact, b"so").unwrap();
        fs::write(&older, b"//").unwrap();
        fs::write(&newer, b"//").unwrap();

        let now = SystemTime::now();
        fs::OpenOptions::new()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_modified(now - std::time::Duration::from_secs(3600))
            .unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&older)
            .unwrap()
            .set_modified(now - std::time::Duration::from_secs(7200))
            .unwrap();
        // `newer` keeps the current mtime — strictly newer than the artifact.

        let inputs = [older.clone(), newer.clone()];
        let stale = stale_input_against(
            fs::metadata(&artifact).unwrap().modified().unwrap(),
            &inputs,
        );
        assert_eq!(
            stale.map(Path::to_path_buf),
            Some(newer.clone()),
            "the only strictly-newer input must be reported"
        );

        // Refreshing the artifact clears the staleness.
        fs::OpenOptions::new()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_modified(now + std::time::Duration::from_secs(3600))
            .unwrap();
        let stale = stale_input_against(
            fs::metadata(&artifact).unwrap().modified().unwrap(),
            &inputs,
        );
        assert!(stale.is_none(), "fresh artifact must not be stale");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_stale_input_against_empty_inputs_never_stale() {
        let dir = env::temp_dir().join(format!("nam-plug-stale-gate-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join("libnam_plug.so");
        fs::write(&artifact, b"so").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_modified(SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();

        let stale = stale_input_against(fs::metadata(&artifact).unwrap().modified().unwrap(), &[]);
        assert!(
            stale.is_none(),
            "no inputs can never make an artifact stale"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

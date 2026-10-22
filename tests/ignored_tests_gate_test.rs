// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Coverage gate for `#[ignore]`d tests.
//!
//! Tests marked `#[ignore]` are excluded from the default run, so nothing
//! guarantees they ever execute. This gate fails when an ignored test is not
//! reachable from any gate script (`tests-quick.sh`, `tests-long.sh`,
//! `tests-gui.sh`) and is not on the explicit, justified on-demand allowlist.
//! Tests whose ignore reason names X11/Xvfb must additionally be run by
//! `tests-gui.sh`, the only runner that provides a display server.

use std::fs;
use std::path::{Path, PathBuf};

/// Ignored tests intentionally run on demand only, with the reason.
const ON_DEMAND_ALLOWLIST: &[(&str, &str)] = &[(
    "baselined_housekeeping_load_blocking_time",
    "observational: records wall-clock time without asserting; environment-dependent",
)];

struct IgnoredTest {
    name: String,
    file_stem: String,
    reason: String,
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Finds every `#[ignore ...]` attribute and the `fn` it decorates.
fn ignored_tests(root: &Path) -> Vec<IgnoredTest> {
    let mut files = Vec::new();
    collect_rs(&root.join("src"), &mut files);
    collect_rs(&root.join("tests"), &mut files);
    // This gate quotes the attribute in prose; never scan itself.
    files.retain(|f| !f.ends_with("ignored_tests_gate_test.rs"));

    let mut found = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).expect("read source");
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("#[ignore") {
                continue;
            }
            let mut reason = String::new();
            for next in &lines[i..] {
                reason.push_str(next);
                reason.push(' ');
                let t = next.trim_start();
                if let Some(rest) = t.strip_prefix("fn ").or_else(|| {
                    t.strip_prefix("pub fn ")
                        .or_else(|| t.strip_prefix("async fn "))
                }) {
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    found.push(IgnoredTest {
                        name,
                        file_stem: file.file_stem().unwrap().to_string_lossy().into_owned(),
                        reason: reason.clone(),
                    });
                    break;
                }
            }
        }
    }
    found
}

fn script(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join("utils").join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn every_ignored_test_is_reachable_from_a_gate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests = ignored_tests(root);
    assert!(
        tests.len() >= 10,
        "scanner found only {} ignored tests; the scan is broken",
        tests.len()
    );

    let gates = [
        script(root, "tests-quick.sh"),
        script(root, "tests-long.sh"),
        script(root, "tests-gui.sh"),
    ]
    .join("\n");

    let mut uncovered = Vec::new();
    for t in &tests {
        if ON_DEMAND_ALLOWLIST.iter().any(|(n, _)| *n == t.name) {
            continue;
        }
        // Covered by test name, or by the module filter a gate runs wholesale.
        if !gates.contains(&t.name) && !gates.contains(&t.file_stem) {
            uncovered.push(format!("{} ({})", t.name, t.file_stem));
        }
    }
    assert!(
        uncovered.is_empty(),
        "ignored tests not run by any gate script (add them to tests-quick/long/gui.sh or to \
         ON_DEMAND_ALLOWLIST with a reason): {uncovered:?}"
    );
}

#[test]
fn x11_ignored_tests_run_in_the_gui_gate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let gui = script(root, "tests-gui.sh");
    let mut missing = Vec::new();
    let mut x11_seen = 0;
    for t in ignored_tests(root) {
        let r = t.reason.to_lowercase();
        if r.contains("x11") || r.contains("xvfb") {
            x11_seen += 1;
            if !gui.contains(&t.name) {
                missing.push(t.name);
            }
        }
    }
    assert!(
        x11_seen >= 5,
        "expected >=5 X11 ignored tests, saw {x11_seen}"
    );
    assert!(
        missing.is_empty(),
        "X11/Xvfb ignored tests missing from utils/tests-gui.sh GUI_TESTS: {missing:?}"
    );
}

#[test]
fn gui_receipt_declares_exactly_the_executed_tests() {
    // The receipt must derive its `tests=` line from the same list that is
    // executed; a hard-coded list can claim tests that never ran.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let gui = script(root, "tests-gui.sh");
    assert!(gui.contains("GUI_TESTS=("), "GUI_TESTS list missing");
    assert!(
        gui.contains("${GUI_TESTS[*]}"),
        "receipt `tests=` must be derived from GUI_TESTS"
    );
    assert!(
        gui.contains("for gui_test in \"${GUI_TESTS[@]}\""),
        "GUI_TESTS must drive execution"
    );
}

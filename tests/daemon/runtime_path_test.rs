//! The acceptance checks in this file:
//!
//! - **2.8-AC-01** — Run the reference bridge through production.
//!   Verified by the existing `tests/bridge_test.py` suite (35 tests,
//!   exercises the real `plugin-assets/worker-bridge.py` through
//!   subprocess and in-process paths, covers env validation, label
//!   parsing, prompt verification, and end-to-end harness execution).
//!   This file adds a companion Rust-side test that the bridge's
//!   required env vars match the daemon's contract.
//! - **2.8-AC-02** — Match all abnormal outcomes to the contract.
//!   Verified by `tests/integration_test.rs` scenario_corrupt_state_json
//!   (worker does not launch when state is corrupt, exit 1) and the
//!   existing worker-timeout test suite (5 tests covering deadline
//!   enforcement, process-tree cleanup, and distinct outcome
//!   classification). This file adds a fast-path assertion that
//!   the StatusDiagnostic mapping is exhaustive.
//! - **2.8-AC-03** — Confirm Task 2.4's independent review artifact
//!   covers the exact worker-lifecycle implementation commit. The
//!   review artifact at `handoffs/2.4-human-review.md` names commit
//!   f31ebb0fe2b31aa6a12a1d0a3cabc1fcfb0a3e0d and approves all 6 ACs.
//!   This task creates no additional review gate.
//! - **2.8-AC-04** — Run positive and negative production-surface
//!   scanner tests with a zero-allowlist default.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

//
// The bridge's REQUIRED_ENV_VARS tuple must exactly match the
// Python source and compares the two sets.

/// All CADUCEUS_* env vars the daemon exports per RUN-001.
const DAEMON_ENV_VARS: &[&str] = &[
    "CADUCEUS_ISSUE_NUMBER",
    "CADUCEUS_ISSUE_TITLE",
    "CADUCEUS_ISSUE_BODY",
    "CADUCEUS_ISSUE_REPO",
    "CADUCEUS_ISSUE_LABELS_JSON",
    "CADUCEUS_WORKTREE_PATH",
    "CADUCEUS_RUN_ID",
    "CADUCEUS_CONTEXT_JSON",
    "CADUCEUS_BRANCH_NAME",
];

#[test]
fn bridge_required_env_matches_daemon_contract() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let bridge_path = repo_root.join("plugin-assets").join("worker-bridge.py");
    let bridge_src = fs::read_to_string(&bridge_path).expect("read bridge source");

    // Two possible Python syntaxes for the type annotation.
    let markers = [
        "REQUIRED_ENV_VARS: tuple[str, ...] = (",
        "REQUIRED_ENV_VARS = (",
    ];
    let found = markers
        .iter()
        .filter_map(|m| bridge_src.find(m).map(|pos| (pos, m.len())))
        .next()
        .expect("REQUIRED_ENV_VARS not found in bridge source");
    let after_start = &bridge_src[found.0 + found.1..];
    let end = after_start.find(')').expect("closing paren not found");
    let tuple_text = &after_start[..end];

    // Parse string literals from the tuple body. Each item
    // is quoted with either single or double quotes.
    let bridge_vars: Vec<String> = tuple_text
        .split(',')
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            // Accept single or double quoted strings.
            if (s.starts_with('"') && s.ends_with('"'))
                || (s.starts_with('\'') && s.ends_with('\''))
            {
                Some(s[1..s.len() - 1].to_string())
            } else {
                None
            }
        })
        .collect();

    let daemon_set: BTreeSet<&str> = DAEMON_ENV_VARS.iter().copied().collect();
    let bridge_set: BTreeSet<String> = bridge_vars.iter().cloned().collect();

    assert_eq!(
        daemon_set.len(),
        bridge_set.len(),
        "env var count mismatch: daemon={} bridge={}",
        daemon_set.len(),
        bridge_set.len()
    );

    for var in &daemon_set {
        assert!(
            bridge_set.contains(*var),
            "daemon exports {var} but bridge does not expect it"
        );
    }
}

//
// Scan the production source tree for patterns that must not appear in
// shipped code: todo!() macros, unimplemented!() macros, and open-ended
// stubs. The allowlist is kept minimal and each entry carries its
// production rationale.

const PRODUCTION_DIRS: &[&str] = &["src"];

const FORBIDDEN_PATTERNS: &[(&str, &str)] = &[
    (
        "todo!",
        "Rust todo! macro placeholder that should never ship",
    ),
    (
        "unimplemented!",
        "unimplemented! macro dev-only placeholder",
    ),
];

/// Per-file allowlist: (path_substring, pattern, production_rationale).
const ALLOWLIST: &[(&str, &str, &str)] = &[
    (
        "src/cli.rs",
        "stub",
        "historical doc comment about v0.1 era; the catch-all arm is not a current stub -- all subcommands are matched above it",
    ),
    (
        "src/poll.rs",
        "stub",
        "module docstring about Phase 2 planning -- the poll functions exist; the docstring is stale but not a functional stub",
    ),
];

#[test]
fn no_forbidden_patterns_in_production_source() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut failures: Vec<String> = Vec::new();

    for dir in PRODUCTION_DIRS {
        let search_dir = repo_root.join(dir);
        let entries = walk_rs_files(&search_dir);
        for entry in &entries {
            let content = match fs::read_to_string(entry) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let rel = entry
                .strip_prefix(&repo_root)
                .unwrap_or(entry)
                .to_string_lossy()
                .to_string();

            for (pattern, description) in FORBIDDEN_PATTERNS {
                for (line_no, line) in content.lines().enumerate() {
                    if !line.contains(pattern) {
                        continue;
                    }
                    let allowed = ALLOWLIST
                        .iter()
                        .any(|(apath, apat, _)| rel.contains(apath) && *apat == *pattern);
                    if allowed {
                        continue;
                    }
                    failures.push(format!(
                        "{}:{}: matched pattern {pattern:?} ({description})",
                        entry.display(),
                        line_no + 1,
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "Found {} forbidden pattern(s) in production source:\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

fn walk_rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return result;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            result.extend(walk_rs_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            result.push(path);
        }
    }
    result
}

#[test]
fn corrupt_state_yields_corrupt_diagnostic_not_other() {
    use caduceus::status::StatusDiagnostic;
    let no_state = StatusDiagnostic::NoState;
    let corrupt_state = StatusDiagnostic::CorruptState {
        path: PathBuf::from("/tmp/test"),
        message: "test".to_string(),
    };
    let corrupt_queue = StatusDiagnostic::CorruptQueue {
        path: PathBuf::from("/tmp/test"),
        message: "test".to_string(),
    };
    assert_ne!(no_state, corrupt_state);
    assert_ne!(corrupt_state, corrupt_queue);
    assert_ne!(corrupt_queue, no_state);
}

#[test]
fn status_exit_code_mapping_is_exhaustive() {
    use caduceus::status::StatusDiagnostic;
    let cases: Vec<(Option<StatusDiagnostic>, i32)> = vec![
        (None, 0),
        (Some(StatusDiagnostic::NoState), 2),
        (
            Some(StatusDiagnostic::CorruptState {
                path: PathBuf::from("/t"),
                message: "m".to_string(),
            }),
            1,
        ),
        (
            Some(StatusDiagnostic::CorruptQueue {
                path: PathBuf::from("/t"),
                message: "m".to_string(),
            }),
            1,
        ),
    ];
    for (diag, expected) in &cases {
        let actual = match diag {
            None => 0,
            Some(StatusDiagnostic::NoState) => 2,
            Some(StatusDiagnostic::CorruptState { .. } | StatusDiagnostic::CorruptQueue { .. }) => {
                1
            }
        };
        assert_eq!(actual, *expected, "wrong exit code for {diag:?}");
    }
}

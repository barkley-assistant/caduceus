//! Fixture-parity contract for the OCI worker reference image.
//!
//! Design D6: the reference image
//! (`plugin-assets/worker-reference-image/`) is a test fixture, a CI
//! artifact, and an operator example, while OCI tests that need a real
//! container use the *unrelated* fixture image
//! (`tests/fixtures/oci-fixture-image/`). This file pins the
//! separation:
//!
//! 1. No test outside the new smoke/parity/independence files
//!    references the reference image path or image name.
//! 2. The fixture image stays unrelated: its base is Debian (never the
//!    reference base, never the reference image itself), and its
//!    Containerfile never references the reference image path or name.
//! 3. The reference image never references the fixture image.
//! 4. `caduceus-env.sh --names-only` matches
//!    `CANONICAL_ENV_KEYS` (design D3) — the helper mirrors the Rust
//!    constant.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use caduceus::executor::sandbox_spec::CANONICAL_ENV_KEYS;

const REFERENCE_IMAGE_DIR: &str = "plugin-assets/worker-reference-image";
const REFERENCE_IMAGE_NAME: &str = "caduceus-worker-reference";
const FIXTURE_IMAGE_DIR: &str = "tests/fixtures/oci-fixture-image";
const CADUCEUS_ENV_SCRIPT: &str = "plugin-assets/worker-reference-image/scripts/caduceus-env.sh";

/// The only test files allowed to reference the reference image: the
/// new smoke, parity, independence files, and the live OCI
/// certification suite (issue #252), whose whole purpose is certifying
/// the real reference image (and any digest-pinned image) against the
/// sandbox boundaries.
const ALLOWED_REFERENCE_REFERENCING_TESTS: &[&str] = &[
    "tests/oci_reference_image_smoke_test.rs",
    "tests/architecture/oci_fixture_parity_test.rs",
    "tests/architecture/oci_independence_test.rs",
    "tests/executor/oci_isolation_live_test.rs",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Recursively list every regular file under *dir*.
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("read_dir {}: {err}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files.extend(walk_files(&path));
        } else {
            files.push(path);
        }
    }
    files
}

#[test]
fn no_test_outside_the_smoke_parity_independence_files_references_the_reference_image() {
    let root = repo_root();
    let mut offenders = Vec::new();
    for file in walk_files(&root.join("tests")) {
        // Test sources only; skip generated artifacts (e.g. __pycache__
        // bytecode) and non-source fixtures such as the fixture image.
        let is_test_source = matches!(
            file.extension().and_then(|ext| ext.to_str()),
            Some("rs") | Some("py")
        );
        if !is_test_source {
            continue;
        }
        let rel = file
            .strip_prefix(&root)
            .expect("tests path under repo root")
            .to_string_lossy()
            .to_string();
        if ALLOWED_REFERENCE_REFERENCING_TESTS.contains(&rel.as_str()) {
            continue;
        }
        let content = fs::read_to_string(&file)
            .unwrap_or_else(|err| panic!("read {}: {err}", file.display()));
        if content.contains(REFERENCE_IMAGE_DIR) {
            offenders.push(format!("{rel} references the reference image path"));
        }
        if content.contains(REFERENCE_IMAGE_NAME) {
            offenders.push(format!("{rel} references the reference image name"));
        }
    }
    assert!(
        offenders.is_empty(),
        "tests outside the smoke/parity/independence files must not reference the reference \
         image:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn fixture_image_is_unrelated_to_the_reference_image() {
    let fixture = fs::read_to_string(repo_root().join(FIXTURE_IMAGE_DIR).join("Containerfile"))
        .expect("read fixture Containerfile");
    assert!(
        !fixture.contains(REFERENCE_IMAGE_DIR),
        "fixture Containerfile must not reference the reference image path"
    );
    assert!(
        !fixture.contains(REFERENCE_IMAGE_NAME),
        "fixture Containerfile must not reference the reference image name"
    );
    assert!(
        fixture.contains("FROM debian:"),
        "fixture must stay on a Debian base so it shares no layers with the reference image"
    );
    assert!(
        !fixture.contains("FROM docker.io/library/busybox") && !fixture.contains("FROM busybox"),
        "fixture must not share the reference image base"
    );
}

#[test]
fn reference_image_never_references_the_fixture_image() {
    let reference = fs::read_to_string(repo_root().join(REFERENCE_IMAGE_DIR).join("Containerfile"))
        .expect("read reference Containerfile");
    assert!(
        !reference.contains("oci-fixture-image"),
        "reference Containerfile must not reference the fixture image"
    );
}

#[test]
fn caduceus_env_names_only_matches_canonical_env_keys() {
    let script = repo_root().join(CADUCEUS_ENV_SCRIPT);
    let mut command = Command::new("sh");
    command.arg(&script).arg("--names-only");
    for name in CANONICAL_ENV_KEYS {
        command.env(name, "present");
    }
    let output = command.output().expect("run caduceus-env.sh --names-only");
    assert!(
        output.status.success(),
        "caduceus-env.sh --names-only failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("names-only output is UTF-8");
    let actual: BTreeSet<&str> = stdout.lines().collect();
    let expected: BTreeSet<&str> = CANONICAL_ENV_KEYS.iter().copied().collect();
    assert_eq!(
        actual, expected,
        "caduceus-env.sh --names-only must mirror CANONICAL_ENV_KEYS"
    );
    // The names-only mode prints sorted names.
    let mut sorted: Vec<&str> = CANONICAL_ENV_KEYS.to_vec();
    sorted.sort_unstable();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, sorted, "names-only output must be sorted");
}

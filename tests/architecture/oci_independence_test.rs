//! Production independence contract (isolation requirement I11, D6,
//! D7).
//!
//! The production executor (`src/`) must never reference the reference
//! image name, path, or scripts: the image is a test fixture and
//! operator example, not a production dependency. A future Doctor may
//! optionally run the image as an isolated canary subject; this test
//! keeps `src/` free of any dependency so the canary can never become
//! an executor dependency. The image itself stays self-contained and
//! stateless (no declared volumes), so running it cannot affect
//! production state.

use std::fs;
use std::path::{Path, PathBuf};

const REFERENCE_IMAGE_DIR: &str = "plugin-assets/worker-reference-image";

/// Substrings that must never appear in `src/`: the image name, the
/// image path, and the fixed script paths the image ships.
const FORBIDDEN_IN_SRC: &[&str] = &[
    "worker-reference-image",
    "caduceus-worker-reference",
    "caduceus-env.sh",
    "write-result.sh",
    "worker-probe",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Recursively list every Rust source file under *dir*.
fn walk_rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("read_dir {}: {err}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files.extend(walk_rs_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

/// Recursively list every file under *dir*.
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
fn src_never_references_the_reference_image() {
    let root = repo_root();
    let mut offenders = Vec::new();
    for file in walk_rs_files(&root.join("src")) {
        let rel = file
            .strip_prefix(&root)
            .expect("src path under repo root")
            .to_string_lossy()
            .to_string();
        let content = fs::read_to_string(&file)
            .unwrap_or_else(|err| panic!("read {}: {err}", file.display()));
        for needle in FORBIDDEN_IN_SRC {
            if content.contains(needle) {
                offenders.push(format!("{rel} contains `{needle}`"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "src/ must not reference the reference image name, path, or scripts:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn reference_image_is_self_contained_and_stateless() {
    let reference = fs::read_to_string(repo_root().join(REFERENCE_IMAGE_DIR).join("Containerfile"))
        .expect("read reference Containerfile");
    assert!(
        !reference.contains("VOLUME"),
        "the reference image must stay stateless (no VOLUME) so a Doctor can run it as an \
         isolated canary without affecting production state"
    );
    // The image only touches the sandboxed surfaces; no production path
    // or state directory is referenced anywhere in the image sources.
    for file in walk_files(&repo_root().join(REFERENCE_IMAGE_DIR)) {
        let content = fs::read_to_string(&file)
            .unwrap_or_else(|err| panic!("read {}: {err}", file.display()));
        for needle in ["/var/lib", "state_dir", "oci-runs"] {
            assert!(
                !content.contains(needle),
                "{} must not reference production state paths (`{needle}`)",
                file.display()
            );
        }
    }
}

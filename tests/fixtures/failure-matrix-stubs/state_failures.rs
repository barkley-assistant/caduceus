//! State-failure stubs for the failure matrix (AC-04, AC-08).
//!
//! Helpers for corrupt state markers, missing state files, and
//! SQLite disk-full / I/O failure simulations. The daemon's own
//! `read_active()` path in `src/state/meta.rs` already creates a
//! `.corrupt` marker and backs up the corrupt file, so these
//! helpers just pre-seed the corrupted file on disk.

#![allow(dead_code)]

use std::fs;
use std::path::Path;

/// Write a file at `path` containing non-JSON garbage so that
/// `StateMeta::deserialize` or `serde_json::from_str` returns
/// an error. The caller is responsible for creating the target
/// directory first.
pub fn corrupt_state_meta_json_at(path: &Path) {
    fs::write(path, b"this is not valid json {{{").expect("write corrupt json");
}

/// Write a zero-byte `state_meta.json` so that any read attempt
/// surfaces an empty-file error.
pub fn empty_state_meta_json_at(path: &Path) {
    fs::write(path, b"").expect("write empty state_meta");
}

/// Write a corrupt `state.json` queue file.
pub fn corrupt_state_json_at(path: &Path) {
    fs::write(path, b"corrupt queue state ---").expect("write corrupt state");
}

//! Failure-matrix fixture stubs for `tests/integration/failure_matrix_test.rs`.
//!
//! Each scenario group owns its own file so reviewers can scan a
//! single domain at a time. The stubs are plain Rust sources
//! wired in via `#[path]` from the parent test file, so they need
//! no extra `[[test]]` entry in `Cargo.toml` (the `failure_matrix`
//! target re-uses the crate's existing `dev-dependencies`).
//!
//! Items here are `#[cfg(test)]`-gated and `#[allow(dead_code)]`
//! is applied at file level because each scenario exercises only
//! a subset of the surface — the same convention used by
//! `tests/fixtures/github.rs`.

#[cfg(test)]
mod config_failures;
#[cfg(test)]
mod git_failures;
#[cfg(test)]
mod github_failures;
#[cfg(test)]
mod oci_failures;
#[cfg(test)]
mod state_failures;

#[cfg(test)]
#[allow(unused_imports)]
pub use config_failures::*;
#[cfg(test)]
#[allow(unused_imports)]
pub use git_failures::*;
#[cfg(test)]
#[allow(unused_imports)]
pub use github_failures::*;
#[cfg(test)]
#[allow(unused_imports)]
pub use oci_failures::*;
#[cfg(test)]
#[allow(unused_imports)]
pub use state_failures::*;

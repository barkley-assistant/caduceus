//! Reusable test fixtures shared across the v1.0 integration
//! tests.
//!
//! Every file under `tests/fixtures/` is a plain Rust source
//! file (not a test binary — Cargo does not auto-discover
//! subdirectories of `tests/`). Consumers wire the fixture in
//! by adding, near the top of their `tests/<name>_test.rs`:
//!
//! ```ignore
//! #[path = "fixtures/mod.rs"]
//! mod fixtures;
//! ```
//!
//! The fixture is gated to dev-only code paths because every
//! module pulls in `wiremock`, `tempfile`, and `tokio` and none
//! of those should leak into the production binary.

#[cfg(test)]
mod git_origin;
#[cfg(test)]
mod github;

#[cfg(test)]
#[allow(unused_imports)]
pub use git_origin::LocalOrigin;
#[cfg(test)]
#[allow(unused_imports)]
pub use github::{Counts, MockGitHub};

//! Shared infrastructure used by every other module.
//!
//! `config`, `error`, `logging`, `validate`, `fixtures`, and `install`
//! are primitives — nothing in the daemon depends on them transitively
//! through a business module. They're the "stdio of Caduceus".

pub mod config;
pub mod error;
pub mod fixtures;
pub mod install;
pub mod logging;
pub mod validate;

// Explicit re-exports of `error` types — the canonical error type is
// `CaduceusError` and lives here. `lib.rs` re-exports it again so it
// appears at `crate::infra::error::*` for downstream consumers.
pub use crate::infra::error::{CaduceusError, CaduceusResult};

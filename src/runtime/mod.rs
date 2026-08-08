//! Runtime lifecycle hooks that the daemon invokes at fixed
//! checkpoints. This module owns the audit hook that enforces
//! the "never auto-merge" contract (FINAL-001; see `src/state/checkpoints.rs`).

pub mod audit;

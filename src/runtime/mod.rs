//! Runtime lifecycle hooks that the daemon invokes at fixed
//! checkpoints. This module owns the audit hook that enforces
//! the "never auto-merge" contract (Task 4.3, CONTRACTS.md
//! FINAL-001).

pub mod audit;

//! Legacy state migration. Driven by `caduceus migrate-state --from PATH`.
//! Task 9.1 fills in the actual mapping; the stub defines the entry point.

#![allow(dead_code)]

use std::path::Path;

use crate::error::CaduceusResult;

/// Outcome of a migration attempt.
#[derive(Debug)]
pub struct MigrationReport {
    pub entries_migrated: u64,
    pub entries_skipped: u64,
}

/// Migrate from the supplied legacy state directory.
pub fn run(_from: &Path, _dry_run: bool) -> CaduceusResult<MigrationReport> {
    Ok(MigrationReport {
        entries_migrated: 0,
        entries_skipped: 0,
    })
}

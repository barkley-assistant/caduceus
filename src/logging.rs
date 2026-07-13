//! Structured logging: tracing-subscriber setup. Task 1.4 implements the
//! safe initialisation; the stub keeps the symbol reachable.

#![allow(dead_code)]

use crate::error::CaduceusResult;

/// Initialise the global tracing subscriber. The full implementation is
/// in Task 1.4.
pub fn init() -> CaduceusResult<()> {
    Ok(())
}

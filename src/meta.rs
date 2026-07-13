//! `<state_dir>/state_meta.json` schema and writer. Phase 7 owns the
//! implementation; the stub defines the typed surface.

#![allow(dead_code)]

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::CaduceusResult;

/// Persisted tick metadata. Field semantics are pinned by `CONTRACTS.md`
/// under "State metadata and status".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateMeta {
    pub schema_version: u32,
    pub tick_start: Option<DateTime<Utc>>,
    pub tick_finish: Option<DateTime<Utc>>,
    pub tick_outcome: Option<String>,
    pub last_http_status: Option<u16>,
    pub next_allowed_poll: Option<DateTime<Utc>>,
    pub rate_limit: Option<RateLimitState>,
    pub last_error: Option<String>,
    pub reap_count: u64,
    pub reap_at: Option<DateTime<Utc>>,
    pub version: String,
}

/// Persisted GitHub rate-limit observation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RateLimitState {
    pub limit: Option<u32>,
    pub remaining: u32,
    pub reset_at: DateTime<Utc>,
}

/// Load the latest persisted metadata.
pub async fn load(_state_dir: &Path) -> CaduceusResult<Option<StateMeta>> {
    Ok(None)
}

/// Persist a new metadata snapshot via the same atomic writer as queue state.
pub async fn save(_state_dir: &Path, _meta: &StateMeta) -> CaduceusResult<()> {
    Ok(())
}

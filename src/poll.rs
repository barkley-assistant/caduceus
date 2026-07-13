//! Polling: discover watched repos and merge labeled open issues into the
//! queue. Phase 2 implements the body; the stub defines the typed surface.

#![allow(dead_code)]

use chrono::{DateTime, Utc};

use crate::error::CaduceusResult;
use crate::issue::IssueKey;

/// One open issue surfaced by polling.
#[derive(Debug)]
pub struct PollHit {
    pub key: IssueKey,
    pub updated_at: DateTime<Utc>,
    pub title: String,
    pub labels: Vec<String>,
    pub ambiguous: bool,
}

/// Poll for label `ticket_label_code`.
pub async fn poll_code(_now: DateTime<Utc>) -> CaduceusResult<Vec<PollHit>> {
    Ok(Vec::new())
}

/// Poll for label `ticket_label_investigation`.
pub async fn poll_investigation(_now: DateTime<Utc>) -> CaduceusResult<Vec<PollHit>> {
    Ok(Vec::new())
}

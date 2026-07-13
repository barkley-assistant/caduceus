//! Typed GitHub API surface. `HttpClient`, repositories endpoint, issues
//! endpoint, and ETag-aware conditional GET are owned here. Phase 2 fills
//! them in; the stub keeps the symbol set reachable for compile tests.

#![allow(dead_code)]

use std::sync::Arc;

use crate::error::CaduceusResult;
use crate::issue::IssueKey;

/// HTTP client wrapper carrying the resolved token and the cached HTTP
/// state. Constructed once per tick.
#[derive(Debug)]
pub struct HttpClient {
    pub base_url: Arc<str>,
}

impl HttpClient {
    pub fn new(base_url: impl Into<Arc<str>>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

/// Lookup result for an issue summary by key.
#[derive(Debug)]
pub struct IssueSummary {
    pub key: IssueKey,
    pub title: String,
    pub labels: Vec<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub async fn fetch_issue(_client: &HttpClient, _key: &IssueKey) -> CaduceusResult<IssueSummary> {
    Ok(IssueSummary {
        key: IssueKey {
            owner: String::new(),
            repo: String::new(),
            number: 0,
        },
        title: String::new(),
        labels: Vec::new(),
        updated_at: chrono::Utc::now(),
    })
}

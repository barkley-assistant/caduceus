//! Per-repository exclusion map.
//!
//! [`RepoExclusionMap`] wraps a `Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>`
//! and provides a single `get_or_init` method. The returned `Arc<tokio::sync::Mutex<()>>`
//! is shared across concurrent calls for the same repo key, so only one worker per
//! repository can hold the inner lock at a time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Map of repo keys to exclusion mutexes. Every admission for the
/// same repo acquires the same `Arc<tokio::sync::Mutex<()>>`, guaranteeing
/// serialised mutation per repository.
pub struct RepoExclusionMap {
    inner: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl RepoExclusionMap {
    /// Create an empty exclusion map.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Get or initialise the per-repo exclusion mutex for `repo_key`.
    /// The returned `Arc` is shared — concurrent calls with the same
    /// key see the same underlying mutex.
    pub fn get_or_init(&self, repo_key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.inner.lock().expect("exclusion map lock");
        map.entry(repo_key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

impl Default for RepoExclusionMap {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for RepoExclusionMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoExclusionMap")
            .field("len", &self.inner.lock().expect("exclusion map lock").len())
            .finish()
    }
}

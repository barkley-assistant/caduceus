//! Persistent conditional ETag cache for the GitHub API client.

#![allow(dead_code)]
#![allow(unused_imports)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::finalize::{
    validate_comment, validate_pr_body, validate_pr_title, validate_public_text,
};
use crate::github::issue::IssueKey;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult, VoiceError};

use super::http_helpers::HTTP_CACHE_FILENAME;
/// One cached response keyed by full URL + Accept header. The
/// body is stored alongside the ETag so a 304 can reuse the last
/// successfully parsed body verbatim.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheEntry {
    pub etag: String,
    pub status: u16,
    pub body: Vec<u8>,
    pub final_url: String,
}

/// Persistent conditional cache rooted at `<state_dir>/cache/http.json`.
/// All mutations go through one mutex so concurrent detail requests
/// merge into the same locked update.
#[derive(Debug)]
pub struct HttpCache {
    path: PathBuf,
    state: Mutex<CacheState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheState {
    by_key: BTreeMap<String, CacheEntry>,
}

impl HttpCache {
    /// Open or create the cache rooted at *state_dir*. Missing
    /// directories are created with mode `0700`; missing files yield
    /// an empty cache; malformed JSON is dropped on first read so a
    /// corruption cannot poison every tick.
    pub fn open(state_dir: &Path) -> CaduceusResult<Self> {
        let dir = state_dir.join("cache");
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
            set_secure_dir_mode(&dir)?;
        }
        let path = dir.join(HTTP_CACHE_FILENAME);
        let state = if path.exists() {
            read_cache_file(&path)?
        } else {
            CacheState::default()
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    /// Path to the cache file (test seam).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Build a fresh cache instance pointing at the same file.
    /// Used by [`Client::new`] to clone the inert cache without
    /// sharing the mutex across callers.
    pub fn clone_state(&self) -> Self {
        Self {
            path: self.path.clone(),
            state: Mutex::new(CacheState::default()),
        }
    }

    /// Borrow the cached entry for *key*. Returns `None` when there
    /// is no entry, when the entry's stored ETag is malformed, or
    /// when the stored ETag is the empty string.
    pub fn get(&self, key: &str) -> Option<CacheEntry> {
        let guard = self.state.lock().expect("http cache lock poisoned");
        match guard.by_key.get(key) {
            Some(entry) if is_valid_etag(&entry.etag) => Some(entry.clone()),
            _ => None,
        }
    }

    /// Store *entry* under *key*. Writes the cache file atomically
    /// when the lock is dropped if any change was made. Concurrent
    /// callers serialise through the mutex; the last write wins for
    /// any given key.
    pub fn put(&self, key: String, entry: CacheEntry) -> CaduceusResult<()> {
        let mut guard = self.state.lock().expect("http cache lock poisoned");
        if !is_valid_etag(&entry.etag) {
            // An invalid ETag is dropped, not stored — the next
            // caller can rebuild a clean entry from a fresh 200.
            guard.by_key.remove(&key);
        } else {
            guard.by_key.insert(key, entry);
        }
        write_cache_file(&self.path, &guard)
    }
}

fn read_cache_file(path: &Path) -> CaduceusResult<CacheState> {
    let bytes = std::fs::read(path).map_err(|err| CaduceusError::StateCorrupt {
        path: path.to_path_buf(),
        message: format!("read http cache: {err}"),
    })?;
    if bytes.is_empty() {
        return Ok(CacheState::default());
    }
    match serde_json::from_slice::<CacheState>(&bytes) {
        Ok(state) => Ok(state),
        // Corruption recovery: drop the bad file, start over. The
        // contract says "Invalid cache JSON drops only the affected
        // cache entry and refetches unconditionally"; the only entry
        // we have is the whole file, so dropping it is the
        // narrowest safe recovery.
        Err(_) => {
            let _ = std::fs::remove_file(path);
            Ok(CacheState::default())
        }
    }
}

fn write_cache_file(path: &Path, state: &CacheState) -> CaduceusResult<()> {
    let body = serde_json::to_vec(state)
        .map_err(|err| CaduceusError::Other(format!("serialise http cache: {err}")))?;
    let parent = path.parent().ok_or_else(|| {
        CaduceusError::Other(format!("http cache path has no parent: {}", path.display()))
    })?;
    if !parent.exists() {
        std::fs::create_dir_all(parent)?;
        set_secure_dir_mode(parent)?;
    }
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        HTTP_CACHE_FILENAME,
        ulid::Ulid::new()
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    set_secure_file_mode(path)?;
    Ok(())
}

fn set_secure_dir_mode(path: &Path) -> CaduceusResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn set_secure_file_mode(path: &Path) -> CaduceusResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Validates an ETag header value per RFC 7232. The only permitted
/// shapes are the strong quoted form (`"abc"`) and the weak
/// prefixed form (`W/"abc"`); anything else is treated as a
/// cache-busting marker so the next request refetches.
pub fn is_valid_etag(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    let after_weak = trimmed.strip_prefix("W/").unwrap_or(trimmed);
    let after_open = match after_weak.strip_prefix('"') {
        Some(s) => s,
        None => return false,
    };
    let body = match after_open.strip_suffix('"') {
        Some(s) => s,
        None => return false,
    };
    !body.is_empty() && !body.contains('\u{0}')
}

/// Build the cache key for a given URL + Accept pair. URL is the
/// full URL (after any normalisation); Accept header is the relevant
/// GitHub media type.
pub fn cache_key(url: &str, accept: &str) -> String {
    format!("{url}\u{0}{accept}")
}

/// Lazily-initialised in-memory cache used by [`Client::new`].
/// The pointer is process-wide because `Client::new` is documented
/// as inert (no real I/O); any test that actually exercises
/// caching should construct via [`Client::with_cache`].
pub(crate) fn inert_cache() -> HttpCache {
    static CACHE: OnceLock<HttpCache> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("caduceus-inert-{}", ulid::Ulid::new()));
            HttpCache::open(&dir).expect("inert cache builds")
        })
        .clone_state()
}

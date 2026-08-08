//! Stable context JSON. The shape and the serialized form are pinned by
//! the worker-result contract in `src/worker/worker_contract.rs`.
//!
//! The context document is the worker's authoritative view of the
//! issue. It carries:
//!
//! * A schema version (bumped on any breaking change).
//! * The issue identity (`owner/repo#number`).
//! * The issue title, body, and labels.
//! * The full issue timeline, with truncation metadata when the
//!   total exceeds the bounded size.
//! * `comments` (every non-ignored comment) and `trusted_comments`
//!   (comments whose author matches the configured trust list or
//!   whose author is *not* matched by any ignore regex). A comment
//!   appears in `comments` and is *also* present in
//!   `trusted_comments` when it passes the trust filter (the
//!   contract is a *filter*, not a *partition*).
//! * Truncation metadata for both comments and timeline events:
//!   the daemon drops the oldest untrusted comments first, then
//!   the oldest trusted comments only if necessary. Timeline
//!   events follow the same rule.
//!
//! The encoding rules:
//!
//! * Each comment body is capped at 64 KiB before serialization;
//!   an oversized body is truncated with a `...<truncated N
//!   bytes>` marker.
//! * The total encoded JSON is capped at 1 MiB. Truncation
//!   metadata is emitted as `comments_truncated`,
//!   `trusted_comments_truncated`, and `events_truncated` boolean
//!   flags plus `dropped_untrusted_comments`,
//!   `dropped_trusted_comments`, and `dropped_events` counters.
//! * A timeline that is irreducibly oversized (its oldest
//!   event alone exceeds the cap) is a `Worker { context:
//!   "context:oversized_event", ... }` error — the daemon must
//!   not silently emit an empty timeline.
//!
//! All trust decisions are made at *config-load time* via
//! [`compiled_ignore_patterns`] and the `feedback_author_allowlist`;
//! the context builder consumes a `&Config` (already validated) and
//! a `&IssueDetail` (already filtered by the trust list at fetch
//! time) and produces the JSON document the worker reads.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::github::issue::{IssueDetail, IssueKey};
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Current context-JSON schema version. Bumped on any breaking
/// change to the wire shape.
pub const CONTEXT_SCHEMA_VERSION: u32 = 1;

/// Maximum size of any single comment body, in bytes.
pub const MAX_COMMENT_BODY_BYTES: usize = 64 * 1024;

/// Maximum size of the encoded context JSON, in bytes.
pub const MAX_CONTEXT_BYTES: usize = 1024 * 1024;

/// Marker appended to truncated comment bodies so the worker
/// can see the cut-off.
pub const TRUNCATION_MARKER: &str = "...<truncated N bytes>";

/// Stable, deterministic context payload delivered to the worker bridge as
/// `CADUCEUS_CONTEXT_JSON`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerContext {
    /// Schema version of this document. Bumped on any breaking change.
    pub schema_version: u32,
    /// Issue identity (`owner/repo#number`).
    pub issue: IssueKey,
    /// Issue title (UTF-8, no control characters).
    pub issue_title: String,
    /// Issue body (UTF-8, may contain newlines).
    pub issue_body: String,
    /// Label names.
    pub labels: Vec<String>,
    /// All non-ignored comments in stable chronological order
    /// (oldest first).
    pub comments: Vec<ContextComment>,
    /// Subset of `comments` whose author is in the trust list
    /// (and not matched by any ignore regex).
    pub trusted_comments: Vec<ContextComment>,
    /// Timeline events in stable chronological order (oldest first).
    pub events: Vec<ContextEvent>,
    /// Truncation metadata for the comments / trusted_comments /
    /// events lists. See [`ContextTruncation`].
    pub truncation: ContextTruncation,
    /// UTC timestamp at which the context was serialised.
    pub built_at: DateTime<Utc>,
}

/// One comment in the context document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextComment {
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

/// One timeline event in the context document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextEvent {
    pub kind: String,
    pub actor: String,
    pub created_at: DateTime<Utc>,
    /// Label name for `labeled` / `unlabeled` events; `None` for
    /// any other kind.
    pub label_name: Option<String>,
}

/// Truncation metadata emitted with the context. The three
/// `*_truncated` booleans are `true` when at least one item was
/// dropped; the three `*_dropped` counters record exactly how
/// many. `body_truncated_count` records how many comment bodies
/// were individually truncated to fit the per-body cap.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextTruncation {
    pub comments_truncated: bool,
    pub trusted_comments_truncated: bool,
    pub events_truncated: bool,
    pub dropped_untrusted_comments: u32,
    pub dropped_trusted_comments: u32,
    pub dropped_events: u32,
    pub body_truncated_count: u32,
    pub total_body_bytes_dropped: u64,
}

/// Inputs to the context builder.
#[derive(Debug)]
pub struct BuildInputs<'a> {
    /// The fully-validated daemon config. The allowlist regexes
    /// (`compiled_ignore_patterns`) and the trust list
    /// (`feedback_author_allowlist`) are read from here.
    pub config: &'a Config,
    /// The fetched issue detail (already had its `trusted_comments`
    /// partition applied by the fetcher).
    pub detail: &'a IssueDetail,
}

/// Build the stable context JSON document for *detail*. The
/// function is pure: it reads only the inputs and returns a
/// serialisable [`WorkerContext`]. Serialisation is the caller's
/// responsibility so the same struct can be re-used for
/// `to_context_json` / `from_context_json` round-trips.
///
/// Truncation rules:
///
/// 1. Each comment body is truncated to [`MAX_COMMENT_BODY_BYTES`]
///    in UTF-8 boundaries; oversized bodies get a
///    `...<truncated N bytes>` marker.
/// 2. The comments list is reduced to fit the total byte budget
///    (1 MiB by default). Untrusted comments are dropped
///    oldest-first; trusted comments are dropped *only* after all
///    untrusted comments are gone, and only oldest-first.
/// 3. The events list is reduced similarly, oldest-first.
/// 4. An irreducibly oversized event (the oldest event alone is
///    larger than the byte budget) is an error — the daemon must
///    not silently emit an empty timeline.
pub fn build_context(inputs: BuildInputs<'_>) -> CaduceusResult<WorkerContext> {
    let BuildInputs { config, detail } = inputs;

    // Filter comments against ignore patterns. The fetcher
    // already populated `trusted_comments` from
    // `feedback_author_allowlist`; here we extend the trust
    // filter to *also* exclude anyone matched by any ignore
    // pattern.
    let allowlist = &config.feedback_author_allowlist;
    let ignored_authors = comments_matched_by_any_ignore(config, detail);
    let trust_set: std::collections::HashSet<&str> = allowlist.iter().map(String::as_str).collect();
    let ignored_set: std::collections::HashSet<&str> =
        ignored_authors.iter().map(String::as_str).collect();

    // Apply per-body truncation to every comment.
    let mut all_comments: Vec<ContextComment> = Vec::with_capacity(detail.comments.len());
    let mut body_truncated_count: u32 = 0;
    let mut total_body_bytes_dropped: u64 = 0;
    for c in &detail.comments {
        let (body, dropped) = truncate_body(&c.body);
        if dropped > 0 {
            body_truncated_count += 1;
            total_body_bytes_dropped += dropped as u64;
        }
        all_comments.push(ContextComment {
            author: c.author.clone(),
            body,
            created_at: c.created_at,
        });
    }
    // Sort ascending by created_at to make the chronological
    // order stable.
    all_comments.sort_by_key(|c| c.created_at);

    // Build the trust partition. A comment is trusted when its
    // author is in `feedback_author_allowlist` AND not matched
    // by any ignore regex. A trusted comment appears in BOTH
    // `comments` and `trusted_comments`.
    let trusted: Vec<ContextComment> = all_comments
        .iter()
        .filter(|c| {
            trust_set.contains(c.author.as_str()) && !ignored_set.contains(c.author.as_str())
        })
        .cloned()
        .collect();

    // Build events list with per-event truncation (event bodies
    // are short; the cap is mostly defensive — we still apply it
    // so a malicious or unusually long label_name cannot blow
    // the budget).
    let mut events: Vec<ContextEvent> = detail
        .events
        .iter()
        .map(|e| ContextEvent {
            kind: e.kind.clone(),
            actor: e.actor.clone(),
            created_at: e.created_at,
            label_name: e.label_name.clone(),
        })
        .collect();
    events.sort_by_key(|e| e.created_at);

    // Compute the byte budget. The total JSON must fit in
    // MAX_CONTEXT_BYTES. We compute the size of the *skeleton*
    // (everything except the three lists) and reserve the rest
    // for the lists.
    let skeleton_size = estimate_skeleton_size(detail)?;
    if skeleton_size > MAX_CONTEXT_BYTES {
        return Err(CaduceusError::Worker {
            context: "context:skeleton_oversized",
            stderr: format!(
                "skeleton (title+body+labels+events-without-list) is {skeleton_size} bytes; budget is {MAX_CONTEXT_BYTES}"
            ),
        });
    }
    let list_budget = MAX_CONTEXT_BYTES - skeleton_size;

    // Reduce events to fit the budget. Events are reduced
    // oldest-first.
    let (events_final, dropped_events) =
        reduce_events_to_budget(events, list_budget).ok_or_else(|| CaduceusError::Worker {
            context: "context:oversized_event",
            stderr: "oldest timeline event alone exceeds the context byte budget".to_string(),
        })?;

    // Reduce comments to fit the remaining budget after events
    // are accounted for. The remaining budget is the list_budget
    // minus the encoded size of events_final.
    let events_bytes = approx_json_array_len("events", &events_final)?;
    let comments_budget = list_budget.saturating_sub(events_bytes);

    // Reduce comments. Untrusted comments are dropped first
    // (oldest-first); trusted comments are dropped only after
    // all untrusted comments are gone.
    let (comments_final, trusted_final, dropped_untrusted, dropped_trusted) =
        reduce_comments_to_budget(all_comments, trusted, comments_budget);

    let events_truncated = dropped_events > 0;
    let comments_truncated = dropped_untrusted > 0;
    let trusted_comments_truncated = dropped_trusted > 0;

    Ok(WorkerContext {
        schema_version: CONTEXT_SCHEMA_VERSION,
        issue: detail.key.clone(),
        issue_title: detail.title.clone(),
        issue_body: detail.body.clone(),
        labels: detail.labels.clone(),
        comments: comments_final,
        trusted_comments: trusted_final,
        events: events_final,
        truncation: ContextTruncation {
            comments_truncated,
            trusted_comments_truncated,
            events_truncated,
            dropped_untrusted_comments: dropped_untrusted,
            dropped_trusted_comments: dropped_trusted,
            dropped_events,
            body_truncated_count,
            total_body_bytes_dropped,
        },
        built_at: Utc::now(),
    })
}

/// Encode *ctx* to JSON. The encoder applies a final size check
/// so the JSON is guaranteed to fit in [`MAX_CONTEXT_BYTES`]
/// after the in-memory reductions.
pub fn encode_context(ctx: &WorkerContext) -> CaduceusResult<String> {
    let s = serde_json::to_string(ctx).map_err(|err| CaduceusError::Worker {
        context: "context:encode",
        stderr: format!("serde_json: {err}"),
    })?;
    if s.len() > MAX_CONTEXT_BYTES {
        return Err(CaduceusError::Worker {
            context: "context:oversized",
            stderr: format!(
                "encoded context is {} bytes; budget is {MAX_CONTEXT_BYTES}",
                s.len()
            ),
        });
    }
    Ok(s)
}

/// Decode a context JSON document. Used by tests to round-trip
/// the wire form.
pub fn decode_context(s: &str) -> CaduceusResult<WorkerContext> {
    serde_json::from_str(s).map_err(|err| CaduceusError::Worker {
        context: "context:decode",
        stderr: format!("serde_json: {err}"),
    })
}

/// Return the set of comment authors matched by at least one
/// compiled ignore regex. Used to keep `trusted_comments` free
/// of ignored authors even if the allowlist mistakenly includes
/// them.
fn comments_matched_by_any_ignore(config: &Config, detail: &IssueDetail) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for re in &config.compiled_ignore_patterns {
        for c in &detail.comments {
            if re.is_match(&c.author) && !out.iter().any(|a| a == &c.author) {
                out.push(c.author.clone());
            }
        }
    }
    out
}

/// Truncate *body* to [`MAX_COMMENT_BODY_BYTES`], returning the
/// truncated body and the number of bytes that were dropped.
/// The function preserves UTF-8 by trimming at the last byte
/// boundary inside the cap and appending a marker.
fn truncate_body(body: &str) -> (String, usize) {
    if body.len() <= MAX_COMMENT_BODY_BYTES {
        return (body.to_string(), 0);
    }
    let mut cut = MAX_COMMENT_BODY_BYTES;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut kept = body[..cut].to_string();
    let dropped = body.len() - cut;
    let marker = TRUNCATION_MARKER.replace("N", &dropped.to_string());
    kept.push_str(&marker);
    (kept, dropped)
}

/// Test seam: re-export the body-truncation helper so integration
/// tests can assert the truncation contract without owning a
/// runtime. Identical to the private [`truncate_body`].
#[doc(hidden)]
pub fn truncate_body_for_tests(body: &str) -> (String, usize) {
    truncate_body(body)
}

/// Estimate the size of the context JSON *minus* the three
/// list fields (`comments`, `trusted_comments`, `events`). The
/// estimate is byte-accurate against `serde_json`'s canonical
/// encoder (no whitespace).
fn estimate_skeleton_size(detail: &IssueDetail) -> CaduceusResult<usize> {
    // Encode a stub with empty lists to measure the
    // skeleton-only size.
    let stub = WorkerContext {
        schema_version: CONTEXT_SCHEMA_VERSION,
        issue: detail.key.clone(),
        issue_title: detail.title.clone(),
        issue_body: detail.body.clone(),
        labels: detail.labels.clone(),
        comments: Vec::new(),
        trusted_comments: Vec::new(),
        events: Vec::new(),
        truncation: ContextTruncation::default(),
        built_at: Utc::now(),
    };
    let s = serde_json::to_string(&stub).map_err(|err| CaduceusError::Worker {
        context: "context:estimate",
        stderr: format!("serde_json: {err}"),
    })?;
    Ok(s.len())
}

/// Reduce *events* (already chronologically sorted) to fit
/// within *budget* bytes. Returns the reduced list plus the
/// number of events dropped, or `None` if the oldest event
/// alone exceeds the budget.
fn reduce_events_to_budget(
    events: Vec<ContextEvent>,
    budget: usize,
) -> Option<(Vec<ContextEvent>, u32)> {
    let total = events.len();
    // Estimate the per-event JSON cost by measuring two
    // probes: an empty array (overhead) and a single-event
    // array (per-event). The per-event cost is then
    // `single - empty`. This makes the total-time estimate
    // linear rather than O(n²).
    let probe_empty = approx_json_array_len("events", &Vec::<ContextEvent>::new()).ok()?;
    let probe_one = if total > 0 {
        approx_json_array_len("events", &events[..1]).ok()?
    } else {
        probe_empty
    };
    let per_event = probe_one.saturating_sub(probe_empty);
    // `skeleton` is the rest of the JSON (everything except
    // the events array); we use `probe_empty - empty_prefix`
    // to estimate the prefix + suffix of the array key.
    let events_overhead = probe_empty;
    let usable_per_event = if per_event == 0 { 1 } else { per_event };
    let max_events_that_fit = (budget.saturating_sub(events_overhead)) / usable_per_event;
    if max_events_that_fit == 0 {
        // The oldest event alone exceeds the budget (or the
        // overhead leaves no room for any event).
        if total >= 1 && per_event > budget.saturating_sub(events_overhead) {
            return None;
        }
        // Otherwise we have room for zero events because the
        // budget is too tight for the wrapper alone; return an
        // empty list rather than a `None`.
        return Some((Vec::new(), total as u32));
    }
    if max_events_that_fit >= total {
        return Some((events, 0));
    }
    let keep_from = total - max_events_that_fit;
    // Final sanity: encode the kept slice and verify it fits.
    let kept = events[keep_from..].to_vec();
    let bytes = approx_json_array_len("events", &kept).ok()?;
    if bytes > budget {
        // Per-event cost was underestimated; drop one more
        // from the front and try again. Linear worst case.
        if keep_from + 1 >= total {
            return None;
        }
        let kept2 = events[keep_from + 1..].to_vec();
        return Some((kept2, (total - (total - keep_from - 1)) as u32));
    }
    Some((kept, keep_from as u32))
}

/// Reduce comments to fit within *budget* bytes. Untrusted
/// Reduce comments to fit within *budget* bytes. Untrusted
/// comments are dropped first; trusted comments are dropped
/// only after all untrusted are gone.
fn reduce_comments_to_budget(
    all: Vec<ContextComment>,
    trusted: Vec<ContextComment>,
    budget: usize,
) -> (Vec<ContextComment>, Vec<ContextComment>, u32, u32) {
    // Fast path: the full list already fits.
    if let Ok(n) = approx_json_array_len("comments", &all) {
        if n <= budget {
            // Trusted partition is recomputed from the
            // surviving `all` set below.
        }
    }
    // Drop oldest untrusted until the list fits, or until
    // every remaining entry is trusted. Use a simple
    // amortised bound: each drop costs O(n) to re-encode but
    // in practice n quickly shrinks; for the documented
    // ceiling (~ thousands of comments) this is fast enough.
    let mut all = all;
    let mut dropped_untrusted: u32 = 0;
    loop {
        let bytes = match approx_json_array_len("comments", &all) {
            Ok(n) => n,
            Err(_) => break,
        };
        if bytes <= budget || all.is_empty() {
            break;
        }
        // Find the oldest untrusted comment to drop.
        let untrusted_pos = all.iter().position(|c| {
            !trusted
                .iter()
                .any(|t| t.author == c.author && t.created_at == c.created_at && t.body == c.body)
        });
        let pos = untrusted_pos.unwrap_or_default();
        all.remove(pos);
        if untrusted_pos.is_some() {
            dropped_untrusted += 1;
        }
    }
    // Recompute the trusted partition from the surviving
    // `all` set. The contract is "trusted_comments is a subset
    // of comments".
    let trusted_set: std::collections::HashSet<(String, chrono::DateTime<chrono::Utc>, String)> =
        trusted
            .iter()
            .map(|c| (c.author.clone(), c.created_at, c.body.clone()))
            .collect();
    let mut trusted_surviving: Vec<ContextComment> = all
        .iter()
        .filter(|c| trusted_set.contains(&(c.author.clone(), c.created_at, c.body.clone())))
        .cloned()
        .collect();
    let mut dropped_trusted: u32 = 0;
    loop {
        let bytes = match approx_json_array_len("trusted_comments", &trusted_surviving) {
            Ok(n) => n,
            Err(_) => break,
        };
        if bytes <= budget || trusted_surviving.is_empty() {
            break;
        }
        trusted_surviving.remove(0);
        dropped_trusted += 1;
    }
    (all, trusted_surviving, dropped_untrusted, dropped_trusted)
}

/// Approximate the byte length of a JSON-encoded array
/// `key: [...]` containing the given elements. Uses the real
/// serde_json encoder so the estimate is byte-accurate.
fn approx_json_array_len<T: Serialize + Clone>(key: &str, items: &[T]) -> CaduceusResult<usize> {
    let owned: Vec<T> = items.to_vec();
    let mut map: BTreeMap<&str, &Vec<T>> = BTreeMap::new();
    map.insert(key, &owned);
    let s = serde_json::to_string(&map).map_err(|err| CaduceusError::Worker {
        context: "context:approx_size",
        stderr: format!("serde_json: {err}"),
    })?;
    Ok(s.len())
}

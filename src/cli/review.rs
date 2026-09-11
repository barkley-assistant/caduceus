//! `caduceus review` — review observability CLI (issue #318, DAR §13).
//!
//! Three read-only subcommands over the review stores (queue, per-PR
//! state, history) on BOTH state backends:
//!
//! - `status [<owner/repo>]` — aggregate phase counts plus one line
//!   per entry, optionally filtered to one repository;
//! - `list` — every review queue entry as a table;
//! - `show <owner/repo> <pr>` — full detail plus the run history for
//!   one PR, parsing each `result_json` document defensively.
//!
//! Backend selection mirrors the daemon (`config.state_backend ==
//! "sqlite"`), not the JSON-only `queue` CLI. All three subcommands
//! accept `--json`; the envelope is `review/1.0`. The subcommands
//! never take the daemon lock and never write state.

use std::collections::BTreeMap;
use std::path::Path;

use clap::Subcommand;
use serde::Serialize;

use caduceus::config::Config;
use caduceus::error::{CaduceusError, CaduceusResult};
use caduceus::review::{
    ExecutionStatus, PublicationState, RepositoryId, ReviewResult, ReviewState, Verdict,
    REVIEW_SCHEMA_VERSION,
};
use caduceus::state::review::{ReviewHistoryRow, ReviewPhase, ReviewQueueEntry, ReviewStore};

/// Schema version of the `review` JSON envelope emitted by
/// `review status|list|show --json`. Bumped when the envelope shape
/// changes so consumers can detect the version. Distinct from the
/// `queue/1.0` and `status` schemas — the review envelope versions
/// independently.
const REVIEW_CLI_SCHEMA_VERSION: &str = "review/1.0";

/// Nested subcommand for `caduceus review`.
#[derive(Debug, Subcommand)]
pub enum ReviewAction {
    /// Report review queue state (aggregate phase counts + per-entry rows).
    Status {
        /// Optional `owner/repo` filter.
        repo: Option<String>,
        /// Print machine-readable JSON instead of the human summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List every review queue entry.
    List {
        /// Print machine-readable JSON instead of the human summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show full detail + history for one `(repo, pr)`.
    Show {
        /// `owner/repo` repository.
        repo: String,
        /// Pull request number.
        pr: u64,
        /// Print machine-readable JSON instead of the human summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

/// One §13 per-row view: the queue entry joined with its `(repo, pr)`
/// state row and, for terminal rows, the latest same-generation
/// history row's execution status and verdict (the entry's own run
/// outcome — the per-PR last verdict is only a fallback for entries
/// with no run, issue #387). The same field set is rendered by
/// the human and JSON paths (`--json` only changes the rendering).
#[derive(Serialize)]
struct ReviewRow {
    repo: String,
    pr: u64,
    base_sha: String,
    head_sha: String,
    merge_base: String,
    review_state: String,
    run_id: Option<String>,
    review_generation: u64,
    execution_attempts: u32,
    execution_status: Option<String>,
    /// Entry's own run verdict (latest same-generation history row);
    /// falls back to the PR's last verdict only when the entry has
    /// no completed run (issue #387).
    verdict: Option<String>,
    last_error: Option<String>,
    reviewed_at: Option<String>,
    publication_state: String,
    publication_attempt_count: u32,
    next_publication_attempt: Option<String>,
}

/// Defensively parsed fields of one history row's `result_json`
/// document (DAR §4.3). Old schema versions surface as raw
/// `result_json` + a `parse_error`; they are never back-migrated.
#[derive(Serialize)]
struct HistorySummary {
    run_id: String,
    head_sha: String,
    review_generation: u64,
    completed_at: String,
    status: Option<String>,
    verdict: Option<String>,
    summary: Option<String>,
    result_json: String,
    parse_error: Option<String>,
}

/// Dispatch `caduceus review <action>` using the same env-aware config
/// resolution the other CLI subcommands use.
pub fn run(action: ReviewAction) -> CaduceusResult<()> {
    let config = super::resolve_queue_config()?;
    match action {
        ReviewAction::Status { repo, json } => run_review_status(&config, repo.as_deref(), json),
        ReviewAction::List { json } => run_review_list(&config, json),
        ReviewAction::Show { repo, pr, json } => run_review_show(&config, &repo, pr, json),
    }
}

/// `caduceus review status [<owner/repo>] [--json]` — aggregate phase
/// counts over the (optionally repo-filtered) review queue plus one
/// per-entry row.
fn run_review_status(config: &Config, repo_filter: Option<&str>, json: bool) -> CaduceusResult<()> {
    let store = open_store(config, json)?;
    let queue = store.review_queue_snapshot()?;
    let filter = match repo_filter {
        Some(text) => Some(parse_repo(text)?),
        None => None,
    };
    let entries: Vec<&ReviewQueueEntry> = queue
        .entries
        .values()
        .filter(|entry| match &filter {
            Some(repo) => {
                entry
                    .target
                    .repository
                    .owner
                    .eq_ignore_ascii_case(&repo.owner)
                    && entry
                        .target
                        .repository
                        .repo
                        .eq_ignore_ascii_case(&repo.repo)
            }
            None => true,
        })
        .collect();

    // Phase counts over the filtered set, zero-filled for every phase
    // so the JSON shape is stable across runs.
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for phase in [
        ReviewPhase::Queued,
        ReviewPhase::InProgress,
        ReviewPhase::Done,
        ReviewPhase::Failed,
        ReviewPhase::Skipped,
        ReviewPhase::NeedsAttention,
    ] {
        counts.insert(phase.as_str().to_string(), 0);
    }
    for entry in &entries {
        *counts.entry(entry.phase.as_str().to_string()).or_insert(0) += 1;
    }

    let rows = rows_for(&store, &entries)?;
    if json {
        print_review_json(
            &config.state_dir,
            serde_json::json!({ "counts": counts, "entries": rows }),
        )?;
        return Ok(());
    }
    println!("{}", render_status_human(config, &counts, &entries, &rows));
    Ok(())
}

/// `caduceus review list [--json]` — every review queue entry as a
/// table (JSON: the full §13 per-row field set).
fn run_review_list(config: &Config, json: bool) -> CaduceusResult<()> {
    let store = open_store(config, json)?;
    let queue = store.review_queue_snapshot()?;
    let entries: Vec<&ReviewQueueEntry> = queue.entries.values().collect();
    let rows = rows_for(&store, &entries)?;
    if json {
        print_review_json(&config.state_dir, serde_json::json!({ "entries": rows }))?;
        return Ok(());
    }
    println!("{}", render_list_human(&queue.entries, &rows));
    Ok(())
}

/// `caduceus review show <owner/repo> <pr> [--json]` — full detail +
/// run history for one PR. The history rows' `result_json` documents
/// are parsed defensively (see [`parse_result_document`]).
fn run_review_show(config: &Config, repo_text: &str, pr: u64, json: bool) -> CaduceusResult<()> {
    let repo = parse_repo(repo_text)?;
    let store = open_store(config, json)?;
    let queue = store.review_queue_snapshot()?;
    let entry = queue
        .entries
        .values()
        .find(|entry| {
            entry.target.pull_request == pr
                && entry
                    .target
                    .repository
                    .owner
                    .eq_ignore_ascii_case(&repo.owner)
                && entry
                    .target
                    .repository
                    .repo
                    .eq_ignore_ascii_case(&repo.repo)
        })
        .cloned();
    let Some(entry) = entry else {
        let err = CaduceusError::Queue {
            context: "review show",
            stderr: format!("no entry for {}#{pr}", repo.full_name()),
        };
        if json {
            print_review_json_with_diagnostic(
                &config.state_dir,
                serde_json::Value::Null,
                Some("no_entry"),
            )?;
        }
        return Err(err);
    };
    let state = store.review_state(&entry.target.repository, pr)?;
    let history = store.history_for_pull_request(&entry.target.repository, pr)?;
    let row = row_for(&entry, state.as_ref(), &history);
    let history_summaries: Vec<HistorySummary> = history.iter().map(history_summary).collect();
    if json {
        print_review_json(
            &config.state_dir,
            serde_json::json!({ "entry": row, "history": history_summaries }),
        )?;
        return Ok(());
    }
    print!("{}", render_show_human(&entry, &row, &history_summaries));
    Ok(())
}

/// Open the review store on the backend the daemon would use
/// (`config.state_backend == "sqlite"`), mirroring the daemon's
/// `src/daemon/tick/mod.rs` branch rather than the JSON-only `queue`
/// CLI. On a corrupt store the `--json` path still emits the
/// `review/1.0` envelope with `diagnostic: "corrupt_review_state"`
/// before propagating the error.
fn open_store(config: &Config, json: bool) -> CaduceusResult<ReviewStore> {
    let result = if config.state_backend == "sqlite" {
        ReviewStore::open_sqlite(&config.state_dir)
    } else {
        ReviewStore::open(&config.state_dir)
    };
    match result {
        Ok(store) => Ok(store),
        Err(err) => {
            if json {
                print_review_json_with_diagnostic(
                    &config.state_dir,
                    serde_json::Value::Null,
                    Some("corrupt_review_state"),
                )?;
            }
            Err(err)
        }
    }
}

/// Build the §13 per-row views for the given queue entries.
fn rows_for(store: &ReviewStore, entries: &[&ReviewQueueEntry]) -> CaduceusResult<Vec<ReviewRow>> {
    entries
        .iter()
        .map(|entry| {
            let state = store.review_state(&entry.target.repository, entry.target.pull_request)?;
            let history = store
                .history_for_pull_request(&entry.target.repository, entry.target.pull_request)?;
            Ok(row_for(entry, state.as_ref(), &history))
        })
        .collect()
}

/// Join one queue entry with its state row and history.
fn row_for(
    entry: &ReviewQueueEntry,
    state: Option<&ReviewState>,
    history: &[ReviewHistoryRow],
) -> ReviewRow {
    ReviewRow {
        repo: canonical_repo(&entry.target.repository),
        pr: entry.target.pull_request,
        base_sha: entry.target.base_sha.clone(),
        head_sha: entry.target.head_sha.clone(),
        merge_base: entry.target.merge_base.clone(),
        review_state: entry.phase.as_str().to_string(),
        run_id: entry
            .last_run_id
            .clone()
            .or_else(|| state.and_then(|s| s.last_run_id.clone())),
        review_generation: entry.review_generation,
        execution_attempts: entry.attempts,
        execution_status: derive_execution_status(entry, history),
        verdict: derive_verdict(entry, history, state),
        last_error: entry.last_error.clone(),
        reviewed_at: state
            .and_then(|s| s.last_reviewed_at)
            .map(|t| t.to_rfc3339()),
        publication_state: state
            .map(|s| publication_state_label(s.publication_state).to_string())
            .unwrap_or_else(|| "pending".to_string()),
        publication_attempt_count: state.map(|s| s.publication_attempt_count).unwrap_or(0),
        next_publication_attempt: state
            .and_then(|s| s.next_publish_at)
            .map(|t| t.to_rfc3339()),
    }
}

/// Execution status is a presentation field, not a stored column: it
/// is derived from the latest same-generation history row's durable
/// `ReviewResult.status` once the run completed. Active and
/// non-terminal rows (no history row for their generation) carry
/// `None`. A `failed` verdict run is still `success` here —
/// `status` describes execution, `verdict` describes the outcome
/// (DAR §8: the two never conflate).
fn derive_execution_status(
    entry: &ReviewQueueEntry,
    history: &[ReviewHistoryRow],
) -> Option<String> {
    latest_same_generation_row(entry, history)
        .and_then(|row| parse_result_document(&row.result_json).0)
}

/// The history row that owns this entry's outcome: the latest (append
/// order) row for the entry's own generation, or `None` while the
/// entry has no completed run. Generation matching implies head-SHA
/// matching: every admission bumps the per-PR generation (a same-SHA
/// re-review replaces the entry under a new one), and a run completes
/// under the claimed entry's own target (DAR §9.4).
fn latest_same_generation_row<'a>(
    entry: &ReviewQueueEntry,
    history: &'a [ReviewHistoryRow],
) -> Option<&'a ReviewHistoryRow> {
    history
        .iter()
        .rfind(|row| row.review_generation == entry.review_generation)
}

/// Per-entry verdict (issue #387): an entry that HAS a completed run
/// shows that run's verdict — the latest same-generation history
/// row's `ReviewResult.review.verdict` — never the PR's current
/// verdict, so a superseded FAIL review no longer displays as PASS.
/// When the row's document cannot be read (old schema version,
/// malformed document, or an execution failure with no review
/// payload) the defensive-parse contract (DAR §4.3) surfaces `None`;
/// the PR-level verdict is NOT substituted for a run that exists.
/// Only entries with no same-generation run (queued, in-progress)
/// fall back to the PR-level `ReviewState.last_verdict`.
fn derive_verdict(
    entry: &ReviewQueueEntry,
    history: &[ReviewHistoryRow],
    state: Option<&ReviewState>,
) -> Option<String> {
    match latest_same_generation_row(entry, history) {
        Some(row) => parse_result_document(&row.result_json).1,
        None => state
            .and_then(|s| s.last_verdict)
            .map(|v| verdict_label(v).to_string()),
    }
}

/// Render one history row's `result_json` into the defensively parsed
/// [`HistorySummary`].
fn history_summary(row: &ReviewHistoryRow) -> HistorySummary {
    let (status, verdict, summary, parse_error) = parse_result_document(&row.result_json);
    HistorySummary {
        run_id: row.review_run_id.clone(),
        head_sha: row.head_sha.clone(),
        review_generation: row.review_generation,
        completed_at: row.completed_at.to_rfc3339(),
        status,
        verdict,
        summary,
        result_json: row.result_json.clone(),
        parse_error,
    }
}

/// Parse a version-tagged `result_json` document defensively. Never
/// panics and never back-migrates: schema versions other than the
/// current one (and malformed documents) surface as
/// `(None, None, None, Some(parse_error))` with the raw document kept
/// in the caller's `result_json` field (DAR §4.3).
///
/// Returns `(status, verdict, summary, parse_error)`.
fn parse_result_document(
    json: &str,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let value: serde_json::Value = match serde_json::from_str(json) {
        Ok(value) => value,
        Err(err) => return (None, None, None, Some(format!("malformed document: {err}"))),
    };
    let schema_version = value.get("schema_version").and_then(|v| v.as_u64());
    match schema_version {
        Some(version) if version == REVIEW_SCHEMA_VERSION as u64 => {
            match serde_json::from_str::<ReviewResult>(json) {
                Ok(result) => {
                    let status = match result.status {
                        ExecutionStatus::Success => Some("success".to_string()),
                        ExecutionStatus::Failure => Some("failure".to_string()),
                    };
                    let (verdict, summary) = match result.review {
                        Some(review) => (
                            Some(verdict_label(review.verdict).to_string()),
                            Some(review.summary),
                        ),
                        None => (None, None),
                    };
                    (status, verdict, summary, None)
                }
                Err(err) => (
                    None,
                    None,
                    None,
                    Some(format!("schema v{version} parse: {err}")),
                ),
            }
        }
        Some(version) => (
            None,
            None,
            None,
            Some(format!("unsupported schema_version {version}")),
        ),
        None => (None, None, None, Some("missing schema_version".to_string())),
    }
}

/// Stable snake_case verdict label.
fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
    }
}

/// Stable snake_case publication-state label.
fn publication_state_label(state: PublicationState) -> &'static str {
    match state {
        PublicationState::Pending => "pending",
        PublicationState::Publishing => "publishing",
        PublicationState::Published => "published",
        PublicationState::FailedRetryable => "failed_retryable",
    }
}

/// Parse the `owner/repo` positional into a [`RepositoryId`]. No case
/// normalisation — matching against store rows is case-insensitive.
fn parse_repo(text: &str) -> CaduceusResult<RepositoryId> {
    let (owner, repo) = text.split_once('/').ok_or_else(|| {
        CaduceusError::Config(format!("invalid repository {text:?}: expected owner/repo"))
    })?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return Err(CaduceusError::Config(format!(
            "invalid repository {text:?}: expected owner/repo"
        )));
    }
    Ok(RepositoryId {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

/// Compact queue-entry display key: `owner/repo#pr@<sha12>` (the repo
/// is rendered in its canonical lowercase form so both backends agree).
fn display_key(entry: &ReviewQueueEntry) -> String {
    format!(
        "{}#{}@{}",
        canonical_repo(&entry.target.repository),
        entry.target.pull_request,
        short_sha(&entry.target.head_sha)
    )
}

/// Canonical lowercase `owner/repo` display form, matching the store's
/// lowercase identity keys and the issue queue CLI's display keys.
fn canonical_repo(repository: &RepositoryId) -> String {
    format!(
        "{}/{}",
        repository.owner.to_lowercase(),
        repository.repo.to_lowercase()
    )
}

/// First 12 characters of a SHA, or the whole string when shorter.
/// Char-boundary safe: head SHAs are validated as ≤64-byte non-empty
/// strings at the store layer (hex from git in practice), so a
/// multi-byte value must never panic the CLI.
fn short_sha(sha: &str) -> &str {
    if sha.len() > 12 && sha.is_char_boundary(12) {
        &sha[..12]
    } else {
        sha
    }
}

/// Human status renderer: header + phase counts + one line per entry.
fn render_status_human(
    config: &Config,
    counts: &BTreeMap<String, u64>,
    entries: &[&ReviewQueueEntry],
    rows: &[ReviewRow],
) -> String {
    let mut out = String::new();
    out.push_str("caduceus review status\n");
    out.push_str(&format!("  state dir: {}\n", config.state_dir.display()));
    out.push_str(&format!("  backend: {}\n", config.state_backend));
    let total: u64 = counts.values().sum();
    out.push_str(&format!("  review queue: {total} entries\n"));
    out.push_str("  phases:\n");
    for (label, count) in counts {
        out.push_str(&format!("    {label}: {count}\n"));
    }
    out.push_str("  entries:\n");
    for (entry, row) in entries.iter().zip(rows) {
        out.push_str(&format!(
            "    {}  phase={} attempts={} gen={} verdict={} publication={} run={}\n",
            display_key(entry),
            row.review_state,
            row.execution_attempts,
            row.review_generation,
            row.verdict.as_deref().unwrap_or("-"),
            row.publication_state,
            row.run_id.as_deref().unwrap_or("-"),
        ));
    }
    out
}

/// Human list renderer: tab-separated table (the full §13 field set
/// is available via `--json` and `show`).
fn render_list_human(entries: &BTreeMap<String, ReviewQueueEntry>, rows: &[ReviewRow]) -> String {
    if rows.is_empty() {
        return "review queue: no entries".to_string();
    }
    let mut out = String::from("key\tphase\tattempts\tgeneration\tverdict\tpublication\trun_id\n");
    for (entry, row) in entries.values().zip(rows) {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            display_key(entry),
            row.review_state,
            row.execution_attempts,
            row.review_generation,
            row.verdict.as_deref().unwrap_or("-"),
            row.publication_state,
            row.run_id.as_deref().unwrap_or("-"),
        ));
    }
    out
}

/// Human show renderer: full detail + history rows.
fn render_show_human(
    entry: &ReviewQueueEntry,
    row: &ReviewRow,
    history: &[HistorySummary],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("entry {}\n", display_key(entry)));
    out.push_str(&format!("  repo: {}\n", row.repo));
    out.push_str(&format!("  pr: {}\n", row.pr));
    out.push_str(&format!("  base_sha: {}\n", row.base_sha));
    out.push_str(&format!("  head_sha: {}\n", row.head_sha));
    out.push_str(&format!("  merge_base: {}\n", row.merge_base));
    out.push_str(&format!("  phase: {}\n", row.review_state));
    out.push_str(&format!(
        "  run_id: {}\n",
        row.run_id.as_deref().unwrap_or("-")
    ));
    out.push_str(&format!("  generation: {}\n", row.review_generation));
    out.push_str(&format!("  attempts: {}\n", row.execution_attempts));
    out.push_str(&format!(
        "  execution_status: {}\n",
        row.execution_status.as_deref().unwrap_or("-")
    ));
    out.push_str(&format!(
        "  verdict: {}\n",
        row.verdict.as_deref().unwrap_or("-")
    ));
    out.push_str(&format!(
        "  last_error: {}\n",
        row.last_error.as_deref().unwrap_or("-")
    ));
    out.push_str(&format!(
        "  reviewed_at: {}\n",
        row.reviewed_at.as_deref().unwrap_or("-")
    ));
    out.push_str(&format!("  publication_state: {}\n", row.publication_state));
    out.push_str(&format!(
        "  publication_attempt_count: {}\n",
        row.publication_attempt_count
    ));
    out.push_str(&format!(
        "  next_publication_attempt: {}\n",
        row.next_publication_attempt.as_deref().unwrap_or("-")
    ));
    out.push_str("  history:\n");
    if history.is_empty() {
        out.push_str("    (none)\n");
    }
    for entry in history {
        out.push_str(&format!(
            "    {} head={} gen={} completed={}\n",
            entry.run_id,
            short_sha(&entry.head_sha),
            entry.review_generation,
            entry.completed_at
        ));
        out.push_str(&format!(
            "      status: {}  verdict: {}\n",
            entry.status.as_deref().unwrap_or("-"),
            entry.verdict.as_deref().unwrap_or("-")
        ));
        match (&entry.summary, &entry.parse_error) {
            (Some(summary), _) => out.push_str(&format!("      summary: {summary}\n")),
            (None, Some(err)) => out.push_str(&format!("      parse_error: {err}\n")),
            (None, None) => out.push_str("      summary: -\n"),
        }
    }
    out
}

/// Print a versioned `review/1.0` JSON envelope to stdout.
fn print_review_json(state_dir: &Path, payload: serde_json::Value) -> CaduceusResult<()> {
    print_review_json_with_diagnostic(state_dir, payload, None)
}

/// Print a versioned `review/1.0` JSON envelope with an optional
/// top-level `diagnostic` string, mirroring the `queue` / `status`
/// envelope convention.
fn print_review_json_with_diagnostic(
    state_dir: &Path,
    payload: serde_json::Value,
    diagnostic: Option<&str>,
) -> CaduceusResult<()> {
    let envelope = serde_json::json!({
        "app_version": env!("CARGO_PKG_VERSION"),
        "schema": REVIEW_CLI_SCHEMA_VERSION,
        "state_dir": state_dir.display().to_string(),
        "diagnostic": diagnostic,
        "payload": payload,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&envelope).map_err(|err| {
            CaduceusError::Other(format!("serialise review JSON envelope: {err}"))
        })?
    );
    Ok(())
}

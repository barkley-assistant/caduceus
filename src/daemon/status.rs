//! `caduceus status` reporter. Reads queue + metadata + heartbeats
//! through the normal resolution chain.
//!
//! The reporter is the orchestrator's only observability surface
//! for live workers; the file format is the JSON envelope the
//! supervisor writes (see [`crate::worker::supervisor::Heartbeat`]).
//!
//! Human output has golden fixtures matching the README; the
//! JSON output carries a schema-version field. Missing state
//! is reported as a distinct nonzero diagnostic; corrupt
//! state is reported as a separate nonzero diagnostic that
//! preserves the underlying file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::readiness::{ReadinessReport, ReadinessVerdict};
use crate::state::meta::{MetaStore, StateMeta, TickOutcome};
use crate::state::queue::{Phase, QueueEntry, QueueState, StateStore, TicketType};
use crate::worker::supervisor::{read_heartbeat_record, Heartbeat};

/// Distinct diagnostic the reporter can surface. The CLI
/// surfaces a missing state directory as a friendly hint to
/// run `caduceus` at least once; corrupt state is preserved
/// and the operator is told to inspect the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusDiagnostic {
    /// No state directory exists yet. The CLI surfaces
    /// "no state yet" with a hint to run a tick.
    NoState,
    /// The metadata file is corrupt. The CLI surfaces
    /// "state_meta corrupt" without deleting the file.
    CorruptState { path: PathBuf, message: String },
    /// The queue state file is corrupt. The CLI surfaces
    /// "state.json corrupt" without deleting the file.
    CorruptQueue { path: PathBuf, message: String },
}

/// JSON-serialisable per-run summary. Used by the human
/// formatter and the `--json` output. The contract pins
/// the field set; new fields land in a new `version`.
#[derive(Clone, Debug, Serialize)]
pub struct StatusReport {
    pub version: String,
    pub state_dir: PathBuf,
    pub last_tick_started: Option<DateTime<Utc>>,
    pub last_tick_finished: Option<DateTime<Utc>>,
    pub last_outcome: Option<TickOutcome>,
    pub last_http_status: Option<u16>,
    pub next_allowed_poll_at: Option<DateTime<Utc>>,
    pub phases: BTreeMap<String, u64>,
    pub next_head: Option<String>,
    pub next_head_earliest_eligibility: Option<DateTime<Utc>>,
    pub recent_errors: Vec<String>,
    /// Dedicated surface for queue entries stuck in
    /// `NeedsAttention` because of a refuse-to-operate
    /// condition. Each entry carries the stable block
    /// source tag and the human-readable recovery command
    /// so the operator can see at a glance what is wedged
    /// and how to unstick it, without parsing error text.
    pub blocked_issues: Vec<BlockedEntry>,
    pub rate_limit: Option<crate::state::meta::RateLimitObservation>,
    pub live_workers: Vec<LiveWorker>,
    pub diagnostics: Vec<String>,
    /// `true` when the daemon recorded a corrupt-marker on
    /// the metadata file. The CLI refuses to start a tick in
    /// that mode; the operator must clear the marker.
    pub state_corrupt: bool,
    /// Readiness diagnostics: bridge/harness/provider status.
    pub readiness: Option<BTreeMap<String, String>>,
    /// Pool state: "idle", "active(n)", "saturated", or "draining".
    pub pool_state: Option<String>,
    /// Last CLI doctor result. Informational only; dispatch never reads it.
    pub doctor: Option<DoctorStatus>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorStatus {
    pub verdict: String,
    pub generated_at: DateTime<Utc>,
    pub informational: bool,
    pub failed_checks: Vec<String>,
}

fn read_doctor_status(state_dir: &Path) -> Option<DoctorStatus> {
    let bytes = fs::read(state_dir.join("doctor.json")).ok()?;
    let report: ReadinessReport = serde_json::from_slice(&bytes).ok()?;
    if report.schema_version != crate::readiness::REPORT_SCHEMA_VERSION {
        return None;
    }
    Some(DoctorStatus {
        verdict: match report.verdict {
            ReadinessVerdict::Ready => "READY".to_string(),
            ReadinessVerdict::Unavailable => "UNAVAILABLE".to_string(),
        },
        generated_at: report.generated_at,
        informational: true,
        failed_checks: report
            .checks
            .iter()
            .filter(|check| check.status == crate::readiness::CheckStatus::Fail)
            .map(|check| check.id.to_string())
            .collect(),
    })
}

/// One blocked (refuse-to-operate) entry surfaced by
/// the status reporter. Built from queue entries in
/// `NeedsAttention` phase that carry both
/// `blocked_source` and `blocked_recovery_hint` — the
/// `recent_errors` list still surfaces blocked info for
/// backwards compatibility, but this struct is the
/// authoritative contract for the dedicated `blocked
/// issues` section and the JSON `blocked_issues` field.
#[derive(Clone, Debug, Serialize)]
pub struct BlockedEntry {
    /// Display key, e.g. `owner/repo#42`.
    pub issue_key: String,
    /// Stable source tag (`worktree/dirty_main`,
    /// `worktree/path_collision`, `queue/claim_mismatch`).
    pub blocked_source: String,
    /// Canonical recovery command the operator should
    /// run to unstick the entry.
    pub blocked_recovery_hint: String,
    /// Last error text recorded when the entry was
    /// routed to `NeedsAttention`. Empty string when
    /// the entry has no recorded reason.
    pub last_error: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveWorker {
    pub run_id: String,
    pub issue: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub transcript_path: PathBuf,
    /// "fresh" (<90s) or "stale" (>=90s). The reporter
    /// surfaces both so the operator can tell whether
    /// the daemon is keeping up.
    pub freshness: String,
}

/// Schema version. Bumped when a new field is added so
/// the `--json` consumer can detect the version.
///
/// 7.7.0 — added the informational `doctor` field.
///
/// 7.6.0 — added `blocked_issues` field on
/// `StatusReport` carrying dedicated `BlockedEntry`
/// records for queue entries stuck in `NeedsAttention`
/// because of a refuse-to-operate condition. The field
/// is additive: the section is omitted from the human
/// output when empty, and the JSON array is always
/// present (empty array on a healthy state).
pub const STATUS_SCHEMA_VERSION: &str = "7.7.0";

/// Maximum number of recent errors surfaced by
/// `StatusReport::recent_errors`.
const MAX_RECENT_ERRORS: usize = 10;

/// Staleness threshold for live workers. A heartbeat
/// older than this is reported as `stale` rather than
/// `fresh`. The contract pins the value at 90 seconds.
const HEARTBEAT_STALE_SECONDS: i64 = 90;

/// Build a [`StatusReport`] for `state_dir` without
/// consuming the lock. The reporter is read-only and
/// always safe to run alongside an active tick.
pub fn build_report(state_dir: &Path) -> CaduceusResult<(StatusReport, Option<StatusDiagnostic>)> {
    if !state_dir.exists() {
        let report = empty_report(state_dir);
        return Ok((report, Some(StatusDiagnostic::NoState)));
    }

    // 1. Load metadata strictly. A corrupt metadata file
    //    preserves the file and surfaces a diagnostic.
    let meta = match MetaStore::open(state_dir) {
        Ok(m) => m,
        Err(err) => {
            let (path, message) = match &err {
                CaduceusError::StateCorrupt { path, message } => (path.clone(), message.clone()),
                _ => (
                    state_dir.join("state_meta.json"),
                    format!("metadata load failed: {err}"),
                ),
            };
            let report = empty_report(state_dir);
            return Ok((
                report,
                Some(StatusDiagnostic::CorruptState { path, message }),
            ));
        }
    };

    // 2. Read the queue state. A corrupt queue file is
    //    preserved and surfaces a distinct diagnostic.
    //    The store's `open` calls
    //    `load_validated`, so a corrupt file produces a
    //    `StateCorrupt` error from `StateStore::open`
    //    itself; we route both the open and snapshot
    //    error paths through the same diagnostic.
    let store = match StateStore::open(state_dir) {
        Ok(s) => s,
        Err(err) => {
            let (path, message) = match &err {
                CaduceusError::StateCorrupt { path, message } => (path.clone(), message.clone()),
                _ => (
                    state_dir.join("state.json"),
                    format!("queue load failed: {err}"),
                ),
            };
            let report = empty_report(state_dir);
            return Ok((
                report,
                Some(StatusDiagnostic::CorruptQueue { path, message }),
            ));
        }
    };
    let queue = match store.snapshot() {
        Ok(s) => s,
        Err(err) => {
            let (path, message) = match &err {
                CaduceusError::StateCorrupt { path, message } => (path.clone(), message.clone()),
                _ => (
                    state_dir.join("state.json"),
                    format!("queue load failed: {err}"),
                ),
            };
            let report = empty_report(state_dir);
            return Ok((
                report,
                Some(StatusDiagnostic::CorruptQueue { path, message }),
            ));
        }
    };

    // 3. Phase counts (including `Previewed`).
    let mut phases: BTreeMap<String, u64> = BTreeMap::new();
    for variant in PHASE_VARIANTS {
        phases.insert(phase_label(*variant).to_string(), 0);
    }
    for entry in queue.entries.values() {
        let key = phase_label(entry.phase).to_string();
        *phases.entry(key).or_insert(0) += 1;
    }

    // 4. FIFO eligible head. The first `Queued` entry
    //    whose `next_attempt_at` is in the past (or
    //    unset) is the next head; if every queued entry
    //    is backed off, surface the earliest eligibility.
    let (next_head, next_head_earliest) = compute_next_head(&queue, Utc::now());

    // 5. Recent errors. The queue entries' `last_error` plus
    //    the metadata's `last_error` plus the metadata
    //    diagnostics, capped at 10.
    let recent_errors = collect_recent_errors(&queue, &meta.snapshot());

    // 5b. Dedicated blocked-issues surface — only queue
    //     entries in `NeedsAttention` with typed block
    //     metadata contribute. The section is omitted from
    //     the human render when empty; the JSON field is
    //     always present (additive over 7.5.0).
    let blocked_issues = collect_blocked_issues(&queue);

    // 6. Live workers from heartbeats. The reporter
    //    filters for `*.heartbeat` regular files whose
    //    mtime is within the staleness window. The
    //    `read_heartbeat_record` helper parses the
    //    supervisor's versioned envelope.
    let live_workers = collect_live_workers(state_dir);

    // 7. The metadata reports `state_corrupt` if the
    //    marker file is present. The CLI refuses to tick
    //    in that mode.
    let state_corrupt = meta.is_corrupt();

    // 8. Readiness diagnostics: check bridge existence
    //    and provider status.
    let mut readiness = BTreeMap::new();
    let bridge_path = state_dir
        .parent()
        .map(|p| p.join("caduceus").join("worker-bridge.py"));
    match bridge_path {
        Some(ref p) if p.is_file() => {
            readiness.insert("bridge".to_string(), "present".to_string());
            readiness.insert("harness".to_string(), "present".to_string());
        }
        _ => {
            readiness.insert("bridge".to_string(), "missing".to_string());
            readiness.insert("harness".to_string(), "missing".to_string());
        }
    }
    readiness.insert("provider".to_string(), "not-applicable".to_string());

    let report = StatusReport {
        version: STATUS_SCHEMA_VERSION.to_string(),
        state_dir: state_dir.to_path_buf(),
        last_tick_started: meta.snapshot().last_tick_started,
        last_tick_finished: meta.snapshot().last_tick_finished,
        last_outcome: meta.snapshot().last_outcome,
        last_http_status: meta.snapshot().last_http_status,
        next_allowed_poll_at: meta.snapshot().next_allowed_poll_at,
        phases,
        next_head,
        next_head_earliest_eligibility: next_head_earliest,
        recent_errors,
        blocked_issues,
        rate_limit: meta.snapshot().rate_limit,
        live_workers,
        diagnostics: Vec::new(),
        state_corrupt,
        readiness: Some(readiness),
        pool_state: None,
        doctor: read_doctor_status(state_dir),
    };
    Ok((report, None))
}

const PHASE_VARIANTS: &[Phase] = &[
    Phase::Queued,
    Phase::InProgress,
    Phase::Previewed,
    Phase::Done,
    Phase::Failed,
    Phase::Skipped,
    Phase::NeedsAttention,
    Phase::AwaitingReview,
];

fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Queued => "queued",
        Phase::InProgress => "in_progress",
        Phase::Previewed => "previewed",
        Phase::Done => "done",
        Phase::Failed => "failed",
        Phase::Skipped => "skipped",
        Phase::NeedsAttention => "needs_attention",
        Phase::AwaitingReview => "awaiting_review",
    }
}

fn empty_report(state_dir: &Path) -> StatusReport {
    let mut phases: BTreeMap<String, u64> = BTreeMap::new();
    for variant in PHASE_VARIANTS {
        phases.insert(phase_label(*variant).to_string(), 0);
    }
    StatusReport {
        version: STATUS_SCHEMA_VERSION.to_string(),
        state_dir: state_dir.to_path_buf(),
        last_tick_started: None,
        last_tick_finished: None,
        last_outcome: None,
        last_http_status: None,
        next_allowed_poll_at: None,
        phases,
        next_head: None,
        next_head_earliest_eligibility: None,
        recent_errors: Vec::new(),
        blocked_issues: Vec::new(),
        rate_limit: None,
        live_workers: Vec::new(),
        diagnostics: Vec::new(),
        state_corrupt: false,
        readiness: None,
        pool_state: None,
        doctor: read_doctor_status(state_dir),
    }
}

/// Compute the FIFO next-head and the earliest future
/// eligibility. The `next_head` is the first `Queued`
/// entry whose `next_attempt_at` is unset or in the past;
/// `next_head_earliest_eligibility` is the earliest
/// future `next_attempt_at` among all queued entries.
pub fn compute_next_head(
    queue: &QueueState,
    now: DateTime<Utc>,
) -> (Option<String>, Option<DateTime<Utc>>) {
    compute_next_head_inner(queue, now)
}

fn compute_next_head_inner(
    queue: &QueueState,
    now: DateTime<Utc>,
) -> (Option<String>, Option<DateTime<Utc>>) {
    let mut earliest_future: Option<DateTime<Utc>> = None;
    let mut head: Option<String> = None;
    // The queue is a BTreeMap keyed by lowercase display
    // key, so iteration order is the canonical lexical
    // FIFO. The first eligible entry wins.
    for (display_key, entry) in queue.entries.iter() {
        if entry.phase != Phase::Queued {
            continue;
        }
        match entry.next_attempt_at {
            Some(at) if at > now => {
                if earliest_future.is_none_or(|e| at < e) {
                    earliest_future = Some(at);
                }
            }
            _ => {
                if head.is_none() {
                    head = Some(display_key.clone());
                }
            }
        }
    }
    (head, earliest_future)
}

fn collect_recent_errors(queue: &QueueState, meta: &StateMeta) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();
    if let Some(last) = &meta.last_error {
        errors.push(format!("daemon: {last}"));
    }
    for diag in meta.recent_diagnostics.iter().rev() {
        errors.push(format!(
            "diagnostic [{}]: {}",
            diag.code,
            match &diag.issue_key {
                Some(k) => k.display_key(),
                None => "-".to_string(),
            }
        ));
    }
    // Pull the most recent `last_error` from each
    // non-terminal entry. Terminal phases are excluded
    // because they aren't actionable. NeedsAttention entries
    // additionally surface the stable source tag and the
    // recovery hint so operators don't have to parse error
    // text at the console.
    for entry in queue.entries.values() {
        if entry.phase == Phase::Done
            || entry.phase == Phase::Skipped
            || entry.phase == Phase::InProgress
        {
            continue;
        }
        if let Some(err) = &entry.last_error {
            errors.push(format!("{}: {}", entry.key.display_key(), err));
        }
        if entry.phase == Phase::NeedsAttention {
            if let Some(source) = &entry.blocked_source {
                errors.push(format!(
                    "{}: blocked: source = {}",
                    entry.key.display_key(),
                    source
                ));
            }
            if let Some(hint) = &entry.blocked_recovery_hint {
                errors.push(format!(
                    "{}: blocked: recovery = {}",
                    entry.key.display_key(),
                    hint
                ));
            }
        }
    }
    errors.truncate(MAX_RECENT_ERRORS);
    errors
}

/// Build the dedicated `blocked_issues` surface from
/// queue entries in `NeedsAttention` phase that carry
/// the typed block metadata. Entries without both
/// `blocked_source` and `blocked_recovery_hint` are
/// skipped — those fields are set only by the
/// terminal-block dispatch path, so a `NeedsAttention`
/// entry without them is a legacy state that the
/// reporter cannot present coherently here (the
/// `recent_errors` list still surfaces it for context).
fn collect_blocked_issues(queue: &QueueState) -> Vec<BlockedEntry> {
    let mut out: Vec<BlockedEntry> = Vec::new();
    for entry in queue.entries.values() {
        if entry.phase != Phase::NeedsAttention {
            continue;
        }
        let (Some(source), Some(hint)) = (
            entry.blocked_source.as_ref(),
            entry.blocked_recovery_hint.as_ref(),
        ) else {
            continue;
        };
        out.push(BlockedEntry {
            issue_key: entry.key.display_key(),
            blocked_source: source.clone(),
            blocked_recovery_hint: hint.clone(),
            last_error: entry.last_error.clone().unwrap_or_default(),
        });
    }
    // Deterministic ordering — the report is rendered
    // for humans, and grep-ability across runs depends
    // on a stable iteration order. BTreeMap iterates in
    // lexical display-key order, which matches the
    // queue's FIFO convention.
    out.sort_by(|a, b| a.issue_key.cmp(&b.issue_key));
    out
}

fn collect_live_workers(state_dir: &Path) -> Vec<LiveWorker> {
    let runs_dir = state_dir.join("runs");
    let entries = match fs::read_dir(&runs_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let now = Utc::now();
    let mut workers = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Symlink rejection: a symlinked heartbeat is
        // never accepted, even if the target is fresh.
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".heartbeat") {
            continue;
        }
        // Read the versioned envelope. `None` means
        // missing / malformed — skip.
        let Some(record) = read_heartbeat_record(&path) else {
            continue;
        };
        // The contract pins the staleness window at 90s
        // regardless of mtime. The reporter uses
        // saturating elapsed-time arithmetic so a future
        // heartbeat (record.updated_at > now) is
        // surfaced as `fresh` with `age = 0`.
        let elapsed = now.signed_duration_since(record.updated_at);
        let age_seconds = elapsed.num_seconds().max(0);
        let freshness = if age_seconds <= HEARTBEAT_STALE_SECONDS {
            "fresh".to_string()
        } else {
            "stale".to_string()
        };
        // Both fresh and stale workers are surfaced so
        // the operator can investigate; the `freshness`
        // marker tells them which is which.
        workers.push(LiveWorker {
            run_id: record.run_id,
            issue: record.target.clone(),
            pid: record.pid,
            started_at: record.started_at,
            updated_at: record.updated_at,
            transcript_path: record.transcript_path,
            freshness,
        });
    }
    // Sort by run_id so the output is deterministic.
    workers.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    workers
}

/// Render the report in the human-readable format the
/// README documents. The output is line-oriented and
/// stable across daemon versions; integration tests
/// pin the exact bytes.
pub fn render_human(report: &StatusReport, diagnostic: Option<&StatusDiagnostic>) -> String {
    let mut out = String::new();
    out.push_str("caduceus status\n");
    out.push_str(&format!("  state dir: {}\n", report.state_dir.display()));
    if let Some(d) = diagnostic {
        match d {
            StatusDiagnostic::NoState => {
                out.push_str("  no state yet — run `caduceus run` to bootstrap\n");
            }
            StatusDiagnostic::CorruptState { path, message } => {
                out.push_str(&format!("  state_meta corrupt: {}\n", path.display()));
                out.push_str(&format!("    {message}\n"));
            }
            StatusDiagnostic::CorruptQueue { path, message } => {
                out.push_str(&format!("  state.json corrupt: {}\n", path.display()));
                out.push_str(&format!("    {message}\n"));
            }
        }
        return out;
    }
    if let Some(started) = report.last_tick_started {
        out.push_str(&format!("  last tick started: {}\n", started.to_rfc3339()));
    } else {
        out.push_str("  last tick started: -\n");
    }
    if let Some(finished) = report.last_tick_finished {
        out.push_str(&format!(
            "  last tick finished: {}\n",
            finished.to_rfc3339()
        ));
    } else {
        out.push_str("  last tick finished: -\n");
    }
    if let Some(o) = report.last_outcome {
        out.push_str(&format!("  last outcome: {:?}\n", o));
    } else {
        out.push_str("  last outcome: -\n");
    }
    if let Some(s) = report.last_http_status {
        out.push_str(&format!("  last http status: {s}\n"));
    }
    if let Some(next) = report.next_allowed_poll_at {
        out.push_str(&format!("  next allowed poll: {}\n", next.to_rfc3339()));
    }
    if report.state_corrupt {
        out.push_str("  STATE META CORRUPT — refusing to tick until cleared\n");
    }
    if let Some(ref pool) = report.pool_state {
        out.push_str(&format!("  pool state: {pool}\n"));
    }
    if let Some(doctor) = &report.doctor {
        out.push_str(&format!(
            "  doctor: {} (informational, generated {})\n",
            doctor.verdict,
            doctor.generated_at.to_rfc3339()
        ));
    }
    out.push_str("  phases:\n");
    for (label, count) in &report.phases {
        out.push_str(&format!("    {label}: {count}\n"));
    }
    if let Some(head) = &report.next_head {
        out.push_str(&format!("  next head: {head}\n"));
    } else if let Some(earliest) = report.next_head_earliest_eligibility {
        out.push_str(&format!(
            "  next head: (all backed off) earliest eligibility = {}\n",
            earliest.to_rfc3339()
        ));
    } else {
        out.push_str("  next head: -\n");
    }
    if report.live_workers.is_empty() {
        out.push_str("  live workers: (none)\n");
    } else {
        out.push_str(&format!("  live workers: {}\n", report.live_workers.len()));
        for w in &report.live_workers {
            out.push_str(&format!(
                "    - {} issue={} pid={} freshness={} age={}s transcript={}\n",
                w.run_id,
                w.issue,
                w.pid,
                w.freshness,
                now_seconds_ago(&w.updated_at),
                w.transcript_path.display()
            ));
        }
    }
    if !report.blocked_issues.is_empty() {
        out.push_str("  blocked issues:\n");
        for entry in &report.blocked_issues {
            out.push_str(&format!("    - {}\n", entry.issue_key));
            out.push_str(&format!("      source: {}\n", entry.blocked_source));
            // The reason line is informational; an entry
            // can legitimately lack `last_error` if the
            // daemon routed it via a context-only path,
            // so we fall back to `-` to keep the section
            // shape stable.
            let reason = if entry.last_error.is_empty() {
                "-".to_string()
            } else {
                entry.last_error.clone()
            };
            out.push_str(&format!("      reason: {reason}\n"));
            out.push_str(&format!(
                "      recovery: {}\n",
                entry.blocked_recovery_hint
            ));
        }
    }
    if !report.recent_errors.is_empty() {
        out.push_str("  recent errors:\n");
        for err in &report.recent_errors {
            out.push_str(&format!("    - {err}\n"));
        }
    }
    out
}

fn now_seconds_ago(updated_at: &DateTime<Utc>) -> i64 {
    let elapsed = Utc::now().signed_duration_since(*updated_at);
    elapsed.num_seconds().max(0)
}

/// Render the report as JSON. The output carries a
/// `version` field so consumers can detect schema
/// changes. The reporter always succeeds when the
/// report was built; the caller is responsible for
/// surfacing a separate diagnostic.
pub fn render_json(report: &StatusReport) -> CaduceusResult<String> {
    serde_json::to_string_pretty(report)
        .map_err(|err| CaduceusError::Other(format!("serialise status report: {err}")))
}

/// Public entry point that powers `caduceus status`. The
/// reporter is read-only; it does not take the daemon
/// lock. The CLI host passes `--json` to switch output
/// format, and maps the returned diagnostic to the
/// correct process exit code per `RUN-005`.
pub fn report(state_dir: &Path, json: bool) -> CaduceusResult<(String, Option<StatusDiagnostic>)> {
    let (report, diagnostic) = build_report(state_dir)?;
    if json {
        // The JSON output embeds the diagnostic as a
        // top-level string so consumers can detect the
        // "no state yet" / "corrupt" cases without
        // parsing the human format.
        let payload = serde_json::json!({
            "app_version": env!("CARGO_PKG_VERSION"),
            "version": report.version,
            "state_dir": report.state_dir,
            "diagnostic": match diagnostic.as_ref() {
                Some(StatusDiagnostic::NoState) => Some("no_state"),
                Some(StatusDiagnostic::CorruptState { .. }) => Some("corrupt_state"),
                Some(StatusDiagnostic::CorruptQueue { .. }) => Some("corrupt_queue"),
                None => None,
            },
            "report": report,
        });
        let output = serde_json::to_string_pretty(&payload)
            .map_err(|err| CaduceusError::Other(format!("serialise status report: {err}")))?;
        Ok((output, diagnostic))
    } else {
        Ok((render_human(&report, diagnostic.as_ref()), diagnostic))
    }
}

/// Build a [`StatusReport`] directly from a [`StateMeta`]
/// snapshot and a [`QueueState`] snapshot. Used by the
/// integration tests that already have both in scope and
/// don't want to round-trip through the on-disk format.
#[allow(dead_code)]
pub fn build_report_from_state(
    state_dir: &Path,
    meta: &StateMeta,
    queue: &QueueState,
) -> StatusReport {
    let mut phases: BTreeMap<String, u64> = BTreeMap::new();
    for variant in PHASE_VARIANTS {
        phases.insert(phase_label(*variant).to_string(), 0);
    }
    for entry in queue.entries.values() {
        let key = phase_label(entry.phase).to_string();
        *phases.entry(key).or_insert(0) += 1;
    }
    let (next_head, next_head_earliest) = compute_next_head(queue, Utc::now());
    let recent_errors = collect_recent_errors(queue, meta);
    let blocked_issues = collect_blocked_issues(queue);
    StatusReport {
        version: STATUS_SCHEMA_VERSION.to_string(),
        state_dir: state_dir.to_path_buf(),
        last_tick_started: meta.last_tick_started,
        last_tick_finished: meta.last_tick_finished,
        last_outcome: meta.last_outcome,
        last_http_status: meta.last_http_status,
        next_allowed_poll_at: meta.next_allowed_poll_at,
        phases,
        next_head,
        next_head_earliest_eligibility: next_head_earliest,
        recent_errors,
        blocked_issues,
        rate_limit: meta.rate_limit.clone(),
        live_workers: Vec::new(),
        diagnostics: Vec::new(),
        state_corrupt: false,
        readiness: None,
        pool_state: None,
        doctor: read_doctor_status(state_dir),
    }
}

/// `true` when the entry is a `Code` ticket, used by the
/// README golden fixture to align ticket-type columns.
#[allow(dead_code)]
pub fn entry_is_code(entry: &QueueEntry) -> bool {
    entry.ticket_type == TicketType::Code
}

/// Build a [`LiveWorker`] from a [`Heartbeat`] and the
/// `now` time. Used by tests that drive the heartbeat
/// path without touching the on-disk file.
#[allow(dead_code)]
pub fn live_worker_from_heartbeat(record: &Heartbeat, now: DateTime<Utc>) -> LiveWorker {
    let elapsed = now.signed_duration_since(record.updated_at);
    let age_seconds = elapsed.num_seconds().max(0);
    let freshness = if age_seconds <= HEARTBEAT_STALE_SECONDS {
        "fresh".to_string()
    } else {
        "stale".to_string()
    };
    LiveWorker {
        run_id: record.run_id.clone(),
        issue: record.target.clone(),
        pid: record.pid,
        started_at: record.started_at,
        updated_at: record.updated_at,
        transcript_path: record.transcript_path.clone(),
        freshness,
    }
}

/// Build a fresh [`Heartbeat`] for tests. Mirrors the
/// supervisor's production writer so the
/// `read_heartbeat_record` round-trip succeeds.
#[allow(dead_code)]
pub fn sample_heartbeat(run_id: &str, target: &str, now: DateTime<Utc>) -> Heartbeat {
    Heartbeat {
        version: crate::worker::supervisor::HEARTBEAT_FILE_VERSION,
        run_id: run_id.to_string(),
        pid: std::process::id(),
        started_at: now,
        updated_at: now,
        target: target.to_string(),
        transcript_path: PathBuf::from(format!("/tmp/runs/{run_id}.log")),
    }
}

//! In-tick PR discovery + admission tests (issue #312, plan Tasks 1/4/5/6/7).
//!
//! Coverage per DAR §15 and the issue's required-tests list:
//!
//! - Event-constant catalog pin + structured emission capture
//!   (`serial_test::serial`: `tracing_core` caches callsite interest
//!   process-wide, the #167 finding).
//! - The pure eligibility classifier's §5.1 truth table: open,
//!   draft (default + opted-in), closed, merged, fork, nullable
//!   head repo, already-active / already-reviewed dedup,
//!   stale-SHA observation, malformed rows, and the
//!   draft-beats-fork ordering pin.
//! - Admission against a real local git remote + `ReviewStore`:
//!   merge-base capture (AC5), generation assignment + bump (AC4),
//!   dedup, and unavailable-SHA skip.
//! - The `poll_review_step` loop over wiremock: per-repo failure
//!   isolation (AC2), rate-limit surfacing, malformed body,
//!   admission budget (AC6), zero mirror work when nothing
//!   changed (AC7), stale event + admission, pagination, and
//!   wire-order admission.
//! - Tick wiring: issues-before-pulls HTTP order (AC1), disabled /
//!   dry-run skip, corrupt review store does not starve the drain
//!   (AC3), and the D14 rate-limit observation regression.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use caduceus::config::{AutoReviewConfig, Config, LoadContext, RawConfig};
use caduceus::daemon::tick::review_discovery::{
    admit_target_for_tests, classify_discovery_row_for_tests, emit_admitted_for_tests,
    emit_skipped_already_complete_for_tests, emit_skipped_draft_for_tests,
    emit_stale_sha_for_tests, poll_review_step_for_tests, ADMITTED_EVENT, DISCOVERED_EVENT,
    SKIPPED_ALREADY_COMPLETE_EVENT, SKIPPED_DRAFT_EVENT, STALE_SHA_EVENT,
};
use caduceus::error::CaduceusError;
use caduceus::github::fork_gate::FORK_SKIP_EVENT;
use caduceus::github::{Client, HttpCache, PullRequestDetail};
use caduceus::infra::logging::build_test_subscriber;
use caduceus::meta::TickOutcome;
use caduceus::orchestration::SystemClock;
use caduceus::repo::BareMirror;
use caduceus::review::{RepositoryId, ReviewState, ReviewTarget};
use caduceus::scheduler::{DrainConfig, Pool};
use caduceus::state::meta::MetaStore;
use caduceus::state::review::{ReviewPhase, ReviewStore};
use caduceus::worktree::GitRunner;
use chrono::Utc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A `/pulls` row. `None` renders `"repo": null` (the deleted-head
/// wire shape). Defaults: number 7, open, non-draft, SHAs `aaaa` /
/// `bbbb`.
fn pr_row(head_repo: Option<&str>, base_repo: Option<&str>) -> serde_json::Value {
    let mut row = serde_json::json!({
        "number": 7,
        "title": "Discovery row",
        "state": "open",
        "draft": false,
        "base": {"ref": "main", "sha": "aaaa"},
        "head": {"ref": "feature-x", "sha": "bbbb"}
    });
    row["base"]["repo"] = base_repo
        .map(|name| serde_json::json!({"full_name": name}))
        .unwrap_or(serde_json::Value::Null);
    row["head"]["repo"] = head_repo
        .map(|name| serde_json::json!({"full_name": name}))
        .unwrap_or(serde_json::Value::Null);
    row
}

fn with_state(mut row: serde_json::Value, state: &str) -> serde_json::Value {
    row["state"] = serde_json::json!(state);
    row
}

fn with_draft(mut row: serde_json::Value, draft: bool) -> serde_json::Value {
    row["draft"] = serde_json::json!(draft);
    row
}

fn with_number(mut row: serde_json::Value, number: Option<u64>) -> serde_json::Value {
    row["number"] = number
        .map(|n| serde_json::json!(n))
        .unwrap_or(serde_json::Value::Null);
    row
}

fn with_head_sha(mut row: serde_json::Value, sha: &str) -> serde_json::Value {
    row["head"]["sha"] = serde_json::json!(sha);
    row
}

fn with_head_sha_absent(mut row: serde_json::Value) -> serde_json::Value {
    if let Some(head) = row.get_mut("head").and_then(|h| h.as_object_mut()) {
        head.remove("sha");
    }
    row
}

fn with_base_ref_absent(mut row: serde_json::Value) -> serde_json::Value {
    if let Some(base) = row.get_mut("base").and_then(|b| b.as_object_mut()) {
        base.remove("ref");
    }
    row
}

/// Decode a wire row into the typed model. A panic here means the
/// fixture is not a valid `/pulls` row.
fn decode(row: serde_json::Value) -> PullRequestDetail {
    serde_json::from_value(row).expect("wire row decodes into PullRequestDetail")
}

fn ar_config(enabled: bool) -> AutoReviewConfig {
    AutoReviewConfig {
        enabled,
        draft_pull_requests: false,
    }
}

fn ar_config_drafts(enabled: bool, drafts: bool) -> AutoReviewConfig {
    AutoReviewConfig {
        enabled,
        draft_pull_requests: drafts,
    }
}

fn held_none() -> caduceus::daemon::tick::review_discovery::HeldShas {
    caduceus::daemon::tick::review_discovery::HeldShas {
        active: Vec::new(),
        last_reviewed: None,
    }
}

fn held(
    active: &[&str],
    last_reviewed: Option<&str>,
) -> caduceus::daemon::tick::review_discovery::HeldShas {
    caduceus::daemon::tick::review_discovery::HeldShas {
        active: active.iter().map(|s| s.to_string()).collect(),
        last_reviewed: last_reviewed.map(|s| s.to_string()),
    }
}

/// Config with the `auto_review` block enabled and discovery pointed
/// at the wiremock base. TrustedHost is fine here: only `from_raw`
/// rejects the TrustedHost+enabled combination, and `test_defaults`
/// bypasses `from_raw` (the documented escape hatch for tests).
fn discovery_config(root: &Path, api_base: &str) -> Config {
    let mut cfg = Config::test_defaults(root);
    cfg.api_base = api_base.to_string();
    cfg.watched_repos = vec!["owner/r".to_string()];
    cfg.auto_review = Some(ar_config(true));
    cfg
}

fn review_store(state_dir: &Path) -> ReviewStore {
    ReviewStore::open(state_dir).expect("review store opens")
}

fn repo_id() -> RepositoryId {
    RepositoryId {
        owner: "owner".to_string(),
        repo: "r".to_string(),
    }
}

fn target(head_sha: &str, base_sha: &str, merge_base: &str) -> ReviewTarget {
    ReviewTarget {
        repository: repo_id(),
        pull_request: 7,
        head_sha: head_sha.to_string(),
        base_sha: base_sha.to_string(),
        base_ref: "main".to_string(),
        merge_base: merge_base.to_string(),
    }
}

/// Initialise a bare remote with `main` (commit A), a `feature`
/// branch (commit B, child of A), and commit C (child of B, the
/// branch tip). Returns `(A, B, C)`. B and C are non-ancestors of
/// main, so deleting `feature` + gc prunes them (mirror_test.rs
/// discipline).
fn init_bare_remote_with_feature(path: &Path) -> (String, String, String) {
    let run = |cmd: &mut Command| {
        let output = cmd.output().expect("spawn command");
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(Command::new("git").arg("init").arg("--bare").arg(path));
    run(Command::new("git")
        .current_dir(path)
        .args(["symbolic-ref", "HEAD", "refs/heads/main"]));
    let tree = {
        let output = Command::new("git")
            .current_dir(path)
            .args(["hash-object", "-w", "-t", "tree", "/dev/null"])
            .output()
            .expect("hash-object");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let commit = |message: &str, parent: Option<&str>| -> String {
        let mut args = vec!["commit-tree".to_string(), tree.clone()];
        if let Some(p) = parent {
            args.push("-p".to_string());
            args.push(p.to_string());
        }
        args.push("-m".to_string());
        args.push(message.to_string());
        let output = Command::new("git")
            .current_dir(path)
            .args(&args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("commit-tree");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let commit_a = commit("base", None);
    run(Command::new("git")
        .current_dir(path)
        .args(["update-ref", "refs/heads/main", &commit_a]));
    let commit_b = commit("feature", Some(&commit_a));
    let commit_c = commit("feature tip", Some(&commit_b));
    run(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/feature",
        &commit_c,
    ]));
    (commit_a, commit_b, commit_c)
}

struct MirrorFixture {
    root: std::path::PathBuf,
    runner: GitRunner,
    mirror: BareMirror,
    base_sha: String,
    head_sha: String,
}

/// Add an orphan commit (a second root, no parents) on
/// `refs/heads/orphan` to the bare remote at `path`. The commit is
/// fetchable (a branch tip), but has NO common ancestor with
/// `main`, so `git merge-base main..orphan` fails — discovery
/// surfaces that as `CaduceusError::Git` (the per-target git-error
/// shape the D9 isolation tests need, distinct from
/// `HeadShaUnavailable`).
fn add_orphan_commit(path: &Path) -> String {
    let run = |cmd: &mut Command| {
        let output = cmd.output().expect("spawn command");
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let tree = {
        let output = Command::new("git")
            .current_dir(path)
            .args(["hash-object", "-w", "-t", "tree", "/dev/null"])
            .output()
            .expect("hash-object");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let output = Command::new("git")
        .current_dir(path)
        .args(["commit-tree", &tree, "-m", "orphan root"])
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("commit-tree");
    let orphan = String::from_utf8_lossy(&output.stdout).trim().to_string();
    run(Command::new("git")
        .current_dir(path)
        .args(["update-ref", "refs/heads/orphan", &orphan]));
    orphan
}

/// Local bare remote + mirror with `main` fetched; `feature` tip NOT
/// fetched (mirrors production: `ensure` fetches the base branch
/// only).
async fn mirror_fixture(label: &str) -> MirrorFixture {
    let root = tempdir(label);
    let remote_dir = root.join("remote.git");
    let (base_sha, head_sha, _tip) = init_bare_remote_with_feature(&remote_dir);
    let remote_url = format!("file://{}", remote_dir.display());

    let mut cfg = Config::test_defaults(&root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    let runner = GitRunner::new(&cfg);

    let mirror = BareMirror::ensure(&runner, &cfg, "owner", "r", &remote_url, "main")
        .await
        .expect("ensure");

    MirrorFixture {
        root,
        runner,
        mirror,
        base_sha,
        head_sha,
    }
}

// ---------------------------------------------------------------------------
// Task 1 — event constants + emission
// ---------------------------------------------------------------------------

#[test]
fn event_constants_match_dar_13_catalog() {
    assert_eq!(DISCOVERED_EVENT, "review_discovered");
    assert_eq!(ADMITTED_EVENT, "review_admitted");
    assert_eq!(SKIPPED_DRAFT_EVENT, "review_skipped_draft");
    assert_eq!(
        SKIPPED_ALREADY_COMPLETE_EVENT,
        "review_skipped_already_complete"
    );
    assert_eq!(STALE_SHA_EVENT, "review_stale_sha_observed");
    assert_eq!(FORK_SKIP_EVENT, "review_skipped_fork_unsupported");
}

/// Capture `emit_discovered` through the plan's serial +
/// `tracing_appender::non_blocking` discipline (fork_gate_test.rs
/// pattern).
#[test]
#[serial_test::serial]
fn discovered_event_emits_structured_line() {
    let root = tempdir("discovery-event");
    let log_path = root.join("discovery.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        caduceus::daemon::tick::review_discovery::emit_discovered_for_tests("o/r", 7, "abc");
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains(&format!("\"event\":\"{DISCOVERED_EVENT}\"")),
        "event name missing: {body}"
    );
    assert!(body.contains("\"repo\":\"o/r\""), "got: {body}");
    assert!(body.contains("\"pr\":7"), "got: {body}");
    assert!(body.contains("\"head_sha\":\"abc\""), "got: {body}");
}

/// Capture `emit_admitted` (the `review_admitted` transition) through
/// the same serial + `tracing_appender::non_blocking` discipline.
#[test]
#[serial_test::serial]
fn admitted_event_emits_structured_line() {
    let root = tempdir("admission-event");
    let log_path = root.join("admission.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        emit_admitted_for_tests("o/r", 7, "abc");
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains(&format!("\"event\":\"{ADMITTED_EVENT}\"")),
        "event name missing: {body}"
    );
    assert!(body.contains("\"repo\":\"o/r\""), "got: {body}");
    assert!(body.contains("\"pr\":7"), "got: {body}");
    assert!(body.contains("\"head_sha\":\"abc\""), "got: {body}");
}

/// Capture `emit_skipped_draft` (the `review_skipped_draft` skip
/// transition).
#[test]
#[serial_test::serial]
fn skipped_draft_event_emits_structured_line() {
    let root = tempdir("draft-skip-event");
    let log_path = root.join("draft-skip.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        emit_skipped_draft_for_tests("o/r", 7, "abc");
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains(&format!("\"event\":\"{SKIPPED_DRAFT_EVENT}\"")),
        "event name missing: {body}"
    );
    assert!(body.contains("\"repo\":\"o/r\""), "got: {body}");
    assert!(body.contains("\"pr\":7"), "got: {body}");
    assert!(body.contains("\"head_sha\":\"abc\""), "got: {body}");
}

/// Capture `emit_skipped_already_complete` (the dedup skip
/// transition).
#[test]
#[serial_test::serial]
fn skipped_already_complete_event_emits_structured_line() {
    let root = tempdir("dedup-skip-event");
    let log_path = root.join("dedup-skip.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        emit_skipped_already_complete_for_tests("o/r", 7, "abc");
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains(&format!("\"event\":\"{SKIPPED_ALREADY_COMPLETE_EVENT}\"")),
        "event name missing: {body}"
    );
    assert!(body.contains("\"repo\":\"o/r\""), "got: {body}");
    assert!(body.contains("\"pr\":7"), "got: {body}");
    assert!(body.contains("\"head_sha\":\"abc\""), "got: {body}");
}

/// Capture `emit_stale_sha` (the `review_stale_sha_observed` poll
/// transition), including the held→observed SHA pair.
#[test]
#[serial_test::serial]
fn stale_sha_event_emits_structured_line() {
    let root = tempdir("stale-sha-event");
    let log_path = root.join("stale-sha.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        emit_stale_sha_for_tests("o/r", 7, "prev", "next");
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains(&format!("\"event\":\"{STALE_SHA_EVENT}\"")),
        "event name missing: {body}"
    );
    assert!(body.contains("\"repo\":\"o/r\""), "got: {body}");
    assert!(body.contains("\"pr\":7"), "got: {body}");
    assert!(body.contains("\"previous_sha\":\"prev\""), "got: {body}");
    assert!(body.contains("\"observed_sha\":\"next\""), "got: {body}");
}

// ---------------------------------------------------------------------------
// Task 4 — pure eligibility classifier (DAR §5.1 truth table)
// ---------------------------------------------------------------------------

use caduceus::daemon::tick::review_discovery::RowAction;

#[test]
fn open_non_draft_new_sha_admits() {
    let row = decode(pr_row(Some("owner/r"), Some("owner/r")));
    let decision = classify_discovery_row_for_tests(&row, &ar_config(true), &held_none());
    assert_eq!(decision.action, RowAction::Admit);
    assert!(decision.stale.is_none());
    assert_eq!(decision.head_sha, "bbbb");
    assert_eq!(decision.base_sha, "aaaa");
    assert_eq!(decision.base_ref, "main");
}

#[test]
fn draft_skips_with_event_reason() {
    let row = decode(with_draft(pr_row(Some("owner/r"), Some("owner/r")), true));
    let decision = classify_discovery_row_for_tests(&row, &ar_config(true), &held_none());
    assert_eq!(decision.action, RowAction::SkipDraft);
}

#[test]
fn draft_admits_when_configured() {
    let row = decode(with_draft(pr_row(Some("owner/r"), Some("owner/r")), true));
    let decision =
        classify_discovery_row_for_tests(&row, &ar_config_drafts(true, true), &held_none());
    assert_eq!(decision.action, RowAction::Admit);
}

#[test]
fn closed_is_ineligible_without_event() {
    let row = decode(with_state(
        pr_row(Some("owner/r"), Some("owner/r")),
        "closed",
    ));
    let decision = classify_discovery_row_for_tests(&row, &ar_config(true), &held_none());
    assert_eq!(decision.action, RowAction::Ineligible);
}

#[test]
fn merged_is_ineligible_without_event() {
    let mut row = with_state(pr_row(Some("owner/r"), Some("owner/r")), "open");
    row["merged"] = serde_json::json!(true);
    let decision = classify_discovery_row_for_tests(&decode(row), &ar_config(true), &held_none());
    assert_eq!(decision.action, RowAction::Ineligible);
}

#[test]
fn fork_skips_with_head_repo_identity() {
    let row = decode(pr_row(Some("someone/else"), Some("owner/r")));
    let decision = classify_discovery_row_for_tests(&row, &ar_config(true), &held_none());
    assert_eq!(
        decision.action,
        RowAction::SkipFork {
            head_repo: Some("someone/else".to_string())
        }
    );
}

#[test]
fn null_head_repo_skips_with_none() {
    // Deleted head branch: `head.repo: null` lands on the fork gate
    // as `HeadRepoMissing` (fail closed) — the nullable-head-repo
    // discovery fixture.
    let row = decode(pr_row(None, Some("owner/r")));
    let decision = classify_discovery_row_for_tests(&row, &ar_config(true), &held_none());
    assert_eq!(decision.action, RowAction::SkipFork { head_repo: None });
}

#[test]
fn already_reviewed_sha_skips() {
    let row = decode(pr_row(Some("owner/r"), Some("owner/r")));
    let decision =
        classify_discovery_row_for_tests(&row, &ar_config(true), &held(&[], Some("bbbb")));
    assert_eq!(decision.action, RowAction::SkipAlreadyComplete);
}

#[test]
fn active_queue_sha_skips() {
    let row = decode(pr_row(Some("owner/r"), Some("owner/r")));
    let decision = classify_discovery_row_for_tests(&row, &ar_config(true), &held(&["bbbb"], None));
    assert_eq!(decision.action, RowAction::SkipAlreadyComplete);
}

#[test]
fn moved_head_reports_stale_and_admits() {
    let row = decode(with_head_sha(
        pr_row(Some("owner/r"), Some("owner/r")),
        "cccc",
    ));
    let decision = classify_discovery_row_for_tests(&row, &ar_config(true), &held(&["bbbb"], None));
    assert_eq!(decision.action, RowAction::Admit);
    assert_eq!(
        decision.stale,
        Some(("bbbb".to_string(), "cccc".to_string()))
    );
}

#[test]
fn missing_number_is_malformed() {
    let row = decode(with_number(pr_row(Some("owner/r"), Some("owner/r")), None));
    let decision = classify_discovery_row_for_tests(&row, &ar_config(true), &held_none());
    assert!(matches!(decision.action, RowAction::Malformed { .. }));
}

#[test]
fn missing_head_sha_is_malformed() {
    let row = decode(with_head_sha_absent(pr_row(
        Some("owner/r"),
        Some("owner/r"),
    )));
    let decision = classify_discovery_row_for_tests(&row, &ar_config(true), &held_none());
    assert!(matches!(decision.action, RowAction::Malformed { .. }));
}

#[test]
fn missing_base_sha_is_malformed() {
    let row = decode(with_base_ref_absent(pr_row(
        Some("owner/r"),
        Some("owner/r"),
    )));
    // Remove the base ref: classifier must treat the row as
    // malformed before any admission path.
    assert!(matches!(
        classify_discovery_row_for_tests(&row, &ar_config(true), &held_none()).action,
        RowAction::Malformed { .. }
    ));
}

#[test]
fn missing_head_is_malformed() {
    let mut row = pr_row(Some("owner/r"), Some("owner/r"));
    row["head"] = serde_json::Value::Null;
    let decision = classify_discovery_row_for_tests(&decode(row), &ar_config(true), &held_none());
    assert!(matches!(decision.action, RowAction::Malformed { .. }));
}

#[test]
fn eligibility_order_draft_beats_fork() {
    // D5 ordering pin: the draft gate fires before the fork gate, so
    // a draft fork row carries the draft skip, not the fork skip.
    let row = decode(with_draft(
        pr_row(Some("someone/else"), Some("owner/r")),
        true,
    ));
    let decision = classify_discovery_row_for_tests(&row, &ar_config(true), &held_none());
    assert_eq!(decision.action, RowAction::SkipDraft);
}

#[test]
fn closed_beats_everything_including_malformed_base_repo() {
    // Closed state is checked before the fork gate: a closed fork
    // row is plain Ineligible (no event).
    let row = decode(with_state(
        pr_row(Some("someone/else"), Some("owner/r")),
        "closed",
    ));
    let decision = classify_discovery_row_for_tests(&row, &ar_config(true), &held_none());
    assert_eq!(decision.action, RowAction::Ineligible);
}

// ---------------------------------------------------------------------------
// Task 5 — admission (local git + real ReviewStore)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admit_persists_merge_base_and_assigns_generation() {
    let f = mirror_fixture("admit-merge-base").await;
    let store = review_store(&f.root.join("state"));

    let inserted = admit_target_for_tests(
        &f.runner,
        &f.mirror,
        &store,
        &repo_id(),
        7,
        &f.head_sha,
        &f.base_sha,
        "main",
    )
    .await
    .expect("admission succeeds");

    assert!(inserted, "first admission inserts");
    let queue = store.review_queue_snapshot().expect("snapshot");
    assert_eq!(queue.entries.len(), 1, "exactly one queue entry");
    let entry = queue.entries.values().next().expect("entry");
    assert_eq!(entry.target.head_sha, f.head_sha);
    assert_eq!(entry.target.base_sha, f.base_sha);
    assert_eq!(
        entry.target.merge_base, f.base_sha,
        "AC5: merge base persisted"
    );
    assert_eq!(entry.review_generation, 1, "first generation");
    assert_eq!(entry.phase, ReviewPhase::Queued);
}

#[tokio::test]
async fn second_admission_of_new_sha_bumps_generation() {
    let f = mirror_fixture("admit-generation").await;
    let store = review_store(&f.root.join("state"));

    admit_target_for_tests(
        &f.runner,
        &f.mirror,
        &store,
        &repo_id(),
        7,
        &f.head_sha,
        &f.base_sha,
        "main",
    )
    .await
    .expect("first admission");

    // Same (repo, pr, head): dedup keeps the queue at one entry.
    let again = admit_target_for_tests(
        &f.runner,
        &f.mirror,
        &store,
        &repo_id(),
        7,
        &f.head_sha,
        &f.base_sha,
        "main",
    )
    .await
    .expect("second admission of same target");
    assert!(!again, "AC4: duplicate (repo, pr, head) not re-admitted");
    let queue = store.review_queue_snapshot().expect("snapshot");
    assert_eq!(queue.entries.len(), 1, "still one entry");
    let generation = queue
        .entries
        .values()
        .next()
        .expect("entry")
        .review_generation;

    // A new head SHA on the same PR re-admits with a new generation
    // (AC4). The base SHA is present in the mirror, so admitting it
    // as a (synthetic) new head exercises the store's generation
    // bookkeeping without another fixture commit.
    let inserted = admit_target_for_tests(
        &f.runner,
        &f.mirror,
        &store,
        &repo_id(),
        7,
        &f.base_sha,
        &f.base_sha,
        "main",
    )
    .await
    .expect("re-admission with new head SHA");
    assert!(inserted, "new SHA after completion re-admits");
    let queue = store.review_queue_snapshot().expect("snapshot");
    let entry = queue
        .entries
        .values()
        .find(|e| e.target.head_sha == f.base_sha)
        .expect("new entry");
    assert_eq!(
        entry.review_generation,
        generation + 1,
        "AC4: generation bumped for the new head"
    );
}

#[tokio::test]
async fn unavailable_head_sha_is_skipped_not_admitted() {
    let f = mirror_fixture("admit-unavailable").await;
    let store = review_store(&f.root.join("state"));

    // A SHA the remote never had (and the mirror cannot fetch).
    let ghost = "1111111111111111111111111111111111111111";
    let result = admit_target_for_tests(
        &f.runner,
        &f.mirror,
        &store,
        &repo_id(),
        7,
        ghost,
        &f.base_sha,
        "main",
    )
    .await;

    match result {
        // D8 contract: unavailable SHA surfaces as HeadShaUnavailable
        // (discovery routes it to a log-and-skip; never enqueues).
        Err(CaduceusError::HeadShaUnavailable { sha }) => {
            assert_eq!(sha, ghost);
        }
        Ok(_) => panic!("unavailable SHA must not be admitted"),
        Err(other) => panic!("expected HeadShaUnavailable, got: {other:?}"),
    }
    let queue = store.review_queue_snapshot().expect("snapshot");
    assert!(
        queue.entries.is_empty(),
        "queue must be unchanged on unavailable SHA"
    );
}

// ---------------------------------------------------------------------------
// Task 6 — poll_review_step (wiremock + local git)
// ---------------------------------------------------------------------------

/// Discovery-loop harness: wiremock serves `/repos/owner/r/pulls`;
/// the remote resolver points at a local bare `origin` so the lazy
/// mirror bootstrap works hermetically.
struct StepHarness {
    cfg: Config,
    server: MockServer,
    store: ReviewStore,
    remote_dir: std::path::PathBuf,
    /// Real fixture SHAs: base (A, on main), mid (B), tip (C, the
    /// feature-branch head the wire rows advertise). Discovery can
    /// only admit SHAs the local remote actually serves.
    base_sha: String,
    mid_sha: String,
    tip_sha: String,
}

impl StepHarness {
    async fn start(label: &str) -> Self {
        let root = tempdir(label);
        let server = MockServer::start().await;
        let mut cfg = discovery_config(&root, &server.uri());
        cfg.repo_storage_root = root.join("repos");
        cfg.git_timeout_seconds = 30;

        let remote_dir = root.join("origin.git");
        let (base_sha, mid_sha, tip_sha) = init_bare_remote_with_feature(&remote_dir);

        let store = review_store(&root.join("state"));
        StepHarness {
            cfg,
            server,
            store,
            remote_dir,
            base_sha,
            mid_sha,
            tip_sha,
        }
    }

    /// A wire row for `owner/{repo}` whose head SHA is the fixture's
    /// real tip commit (fetchable from the local remote).
    fn row(&self, repo: &str, pr: u64, head_sha: &str) -> serde_json::Value {
        let mut row = pr_row(
            Some(&format!("owner/{repo}")),
            Some(&format!("owner/{repo}")),
        );
        row["number"] = serde_json::json!(pr);
        row["head"]["sha"] = serde_json::json!(head_sha);
        row["base"]["sha"] = serde_json::json!(self.base_sha);
        row
    }

    fn client(&self) -> Client {
        let cache = HttpCache::open(&self.cfg.state_dir).expect("cache opens");
        Client::with_cache(&self.cfg, cache).expect("client builds")
    }

    fn remote_url(&self) -> String {
        format!("file://{}", self.remote_dir.display())
    }
}

fn pulls_row_open(pr: u64, head_sha: &str) -> serde_json::Value {
    let mut row = pr_row(Some("owner/r"), Some("owner/r"));
    row["number"] = serde_json::json!(pr);
    row["head"]["sha"] = serde_json::json!(head_sha);
    row
}

#[tokio::test]
async fn per_repo_500_does_not_stop_later_repos() {
    let h = StepHarness::start("iso-500").await;
    // Repo A fails hard; repo B lists one new open PR.
    Mock::given(method("GET"))
        .and(path("/repos/owner/a/pulls"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/b/pulls"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([h.row("b", 7, &h.tip_sha)])),
        )
        .mount(&h.server)
        .await;

    // B must resolve to the same local remote.
    let remote_url = h.remote_url();
    let stats = poll_review_step_for_tests(
        &["owner/a".to_string(), "owner/b".to_string()],
        &h.client(),
        &h.cfg,
        &h.store,
        &GitRunner::new(&h.cfg),
        &move |owner: &str, repo: &str| {
            let _ = (owner, repo);
            Ok(remote_url.clone())
        },
    )
    .await
    .expect("step returns Ok despite repo A's 500 (AC2)");

    assert_eq!(stats.failed_repos, 1, "repo A counted, not fatal");
    assert_eq!(stats.repos_polled, 1, "repo B still polled");
    assert_eq!(stats.admitted, 1, "repo B's PR admitted");
    let queue = h.store.review_queue_snapshot().expect("snapshot");
    assert_eq!(queue.entries.len(), 1, "repo B's entry queued");
    let entry = queue.entries.values().next().expect("entry");
    assert_eq!(entry.target.repository.owner, "owner");
    assert_eq!(entry.target.repository.repo, "b");
}

#[tokio::test]
async fn git_admission_failure_continues_to_later_repos() {
    let h = StepHarness::start("iso-git").await;
    // Repo A lists one open PR whose head is a FETCHABLE orphan
    // commit (second root): fetch succeeds, but merge-base has no
    // common ancestor and fails as `CaduceusError::Git` — the
    // per-target git-error shape (NOT `HeadShaUnavailable`). Repo B
    // lists one new open PR and must still admit (D9 per-target
    // isolation; the old code aborted the whole step here).
    let orphan = add_orphan_commit(&h.remote_dir);
    Mock::given(method("GET"))
        .and(path("/repos/owner/a/pulls"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([h.row("a", 7, &orphan)])),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/b/pulls"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([h.row("b", 9, &h.tip_sha)])),
        )
        .mount(&h.server)
        .await;

    // Both repos resolve to the same local remote (which serves the
    // orphan branch).
    let remote_url = h.remote_url();
    let stats = poll_review_step_for_tests(
        &["owner/a".to_string(), "owner/b".to_string()],
        &h.client(),
        &h.cfg,
        &h.store,
        &GitRunner::new(&h.cfg),
        &move |owner: &str, repo: &str| {
            let _ = (owner, repo);
            Ok(remote_url.clone())
        },
    )
    .await
    .expect("step returns Ok despite repo A's git admission failure (D9)");

    assert_eq!(stats.failed_admissions, 1, "git error counted, not fatal");
    assert_eq!(stats.admitted, 1, "repo B's PR still admitted");
    assert_eq!(
        stats.skipped_unavailable_sha, 0,
        "the failure is Git, not HeadShaUnavailable"
    );
    let queue = h.store.review_queue_snapshot().expect("snapshot");
    assert_eq!(queue.entries.len(), 1, "only repo B's entry queued");
    let entry = queue.entries.values().next().expect("entry");
    assert_eq!(entry.target.repository.owner, "owner");
    assert_eq!(entry.target.repository.repo, "b");
    assert_eq!(
        entry.target.head_sha, h.tip_sha,
        "repo B's head, not the orphan"
    );
}

#[tokio::test]
async fn malformed_json_body_is_per_repo_continue() {
    let h = StepHarness::start("iso-malformed").await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/a/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"not\":\"an array\"}"))
        .mount(&h.server)
        .await;

    let stats = poll_review_step_for_tests(
        &["owner/a".to_string()],
        &h.client(),
        &h.cfg,
        &h.store,
        &GitRunner::new(&h.cfg),
        &|_o, _r| Err(CaduceusError::Config("unused".to_string())),
    )
    .await
    .expect("step returns Ok (per-repo tier)");

    assert_eq!(stats.failed_repos, 1);
    assert_eq!(stats.admitted, 0);
}

#[tokio::test]
async fn rate_limit_surfaces_as_step_error() {
    let h = StepHarness::start("rate-limit").await;
    let reset_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    Mock::given(method("GET"))
        .and(path("/repos/owner/a/pulls"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-RateLimit-Remaining", "0")
                .insert_header("X-RateLimit-Reset", reset_at_unix.to_string())
                .insert_header("X-RateLimit-Limit", "5000")
                .set_body_string("[]"),
        )
        .mount(&h.server)
        .await;

    let err = poll_review_step_for_tests(
        &["owner/a".to_string()],
        &h.client(),
        &h.cfg,
        &h.store,
        &GitRunner::new(&h.cfg),
        &|_o, _r| Err(CaduceusError::Config("unused".to_string())),
    )
    .await
    .expect_err("exhausted quota is a step-level error (D9)");

    assert!(
        matches!(err, CaduceusError::RateLimited { .. }),
        "expected RateLimited, got: {err:?}"
    );
}

#[tokio::test]
async fn budget_caps_admissions_across_repos() {
    let h = StepHarness::start("budget").await;
    let mut cfg = h.cfg.clone();
    cfg.max_reviews_per_tick = 1;
    // Both repos list one open PR whose head is the fixture's real
    // tip commit (fetchable), so the ONLY limiter is the budget.
    for (slug, name) in [("owner/a", "a"), ("owner/b", "b")] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{slug}/pulls")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([h.row(name, 7, &h.tip_sha)])),
            )
            .mount(&h.server)
            .await;
    }

    let remote_url = h.remote_url();
    let stats = poll_review_step_for_tests(
        &["owner/a".to_string(), "owner/b".to_string()],
        &h.client(),
        &cfg,
        &h.store,
        &GitRunner::new(&cfg),
        &move |_o, _r| Ok(remote_url.clone()),
    )
    .await
    .expect("budgeted step succeeds");

    assert_eq!(stats.admitted, 1, "AC6: budget caps tick-wide admissions");
    assert!(stats.budget_exhausted, "exhaustion flagged");
    let queue = h.store.review_queue_snapshot().expect("snapshot");
    assert_eq!(queue.entries.len(), 1, "only one entry enqueued");
}

#[tokio::test]
async fn no_new_shas_means_no_mirror_and_no_writes() {
    let h = StepHarness::start("no-new").await;
    // Wire: one open PR whose head SHA is already held as reviewed.
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([pulls_row_open(7, "bbbb")])),
        )
        .mount(&h.server)
        .await;

    // Seed the completed-review pointer: last_reviewed == wire head.
    let store = &h.store;
    let state = ReviewState::new(repo_id(), 7, 1);
    store.save_review_state(&state).expect("seed");
    let mut reviewed = ReviewState::new(repo_id(), 7, 1);
    reviewed.last_reviewed_head_sha = Some("bbbb".to_string());
    store.save_review_state(&reviewed).expect("seed reviewed");

    let queue_before = store.review_queue_snapshot().expect("snapshot");

    let stats = poll_review_step_for_tests(
        &["owner/r".to_string()],
        &h.client(),
        &h.cfg,
        store,
        &GitRunner::new(&h.cfg),
        &|_o, _r| {
            Err(CaduceusError::Config(
                "resolver must not be called".to_string(),
            ))
        },
    )
    .await
    .expect("no-op step succeeds");

    assert_eq!(stats.admitted, 0);
    assert_eq!(stats.skipped_already_complete, 1);
    assert!(
        !h.cfg.repo_storage_root.join("mirrors").exists(),
        "AC7: lazy mirror bootstrap must not run when nothing changed"
    );
    let queue_after = store.review_queue_snapshot().expect("snapshot");
    assert_eq!(
        queue_before.entries, queue_after.entries,
        "queue untouched when nothing changed"
    );
}

#[tokio::test]
async fn stale_sha_event_then_admission() {
    let h = StepHarness::start("stale").await;
    // Active (Queued) entry for the old SHA (commit A, on main —
    // fetchable, so the seed is a valid stored target); the wire
    // reports the fixture's real tip commit C as the new head.
    let old_target = target(&h.base_sha, &h.base_sha, &h.base_sha);
    let outcome = h.store.enqueue_review(&old_target).expect("seed active");
    assert!(matches!(
        outcome,
        caduceus::state::review::ReviewEnqueueOutcome::Inserted
    ));

    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([h.row("r", 7, &h.tip_sha)])),
        )
        .mount(&h.server)
        .await;

    let remote_url = h.remote_url();
    let stats = poll_review_step_for_tests(
        &["owner/r".to_string()],
        &h.client(),
        &h.cfg,
        &h.store,
        &GitRunner::new(&h.cfg),
        &move |_o, _r| Ok(remote_url.clone()),
    )
    .await
    .expect("stale path admits");

    assert_eq!(stats.stale_observed, 1, "D6 stale observation");
    assert_eq!(stats.admitted, 1, "new head admits");
    let queue = h.store.review_queue_snapshot().expect("snapshot");
    let fresh = queue
        .entries
        .values()
        .find(|e| e.target.head_sha == h.tip_sha)
        .expect("new-head entry");
    assert_eq!(fresh.review_generation, 2, "generation advanced");
}

#[tokio::test]
async fn pagination_follows_link_header() {
    let h = StepHarness::start("pagination").await;
    // Page 1 carries a `Link: rel="next"` pointing at page 2 on the
    // mock's own URI (the MockGitHub::mount_paged shape, hand-rolled
    // because StepHarness holds the raw MockServer). Both rows carry
    // real fixture SHAs (tip C, mid B) so both admit.
    let page2_url = format!("{}/repos/owner/r/pulls?page=2", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .and(wiremock::matchers::query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([h.row("r", 7, &h.tip_sha)]))
                .append_header("Link", format!("<{page2_url}>; rel=\"next\"")),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .and(wiremock::matchers::query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([h.row("r", 8, &h.mid_sha)])),
        )
        .mount(&h.server)
        .await;

    let remote_url = h.remote_url();
    let stats = poll_review_step_for_tests(
        &["owner/r".to_string()],
        &h.client(),
        &h.cfg,
        &h.store,
        &GitRunner::new(&h.cfg),
        &move |_o, _r| Ok(remote_url.clone()),
    )
    .await
    .expect("paginated listing works");

    assert_eq!(stats.discovered, 2, "both pages classified");
    assert_eq!(stats.admitted, 2, "both PRs admitted");
}

#[tokio::test]
async fn admission_follows_wire_order() {
    let h = StepHarness::start("wire-order").await;
    let mut cfg = h.cfg.clone();
    cfg.max_reviews_per_tick = 2;
    // Wire order (GitHub sort=updated desc): PR 9 (tip C), PR 8 (mid
    // B), PR 7 (base A). All three SHAs exist on the local remote, so
    // fetchability never decides — wire order + budget do.
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            h.row("r", 9, &h.tip_sha),
            h.row("r", 8, &h.mid_sha),
            h.row("r", 7, &h.base_sha),
        ])))
        .mount(&h.server)
        .await;

    let remote_url = h.remote_url();
    let stats = poll_review_step_for_tests(
        &["owner/r".to_string()],
        &h.client(),
        &cfg,
        &h.store,
        &GitRunner::new(&cfg),
        &move |_o, _r| Ok(remote_url.clone()),
    )
    .await
    .expect("ordered admissions");

    assert_eq!(stats.admitted, 2, "budget admits the first two wire rows");
    assert!(stats.budget_exhausted, "third row deferred to next tick");
    let queue = h.store.review_queue_snapshot().expect("snapshot");
    let mut admitted_prs: Vec<u64> = queue
        .entries
        .values()
        .map(|e| e.target.pull_request)
        .collect();
    admitted_prs.sort();
    assert_eq!(
        admitted_prs,
        vec![8, 9],
        "wire (list) order decides which rows win the budget"
    );
}

#[tokio::test]
async fn draft_pr_skips_with_event_and_no_admission() {
    let h = StepHarness::start("draft-skip").await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([with_draft(
                pulls_row_open(7, "bbbb"),
                true
            )])),
        )
        .mount(&h.server)
        .await;

    let stats = poll_review_step_for_tests(
        &["owner/r".to_string()],
        &h.client(),
        &h.cfg,
        &h.store,
        &GitRunner::new(&h.cfg),
        &|_o, _r| Err(CaduceusError::Config("resolver must not run".to_string())),
    )
    .await
    .expect("draft skip is not an error");

    assert_eq!(stats.skipped_draft, 1);
    assert_eq!(stats.admitted, 0);
}

#[tokio::test]
async fn closed_pr_is_ineligible_never_admitted() {
    let h = StepHarness::start("closed-skip").await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([with_state(
                pulls_row_open(7, "bbbb"),
                "closed"
            )])),
        )
        .mount(&h.server)
        .await;

    let stats = poll_review_step_for_tests(
        &["owner/r".to_string()],
        &h.client(),
        &h.cfg,
        &h.store,
        &GitRunner::new(&h.cfg),
        &|_o, _r| Err(CaduceusError::Config("resolver must not run".to_string())),
    )
    .await
    .expect("closed rows are not errors");

    assert_eq!(stats.ineligible, 1);
    assert_eq!(stats.admitted, 0);
    assert!(
        h.store
            .review_queue_snapshot()
            .expect("snap")
            .entries
            .is_empty(),
        "DAR §5.1: discovery-time closed PR never admitted"
    );
}

#[tokio::test]
async fn disabled_auto_review_makes_no_requests() {
    let h = StepHarness::start("disabled").await;
    let mut cfg = h.cfg.clone();
    cfg.auto_review = Some(ar_config(false));

    let stats = poll_review_step_for_tests(
        &["owner/r".to_string()],
        &h.client(),
        &cfg,
        &h.store,
        &GitRunner::new(&cfg),
        &|_o, _r| Err(CaduceusError::Config("resolver must not run".to_string())),
    )
    .await
    .expect("disabled step is a no-op");

    assert_eq!(
        stats,
        caduceus::daemon::tick::review_discovery::ReviewDiscoveryStats::default()
    );
    assert!(
        h.server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "no HTTP request when auto_review disabled"
    );
}

// ---------------------------------------------------------------------------
// Task 7 — tick wiring (AC1/AC3), D14
// ---------------------------------------------------------------------------

fn tick_cfg(base: &Path, api_base: &str, auto_review: Option<AutoReviewConfig>) -> Config {
    let raw = RawConfig {
        worker_command: Some(vec!["/bin/true".to_string()]),
        state_dir: Some(base.join("state")),
        workdir_base: Some(base.to_path_buf()),
        watched_repos: Some(vec!["owner/r".to_string()]),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let ctx = LoadContext {
        plugin_root: Some(base.to_path_buf()),
        ..Default::default()
    };
    let mut cfg = Config::from_raw(raw, &ctx).expect("config");
    cfg.api_base = api_base.to_string();
    cfg.auto_review = auto_review;
    cfg
}

async fn run_tick(
    cfg: Config,
    server: &MockServer,
) -> caduceus::error::CaduceusResult<TickOutcome> {
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(server)
        .await;
    let mut cfg = cfg;
    cfg.api_base = server.uri();
    let cache = HttpCache::open(&cfg.state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    let clock: Arc<dyn caduceus::orchestration::Clock> = Arc::new(SystemClock);
    let git = GitRunner::new(&cfg);
    let pool = Arc::new(
        Pool::new(
            cfg.worker_parallelism,
            DrainConfig::from_seconds_and_ms(cfg.drain_timeout_seconds, cfg.backpressure_budget_ms),
        )
        .with_lease_store_dir(
            cfg.state_dir.clone(),
            std::time::Duration::from_secs(cfg.worker_lease_ttl_seconds),
        ),
    );
    let services = caduceus::orchestration::Services::production(
        &cfg,
        clock,
        Arc::new(client),
        git,
        Arc::clone(&pool),
        Arc::new(caduceus::infra::disk::DiskPressureGuard::disabled()),
    );
    caduceus::tick::tick(cfg, services, pool, CancellationToken::new()).await
}

async fn request_paths(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|req| req.url.path().to_string())
        .collect()
}

#[tokio::test]
async fn tick_polls_issues_then_pulls_in_order() {
    let base = tempdir("tick-order");
    let server = MockServer::start().await;
    let mut cfg = tick_cfg(&base, &server.uri(), Some(ar_config(true)));
    cfg.repo_storage_root = base.join("repos");
    cfg.git_timeout_seconds = 30;

    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let outcome = run_tick(cfg, &server).await.expect("tick");
    assert_eq!(outcome, TickOutcome::IdleEmpty);

    let paths = request_paths(&server).await;
    let issues = paths
        .iter()
        .position(|p| p.contains("/issues"))
        .expect("issues polled");
    let pulls = paths
        .iter()
        .position(|p| p.ends_with("/pulls"))
        .expect("pulls polled");
    assert!(
        issues < pulls,
        "AC1: issues must be polled before pulls; got {paths:?}"
    );
}

#[tokio::test]
async fn discovery_skipped_when_disabled_or_dry_run() {
    // Disabled.
    let base = tempdir("tick-disabled");
    let server = MockServer::start().await;
    let mut cfg = tick_cfg(&base, &server.uri(), Some(ar_config(false)));
    cfg.repo_storage_root = base.join("repos");
    run_tick(cfg.clone(), &server).await.expect("tick");
    assert!(
        !request_paths(&server)
            .await
            .iter()
            .any(|p| p.ends_with("/pulls")),
        "no /pulls request when disabled"
    );

    // Dry run.
    let base = tempdir("tick-dryrun");
    let server = MockServer::start().await;
    let mut cfg = tick_cfg(&base, &server.uri(), Some(ar_config(true)));
    cfg.dry_run = true;
    cfg.repo_storage_root = base.join("repos");
    run_tick(cfg, &server).await.expect("tick");
    assert!(
        !request_paths(&server)
            .await
            .iter()
            .any(|p| p.ends_with("/pulls")),
        "no /pulls request in dry run"
    );
}

#[tokio::test]
async fn corrupt_review_store_does_not_break_tick() {
    let base = tempdir("tick-corrupt-store");
    let server = MockServer::start().await;
    let mut cfg = tick_cfg(&base, &server.uri(), Some(ar_config(true)));
    cfg.repo_storage_root = base.join("repos");
    cfg.git_timeout_seconds = 30;

    // One queued issue so the drain has work to consider.
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "number": 1,
                "title": "queued issue",
                "state": "open",
                "labels": [{"name": cfg.ticket_label_code.clone()}],
                "updated_at": "2020-01-01T00:00:00Z",
                "user": {"login": "octocat"}
            }
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    // Corrupt the review queue so the PR step's store open/read
    // fails. The tick has not run yet, so the state directory must
    // exist first (ReviewStore::open creates it lazily).
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    std::fs::write(cfg.state_dir.join("review_queue.json"), b"not valid json")
        .expect("corrupt review queue");

    let outcome = run_tick(cfg, &server).await.expect("tick still Ok (AC3)");
    assert_eq!(outcome, TickOutcome::IdleEmpty, "drain still ran");
}

#[tokio::test]
async fn pr_step_rate_limit_is_persisted_and_drain_runs() {
    let base = tempdir("tick-pr-rate-limit");
    let server = MockServer::start().await;
    let mut cfg = tick_cfg(&base, &server.uri(), Some(ar_config(true)));
    cfg.repo_storage_root = base.join("repos");
    cfg.git_timeout_seconds = 30;

    Mock::given(method("GET"))
        .and(path("/repos/owner/r/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    let reset_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-RateLimit-Remaining", "0")
                .insert_header("X-RateLimit-Reset", reset_at_unix.to_string())
                .insert_header("X-RateLimit-Limit", "5000")
                .set_body_string("[]"),
        )
        .mount(&server)
        .await;

    let outcome = run_tick(cfg, &server).await.expect("tick returns Ok (AC3)");
    assert_eq!(outcome, TickOutcome::IdleEmpty, "drain still ran");

    // D14: the PR step's rate-limit observation is persisted for the
    // next tick's cadence gate. The gate reads the same state_dir the
    // tick used (cfg.state_dir == base/state).
    let gate = caduceus::state::meta::CadenceGate::open(&base.join("state")).expect("gate opens");
    let snap = gate.store().snapshot();
    let obs = snap
        .rate_limit
        .expect("D14: PR-step rate limit persisted via last_error");
    assert_eq!(obs.remaining, 0);
}

#[test]
fn finish_tick_outcome_persists_rate_limit_from_last_error() {
    let base = tempdir("d14-unit");
    let state_dir = base.join("state").join("state");
    let gate = caduceus::state::meta::CadenceGate::open(&state_dir).expect("gate opens");
    let meta = MetaStore::open(&state_dir).expect("meta opens");
    let now = Utc::now();
    let err = CaduceusError::RateLimited {
        reset_at: 600,
        remaining: 0,
        limit: Some(5000),
    };

    caduceus::daemon::tick::awaiting_review::finish_tick_outcome_for_tests(
        &gate,
        &meta,
        now,
        TickOutcome::IdleEmpty,
        None,
        Some(&err),
    )
    .expect("finish succeeds");

    let snap = gate.store().snapshot();
    let obs = snap
        .rate_limit
        .expect("D14: rate-limit observation persisted from last_error");
    assert_eq!(obs.remaining, 0);
    assert_eq!(
        obs.reset_at.timestamp(),
        now.timestamp() + 600,
        "reset_at_unix = observed_at + reset_at"
    );
}

//! PR fixture validation tests (issue #327, plan T1 verification).
//!
//! The committed JSON bodies under `tests/fixtures/pr/` are the shared
//! GitHub PR corpus for the discovery tests and #322 E2E. This binary
//! pins their contract: each single-row fixture parses into the typed
//! `PullRequestDetail` (and carries the discriminating field its DAR
//! §5.1 row exists for), the paginated pair parses as list pages, and
//! the malformed corpus is rejected as a parse error.

use caduceus::config::Config;
use caduceus::github::pr::{list_pull_requests, PullRequestDetail};
use caduceus::github::{Client, HttpCache};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";

fn client_for(gh: &fixtures::MockGitHub, state_dir: &std::path::Path) -> Client {
    let mut cfg = Config::test_defaults(state_dir);
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(state_dir).expect("cache opens");
    Client::with_cache(&cfg, cache).expect("client builds")
}

// ---------------------------------------------------------------------------
// Committed corpus (compile-time embedded; hermetic, no CWD dependence)
// ---------------------------------------------------------------------------

const OPEN: &str = include_str!("../fixtures/pr/open.json");
const DRAFT: &str = include_str!("../fixtures/pr/draft.json");
const FORK: &str = include_str!("../fixtures/pr/fork.json");
const CLOSED: &str = include_str!("../fixtures/pr/closed.json");
const MERGED: &str = include_str!("../fixtures/pr/merged.json");
const NULLABLE_HEAD_REPO: &str = include_str!("../fixtures/pr/nullable-head-repo.json");
const FORCE_PUSHED: &str = include_str!("../fixtures/pr/force-pushed.json");
const MALFORMED: &str = include_str!("../fixtures/pr/malformed.json");
const PAGINATED_1: &str = include_str!("../fixtures/pr/paginated-1.json");
const PAGINATED_2: &str = include_str!("../fixtures/pr/paginated-2.json");
const RATE_LIMITED: &str = include_str!("../fixtures/pr/rate-limited.json");

fn parse_row(body: &str) -> PullRequestDetail {
    serde_json::from_str(body).expect("PR fixture parses into PullRequestDetail")
}

// ---------------------------------------------------------------------------
// Single-row fixtures parse and carry their discriminating field
// ---------------------------------------------------------------------------

#[test]
fn open_fixture_is_open_non_draft() {
    let pr = parse_row(OPEN);
    assert_eq!(pr.state.as_deref(), Some("open"));
    assert!(!pr.draft);
    assert_eq!(pr.number, Some(7));
}

#[test]
fn draft_fixture_is_draft() {
    let pr = parse_row(DRAFT);
    assert!(pr.draft, "draft fixture must carry draft=true");
}

#[test]
fn fork_fixture_has_distinct_head_and_base_repos() {
    let pr = parse_row(FORK);
    let head_repo = pr
        .head
        .as_ref()
        .and_then(|h| h.repo.as_ref())
        .and_then(|r| r.full_name.as_deref());
    let base_repo = pr
        .base
        .as_ref()
        .and_then(|b| b.repo.as_ref())
        .and_then(|r| r.full_name.as_deref());
    assert_ne!(
        head_repo, base_repo,
        "fork fixture must have head.repo != base.repo"
    );
}

#[test]
fn closed_fixture_is_closed_not_merged() {
    let pr = parse_row(CLOSED);
    assert_eq!(pr.state.as_deref(), Some("closed"));
    assert_eq!(pr.merged, Some(false));
}

#[test]
fn merged_fixture_is_merged() {
    let pr = parse_row(MERGED);
    assert_eq!(pr.merged, Some(true));
    assert!(pr.merged_at.is_some(), "merged fixture carries merged_at");
}

#[test]
fn nullable_head_repo_fixture_has_null_head_repo() {
    let pr = parse_row(NULLABLE_HEAD_REPO);
    let head = pr.head.expect("head branch present");
    assert!(
        head.repo.is_none(),
        "nullable-head-repo fixture must carry head.repo: null"
    );
}

#[test]
fn force_pushed_fixture_has_distinct_shas() {
    let pr = parse_row(FORCE_PUSHED);
    let head_sha = pr.head.as_ref().and_then(|h| h.sha.as_deref());
    let base_sha = pr.base.as_ref().and_then(|b| b.sha.as_deref());
    assert_ne!(
        head_sha, base_sha,
        "force-pushed fixture must have head sha != base sha"
    );
}

// ---------------------------------------------------------------------------
// Paginated pair parses as list pages
// ---------------------------------------------------------------------------

#[test]
fn paginated_fixtures_parse_as_list_pages() {
    for (name, body) in [("paginated-1", PAGINATED_1), ("paginated-2", PAGINATED_2)] {
        let rows: Vec<PullRequestDetail> =
            serde_json::from_str(body).unwrap_or_else(|e| panic!("{name} parses as list: {e}"));
        assert!(!rows.is_empty(), "{name} must carry at least one PR row");
    }
}

// ---------------------------------------------------------------------------
// Malformed + rate-limited corpora exercise the failure paths
// ---------------------------------------------------------------------------

#[test]
fn malformed_fixture_is_rejected_by_the_typed_parser() {
    // The committed corpus is intentionally wrong-shaped; the typed
    // parse must fail (the daemon surfaces this as a parse error).
    assert!(
        serde_json::from_str::<PullRequestDetail>(MALFORMED).is_err(),
        "malformed fixture must not parse as PullRequestDetail"
    );
    assert!(
        serde_json::from_str::<Vec<PullRequestDetail>>(MALFORMED).is_err(),
        "malformed fixture must not parse as a PR list either"
    );
}

#[test]
fn rate_limited_fixture_is_the_429_body() {
    let body: serde_json::Value =
        serde_json::from_str(RATE_LIMITED).expect("rate-limited fixture is valid JSON");
    assert!(
        body["message"].as_str().is_some(),
        "429 body carries the GitHub error message shape"
    );
}

// ---------------------------------------------------------------------------
// Round-trip through the real wire path (MockGitHub mounts the corpus)
// ---------------------------------------------------------------------------

/// The corpus is served through the same `MockGitHub` surface the
/// discovery tests use: mounting `open.json` as the `/pulls` list and
/// parsing it through `list_pull_requests` yields the typed row.
#[tokio::test]
async fn corpus_mounts_through_mock_github_and_lists() {
    let gh = fixtures::MockGitHub::start().await;
    let open_row: serde_json::Value = serde_json::from_str(OPEN).unwrap();
    gh.mount("GET", "/repos/owner/r/pulls", serde_json::json!([open_row]))
        .await;

    let state_dir = tempdir("pr-fixtures-open");
    let client = client_for(&gh, &state_dir);
    let prs = list_pull_requests(&client, "owner", "r")
        .await
        .expect("list succeeds over the mounted fixture");
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].state.as_deref(), Some("open"));
    assert!(!prs[0].draft);
}

/// The paginated pair served through `mount_paged` lists as two pages
/// totalling both fixtures' rows (R4: query-param routing).
#[tokio::test]
async fn paginated_corpus_mounts_through_mount_paged() {
    let gh = fixtures::MockGitHub::start().await;
    gh.mount_paged(
        "/repos/owner/r/pulls",
        vec![
            serde_json::from_str::<serde_json::Value>(PAGINATED_1).unwrap(),
            serde_json::from_str::<serde_json::Value>(PAGINATED_2).unwrap(),
        ],
    )
    .await;

    let state_dir = tempdir("pr-fixtures-paged");
    let client = client_for(&gh, &state_dir);
    let prs = list_pull_requests(&client, "owner", "r")
        .await
        .expect("paged list succeeds");
    let page1_rows = serde_json::from_str::<Vec<PullRequestDetail>>(PAGINATED_1)
        .unwrap()
        .len();
    let page2_rows = serde_json::from_str::<Vec<PullRequestDetail>>(PAGINATED_2)
        .unwrap()
        .len();
    assert_eq!(
        prs.len(),
        page1_rows + page2_rows,
        "the list loop must follow the Link header across both pages"
    );
}

/// The rate-limited body mounted with a 429 status surfaces the typed
/// rate-limit error (DAR §5.1 row).
#[tokio::test]
async fn rate_limited_corpus_surfaces_typed_error() {
    let gh = fixtures::MockGitHub::start().await;
    gh.mount_status(
        "GET",
        "/repos/owner/r/pulls",
        429,
        serde_json::from_str::<serde_json::Value>(RATE_LIMITED).unwrap(),
    )
    .await;

    let client = Client::new(gh.uri().as_str());
    let err = list_pull_requests(&client, "owner", "r")
        .await
        .expect_err("429 surfaces a typed error");
    assert!(
        matches!(err, caduceus::error::CaduceusError::RateLimited { .. }),
        "429 must surface CaduceusError::RateLimited, got: {err:?}"
    );
}

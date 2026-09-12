//! Sticky-comment publish flow tests (issue #308, DAR §9.2–9.3).
//!
//! Coverage:
//!
//! - AC1: first-time create; update by id; idempotent re-finalization
//!   (byte-identical body → `Unchanged`, no PATCH).
//! - AC3: gone-state A — PATCH 404 → marker search → create-new →
//!   new id; crash-heal adoption (id None, marker present → PATCH the
//!   adopted id, no duplicate create).
//! - AC4: gone-states B (PR 404 → `PrNotFound`, zero HTTP), C
//!   (closed-unmerged → `PrClosedUnmerged`, zero HTTP), D (merged →
//!   publishes).
//! - Voice gate: forbidden term → error before any HTTP.
//! - Marker search: paged scan finds the marker id; page-cap error.

use caduceus::config::{Config, PublicationMode};
use caduceus::github::merge_detect::MergeStatus;
use caduceus::github::{Client, HttpCache};
use caduceus::review::sticky_comment::{
    find_sticky_comment_by_marker, marker_for_generation, publish, render_sticky_comment,
    MarkerTarget, RenderInput, StickyOutcome, REVIEW_MARKER, STICKY_MARKER_SEARCH_MAX_PAGES,
};
use caduceus::review::{RepositoryId, Review, ReviewState, Severity, Verdict};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::{tempdir, MockGitHub};

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";

fn mock_client(gh: &MockGitHub) -> (Client, Config) {
    let state_dir = tempdir("sticky-publish");
    let mut cfg = Config::test_defaults(&state_dir);
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    (client, cfg)
}

fn sample_state() -> RepositoryId {
    RepositoryId {
        owner: "octocat".to_string(),
        repo: "hello-world".to_string(),
    }
}

fn review_state(sticky_comment_id: Option<u64>) -> ReviewState {
    let mut state = ReviewState::new(sample_state(), 42, 7);
    state.sticky_comment_id = sticky_comment_id;
    state
}

fn review() -> Review {
    Review {
        verdict: Verdict::Fail,
        summary: "found problems".to_string(),
        findings: vec![caduceus::review::Finding {
            severity: Severity::Blocking,
            title: "unsafe".to_string(),
            body: "danger".to_string(),
            path: Some("src/lib.rs".to_string()),
            line: Some(3),
            remediation: Some("fix it".to_string()),
        }],
    }
}

fn render_input<'a>(r: &'a Review) -> RenderInput<'a> {
    RenderInput {
        review: r,
        reviewed_head_sha: "abc123",
        current_head_sha: None,
        review_generation: 1,
        publication_mode: PublicationMode::Update,
    }
}

fn comment_json(id: u64, body: &str) -> serde_json::Value {
    serde_json::json!({ "id": id, "body": body })
}

// -----------------------------------------------------------------------
// Marker search
// -----------------------------------------------------------------------

#[tokio::test]
async fn marker_search_finds_marker_comment() {
    let gh = MockGitHub::start().await;
    gh.mount_paged(
        "/repos/octocat/hello-world/issues/42/comments",
        vec![
            serde_json::json!([
                comment_json(1, "a human comment"),
                comment_json(2, "another one"),
            ]),
            serde_json::json!([
                comment_json(3, "third"),
                comment_json(99, &format!("review\n{REVIEW_MARKER}\nverdict")),
            ]),
        ],
    )
    .await;
    let (client, _cfg) = mock_client(&gh);
    let found =
        find_sticky_comment_by_marker(&client, "octocat", "hello-world", 42, MarkerTarget::Latest)
            .await
            .expect("search succeeds");
    assert_eq!(found, Some(99), "marker comment id found on page 2");
}

#[tokio::test]
async fn marker_search_returns_none_when_absent() {
    let gh = MockGitHub::start().await;
    gh.mount_paged(
        "/repos/octocat/hello-world/issues/42/comments",
        vec![serde_json::json!([comment_json(1, "no marker here")])],
    )
    .await;
    let (client, _cfg) = mock_client(&gh);
    let found =
        find_sticky_comment_by_marker(&client, "octocat", "hello-world", 42, MarkerTarget::Latest)
            .await
            .expect("search succeeds");
    assert_eq!(found, None);
}

#[tokio::test]
async fn marker_search_errors_past_page_cap() {
    let gh = MockGitHub::start().await;
    // One more page than the cap; no page carries the marker, so the
    // scan must run off the end and trip the cap error.
    let pages: Vec<serde_json::Value> = (0..STICKY_MARKER_SEARCH_MAX_PAGES + 1)
        .map(|p| serde_json::json!([comment_json(p as u64, &format!("page {p} without marker"))]))
        .collect();
    gh.mount_paged("/repos/octocat/hello-world/issues/42/comments", pages)
        .await;
    let (client, _cfg) = mock_client(&gh);
    let err =
        find_sticky_comment_by_marker(&client, "octocat", "hello-world", 42, MarkerTarget::Latest)
            .await
            .expect_err("page cap trips");
    assert!(
        err.to_string().contains("pages"),
        "cap error mentions pages: {err}"
    );
}

#[tokio::test]
async fn latest_target_adopts_highest_generation_not_first_match() {
    let gh = MockGitHub::start().await;
    gh.mount_paged(
        "/repos/octocat/hello-world/issues/42/comments",
        vec![serde_json::json!([
            comment_json(11, &format!("review\n{REVIEW_MARKER}\nverdict")),
            comment_json(12, &format!("old\n{}\nbody", marker_for_generation(1))),
            comment_json(13, &format!("new\n{}\nbody", marker_for_generation(2))),
        ])],
    )
    .await;
    let (client, _cfg) = mock_client(&gh);
    let found =
        find_sticky_comment_by_marker(&client, "octocat", "hello-world", 42, MarkerTarget::Latest)
            .await
            .expect("search succeeds");
    assert_eq!(found, Some(13), "latest generation wins over first match");
}

#[tokio::test]
async fn generation_target_finds_exact_generation_early() {
    let gh = MockGitHub::start().await;
    gh.mount_paged(
        "/repos/octocat/hello-world/issues/42/comments",
        vec![serde_json::json!([
            comment_json(11, &format!("a\n{}\nb", marker_for_generation(1))),
            comment_json(12, &format!("b\n{}\nb", marker_for_generation(2))),
            comment_json(13, &format!("c\n{}\nb", marker_for_generation(3))),
        ])],
    )
    .await;
    let (client, _cfg) = mock_client(&gh);
    let found = find_sticky_comment_by_marker(
        &client,
        "octocat",
        "hello-world",
        42,
        MarkerTarget::Generation(2),
    )
    .await
    .expect("search succeeds");
    assert_eq!(found, Some(12), "exact generation match");
}

#[tokio::test]
async fn generation_target_returns_none_when_generation_absent() {
    let gh = MockGitHub::start().await;
    gh.mount_paged(
        "/repos/octocat/hello-world/issues/42/comments",
        vec![serde_json::json!([comment_json(
            11,
            &format!("a\n{}\nb", marker_for_generation(1))
        ),])],
    )
    .await;
    let (client, _cfg) = mock_client(&gh);
    let found = find_sticky_comment_by_marker(
        &client,
        "octocat",
        "hello-world",
        42,
        MarkerTarget::Generation(4),
    )
    .await
    .expect("search succeeds");
    assert_eq!(found, None, "no gen-4 comment exists");
}

#[tokio::test]
async fn latest_target_adopts_legacy_untagged_comment() {
    // Pre-#394 PRs carry the untagged marker; it parses as gen 0 and
    // is adopted when it is the only marker comment.
    let gh = MockGitHub::start().await;
    gh.mount_paged(
        "/repos/octocat/hello-world/issues/42/comments",
        vec![serde_json::json!([
            comment_json(11, "human"),
            comment_json(12, &format!("legacy\n{REVIEW_MARKER}\nbody")),
        ])],
    )
    .await;
    let (client, _cfg) = mock_client(&gh);
    let found =
        find_sticky_comment_by_marker(&client, "octocat", "hello-world", 42, MarkerTarget::Latest)
            .await
            .expect("search succeeds");
    assert_eq!(found, Some(12), "legacy untagged comment adopted");
}

// -----------------------------------------------------------------------
// AC1: create / update / idempotent re-finalization
// -----------------------------------------------------------------------

#[tokio::test]
async fn first_publish_creates_comment() {
    let gh = MockGitHub::start().await;
    gh.mount_paged(
        "/repos/octocat/hello-world/issues/42/comments",
        vec![serde_json::json!([])],
    )
    .await;
    gh.mount_status(
        "POST",
        "/repos/octocat/hello-world/issues/42/comments",
        201,
        comment_json(777, ""),
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    let r = review();
    let outcome = publish(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        42,
        &review_state(None),
        &render_input(&r),
        MergeStatus::StillOpen,
    )
    .await
    .expect("publish succeeds");
    assert_eq!(
        outcome,
        StickyOutcome::Published { comment_id: 777 },
        "first-time create"
    );
}

#[tokio::test]
async fn update_by_id_patches_when_body_differs() {
    let gh = MockGitHub::start().await;
    let r = review();
    let new_body = render_sticky_comment(&render_input(&r));
    gh.mount_status(
        "GET",
        "/repos/octocat/hello-world/issues/comments/42",
        200,
        comment_json(42, "an older, different body"),
    )
    .await;
    gh.mount_status(
        "PATCH",
        "/repos/octocat/hello-world/issues/comments/42",
        200,
        comment_json(42, &new_body),
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    let outcome = publish(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        42,
        &review_state(Some(42)),
        &render_input(&r),
        MergeStatus::StillOpen,
    )
    .await
    .expect("publish succeeds");
    assert_eq!(
        outcome,
        StickyOutcome::Published { comment_id: 42 },
        "update by id"
    );
    let counts = gh.counts();
    assert_eq!(counts.patch, 1, "exactly one PATCH");
}

#[tokio::test]
async fn identical_body_is_unchanged_without_patch() {
    let gh = MockGitHub::start().await;
    let r = review();
    let body = render_sticky_comment(&render_input(&r));
    gh.mount_status(
        "GET",
        "/repos/octocat/hello-world/issues/comments/42",
        200,
        comment_json(42, &body),
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    let outcome = publish(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        42,
        &review_state(Some(42)),
        &render_input(&r),
        MergeStatus::StillOpen,
    )
    .await
    .expect("publish succeeds");
    assert_eq!(
        outcome,
        StickyOutcome::Unchanged { comment_id: 42 },
        "idempotent re-finalization"
    );
    let counts = gh.counts();
    assert_eq!(counts.patch, 0, "no PATCH when byte-identical");
    assert_eq!(counts.post, 0, "no create when byte-identical");
}

// -----------------------------------------------------------------------
// AC3: gone-state A — comment gone → recreate; crash-heal adoption
// -----------------------------------------------------------------------

#[tokio::test]
async fn stale_id_404_recreates_via_marker() {
    // sticky_comment_id points at a deleted comment. Marker search
    // finds comment 99 → adopt via PATCH (no duplicate create).
    let gh = MockGitHub::start().await;
    let r = review();
    let body = render_sticky_comment(&render_input(&r));
    gh.mount_status(
        "GET",
        "/repos/octocat/hello-world/issues/comments/42",
        404,
        serde_json::json!({ "message": "Not Found" }),
    )
    .await;
    gh.mount_paged(
        "/repos/octocat/hello-world/issues/42/comments",
        vec![serde_json::json!([comment_json(
            99,
            &format!("old review\n{REVIEW_MARKER}")
        )])],
    )
    .await;
    gh.mount_status(
        "PATCH",
        "/repos/octocat/hello-world/issues/comments/99",
        200,
        comment_json(99, &body),
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    let outcome = publish(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        42,
        &review_state(Some(42)),
        &render_input(&r),
        MergeStatus::StillOpen,
    )
    .await
    .expect("publish succeeds");
    assert_eq!(
        outcome,
        StickyOutcome::CommentGoneRecreated { new_comment_id: 99 },
        "adopted the marker comment"
    );
    let counts = gh.counts();
    assert_eq!(counts.post, 0, "no duplicate create when adoption works");
    assert_eq!(counts.patch, 1, "one PATCH onto the adopted id");
}

#[tokio::test]
async fn stale_id_404_creates_new_when_marker_absent() {
    let gh = MockGitHub::start().await;
    let r = review();
    gh.mount_status(
        "GET",
        "/repos/octocat/hello-world/issues/comments/42",
        404,
        serde_json::json!({ "message": "Not Found" }),
    )
    .await;
    gh.mount_paged(
        "/repos/octocat/hello-world/issues/42/comments",
        vec![serde_json::json!([comment_json(5, "no marker")])],
    )
    .await;
    gh.mount_status(
        "POST",
        "/repos/octocat/hello-world/issues/42/comments",
        201,
        comment_json(888, ""),
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    let outcome = publish(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        42,
        &review_state(Some(42)),
        &render_input(&r),
        MergeStatus::StillOpen,
    )
    .await
    .expect("publish succeeds");
    assert_eq!(
        outcome,
        StickyOutcome::CommentGoneRecreated {
            new_comment_id: 888
        },
        "create-new when no marker exists"
    );
}

#[tokio::test]
async fn crash_heal_adopts_orphaned_marker_comment() {
    // Crash between create and id-persist: sticky_comment_id is None
    // but the marker comment exists on GitHub. publish must adopt it
    // (PATCH) and NOT create a duplicate.
    let gh = MockGitHub::start().await;
    let r = review();
    let body = render_sticky_comment(&render_input(&r));
    gh.mount_paged(
        "/repos/octocat/hello-world/issues/42/comments",
        vec![serde_json::json!([comment_json(
            99,
            &format!("{REVIEW_MARKER}\nolder body")
        )])],
    )
    .await;
    gh.mount_status(
        "GET",
        "/repos/octocat/hello-world/issues/comments/99",
        200,
        comment_json(99, "older body on the wire"),
    )
    .await;
    gh.mount_status(
        "PATCH",
        "/repos/octocat/hello-world/issues/comments/99",
        200,
        comment_json(99, &body),
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    let outcome = publish(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        42,
        &review_state(None),
        &render_input(&r),
        MergeStatus::StillOpen,
    )
    .await
    .expect("publish succeeds");
    assert_eq!(
        outcome,
        StickyOutcome::Published { comment_id: 99 },
        "adopted the orphaned marker comment"
    );
    let counts = gh.counts();
    assert_eq!(counts.post, 0, "no duplicate create");
    assert_eq!(counts.patch, 1, "adoption PATCH");
}

#[tokio::test]
async fn crash_heal_identical_body_is_unchanged() {
    // Crash-heal where the orphaned comment already carries the exact
    // body: adoption returns Unchanged, no mutation at all.
    let gh = MockGitHub::start().await;
    let r = review();
    let body = render_sticky_comment(&render_input(&r));
    gh.mount_status(
        "GET",
        "/repos/octocat/hello-world/issues/comments/99",
        200,
        comment_json(99, &body),
    )
    .await;
    gh.mount_paged(
        "/repos/octocat/hello-world/issues/42/comments",
        vec![serde_json::json!([comment_json(
            99,
            &format!("{REVIEW_MARKER}\nplaceholder")
        )])],
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    let outcome = publish(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        42,
        &review_state(None),
        &render_input(&r),
        MergeStatus::StillOpen,
    )
    .await
    .expect("publish succeeds");
    assert_eq!(outcome, StickyOutcome::Unchanged { comment_id: 99 });
    let counts = gh.counts();
    assert_eq!(counts.mutations(), 0, "no mutation for identical body");
}

// -----------------------------------------------------------------------
// AC4: gone-states B / C / D
// -----------------------------------------------------------------------

#[tokio::test]
async fn pr_not_found_is_quiet_skip_with_no_http() {
    let gh = MockGitHub::start().await;
    let (client, cfg) = mock_client(&gh);
    let r = review();
    let outcome = publish(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        42,
        &review_state(Some(42)),
        &render_input(&r),
        MergeStatus::NotFound,
    )
    .await
    .expect("quiet skip is not an error");
    assert_eq!(outcome, StickyOutcome::PrNotFound);
    assert_eq!(gh.counts().mutations(), 0, "never recreate");
    assert_eq!(gh.counts().total(), 0, "no HTTP at all");
}

#[tokio::test]
async fn pr_closed_unmerged_is_quiet_skip_with_no_http() {
    let gh = MockGitHub::start().await;
    let (client, cfg) = mock_client(&gh);
    let r = review();
    let outcome = publish(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        42,
        &review_state(Some(42)),
        &render_input(&r),
        MergeStatus::ClosedWithoutMerge,
    )
    .await
    .expect("quiet skip is not an error");
    assert_eq!(outcome, StickyOutcome::PrClosedUnmerged);
    assert_eq!(gh.counts().mutations(), 0, "no mutation");
    assert_eq!(gh.counts().total(), 0, "no HTTP at all");
}

#[tokio::test]
async fn pr_merged_publishes() {
    let gh = MockGitHub::start().await;
    gh.mount_paged(
        "/repos/octocat/hello-world/issues/42/comments",
        vec![serde_json::json!([])],
    )
    .await;
    gh.mount_status(
        "POST",
        "/repos/octocat/hello-world/issues/42/comments",
        201,
        comment_json(901, ""),
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    let r = review();
    let outcome = publish(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        42,
        &review_state(None),
        &render_input(&r),
        MergeStatus::Merged {
            merge_commit_sha: "ff00ff".to_string(),
        },
    )
    .await
    .expect("publish succeeds for merged PR");
    assert_eq!(
        outcome,
        StickyOutcome::Published { comment_id: 901 },
        "merged PRs publish (state input for #310)"
    );
}

// -----------------------------------------------------------------------
// Voice gate (fires inside the wrappers before HTTP)
// -----------------------------------------------------------------------

#[tokio::test]
async fn voice_gate_blocks_publish_before_http() {
    let gh = MockGitHub::start().await;
    let (_client, mut cfg) = mock_client(&gh);
    cfg.comment_forbidden_strings = vec!["secret".to_string()];
    let state_dir = tempdir("sticky-voice");
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");

    // A review whose rendered body contains the forbidden term.
    let r = Review {
        verdict: Verdict::Pass,
        summary: "the secret is out".to_string(),
        findings: vec![],
    };
    let err = publish(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        42,
        &review_state(None),
        &render_input(&r),
        MergeStatus::StillOpen,
    )
    .await
    .expect_err("voice gate blocks");
    assert!(err.to_string().contains("forbidden"), "voice error: {err}");
    // The gate fires before the marker search hits the network: even
    // though nothing is mounted, no request was made.
    assert_eq!(gh.counts().total(), 0, "no HTTP before the gate");
}

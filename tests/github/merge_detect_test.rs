//! PR merge-status poll tests (issue #385).
//!
//! Coverage:
//!
//! - Fresh 200 polls classify open / merged / closed PRs.
//! - The ETag-cached second poll (server replies 304, the client
//!   replays the cached body) must classify identically to the fresh
//!   poll. A 304 means "unchanged", never a poll failure — the #385
//!   bug was the finalizer treating it as one and retrying forever.
//! - The 304 replay must not lose state: a cached merged or closed
//!   body still maps to `Merged` / `ClosedWithoutMerge`.

use caduceus::config::Config;
use caduceus::github::{poll_pr_merge_status, Client, HttpCache, MergeStatus};
use wiremock::matchers::{header, method, path};
use wiremock::{Match, Mock, Request, ResponseTemplate};

use fixtures::{tempdir, MockGitHub};
#[path = "../fixtures/mod.rs"]
mod fixtures;

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";
const OWNER: &str = "octocat";
const REPO: &str = "hello-world";
const PR_NUMBER: u64 = 42;
const PR_PATH: &str = "/repos/octocat/hello-world/pulls/42";
const ETAG: &str = "\"abc\"";

/// Matches requests that do NOT carry the named header. The first
/// (unconditional) GET has no `If-None-Match`; the second does. This
/// disambiguates the two mocks on the same path — same pattern as
/// `tests/github/github_client_test.rs`.
struct NoHeader(&'static str);

impl Match for NoHeader {
    fn matches(&self, req: &Request) -> bool {
        !req.headers.contains_key(self.0)
    }
}

fn mock_client(gh: &MockGitHub) -> Client {
    let state_dir = tempdir("merge-detect");
    let mut cfg = Config::test_defaults(&state_dir);
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(&state_dir).expect("cache opens");
    Client::with_cache(&cfg, cache).expect("client builds")
}

/// Mount the two-arm conditional-GET sequence on the PR endpoint:
/// 200 + ETag for the unconditional GET, 304 for the re-GET.
async fn mount_pulls_two_arm(gh: &MockGitHub, pr_body: &'static str) {
    Mock::given(method("GET"))
        .and(path(PR_PATH))
        .and(NoHeader("if-none-match"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", ETAG)
                .set_body_string(pr_body),
        )
        .expect(1)
        .mount(gh.server())
        .await;
    Mock::given(method("GET"))
        .and(path(PR_PATH))
        .and(header("if-none-match", ETAG))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(gh.server())
        .await;
}

#[tokio::test]
async fn fresh_200_then_conditional_304_polls_still_open_pr() {
    let gh = MockGitHub::start().await;
    mount_pulls_two_arm(&gh, r#"{"merged": false, "state": "open"}"#).await;

    let client = mock_client(&gh);
    let first = poll_pr_merge_status(&client, OWNER, REPO, PR_NUMBER)
        .await
        .expect("fresh poll classifies");
    assert_eq!(first, MergeStatus::StillOpen);

    // The arbiter regression for #385: the conditional re-GET 304s
    // and the client replays the cached body. This poll must NOT
    // fail (on current main it returns Err(GitHubApi { status: 304 })).
    let second = poll_pr_merge_status(&client, OWNER, REPO, PR_NUMBER)
        .await
        .expect("304 poll must not fail (issue #385)");
    assert_eq!(second, MergeStatus::StillOpen);
}

#[tokio::test]
async fn conditional_304_replay_preserves_merged_state() {
    let gh = MockGitHub::start().await;
    mount_pulls_two_arm(
        &gh,
        r#"{"merged": true, "state": "closed", "merge_commit_sha": "abc123"}"#,
    )
    .await;

    let client = mock_client(&gh);
    // Pre-warm the cache with the merged representation.
    let first = poll_pr_merge_status(&client, OWNER, REPO, PR_NUMBER)
        .await
        .expect("fresh poll classifies");
    assert_eq!(
        first,
        MergeStatus::Merged {
            merge_commit_sha: "abc123".to_string()
        }
    );

    // The 304 replay must preserve the merged classification — not
    // collapse it to StillOpen, not fail the poll. Proves Option A
    // loses no state (the reason Option B was rejected).
    let second = poll_pr_merge_status(&client, OWNER, REPO, PR_NUMBER)
        .await
        .expect("304 replay of a merged PR must not fail (issue #385)");
    assert_eq!(
        second,
        MergeStatus::Merged {
            merge_commit_sha: "abc123".to_string()
        },
        "304 replay must not lose the merged state"
    );
}

#[tokio::test]
async fn conditional_304_replay_preserves_closed_unmerged_state() {
    let gh = MockGitHub::start().await;
    mount_pulls_two_arm(&gh, r#"{"merged": false, "state": "closed"}"#).await;

    let client = mock_client(&gh);
    let first = poll_pr_merge_status(&client, OWNER, REPO, PR_NUMBER)
        .await
        .expect("fresh poll classifies");
    assert_eq!(first, MergeStatus::ClosedWithoutMerge);

    let second = poll_pr_merge_status(&client, OWNER, REPO, PR_NUMBER)
        .await
        .expect("304 replay of a closed PR must not fail (issue #385)");
    assert_eq!(second, MergeStatus::ClosedWithoutMerge);
}

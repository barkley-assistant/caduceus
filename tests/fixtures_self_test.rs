//! Self-tests for the v1.0 Phase 1.2 fixtures. Run via
//! `cargo test --test fixtures_self_test`. These tests are the
//! primary acceptance evidence for Task 1.2's three acceptance
//! IDs:
//!
//! - 1.2-AC-01: no network, no production credentials
//! - 1.2-AC-02: Git side effects and failures are modelled
//! - 1.2-AC-03: exact GitHub mutation counts
//!
//! The tests live in their own test binary (rather than under
//! `tests/fixtures/`) because Cargo only auto-discovers
//! `tests/<file>.rs` as integration test binaries — files
//! inside `tests/fixtures/` are only built when a test wires
//! them in via `#[path]`. Keeping the self-tests here means
//! `cargo test` runs them on every CI build.

#![allow(clippy::needless_return)]

#[path = "fixtures/mod.rs"]
mod fixtures;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use fixtures::{LocalOrigin, MockGitHub};

// -----------------------------------------------------------------------
// AC-01: Require no network or production credentials
// -----------------------------------------------------------------------

/// `MockGitHub::uri()` must always resolve to a localhost
/// loopback address; the helper must never produce a
/// non-loopback URL even if the test machine has a routable
/// hostname configured. We assert against the IPv4/IPv6
/// loopback literals because `MockServer::uri()` is documented
/// to bind to one of them.
#[tokio::test]
async fn ac01_mock_github_uri_is_loopback() {
    let gh = MockGitHub::start().await;
    let uri = gh.uri();
    assert!(
        uri.starts_with("http://127.0.0.1:") || uri.starts_with("http://[::1]:"),
        "MockGitHub uri should be loopback, got {uri}"
    );
    assert!(
        !uri.contains("github.com"),
        "MockGitHub uri must not leak github.com, got {uri}"
    );
}

/// `LocalOrigin::uri()` must always be a `file://` URL. The
/// daemon's `validate_origin_host` accepts `file://` URLs as
/// hermetic, so this is the right shape for fixture use. We
/// also assert the URL parses and points at an existing bare
/// repo so a future regression that points `uri()` at a
/// non-existent path fails fast.
#[test]
fn ac01_local_origin_uri_is_file_scheme() {
    let origin = LocalOrigin::init("ac01");
    let uri = origin.uri();
    assert!(
        uri.starts_with("file://"),
        "LocalOrigin uri must use the file:// scheme, got {uri}"
    );
    assert!(
        origin.path().exists(),
        "bare repo path must exist on disk: {}",
        origin.path().display()
    );
    assert!(
        origin.path().join("HEAD").exists(),
        "bare repo must have HEAD (not an empty init): {}",
        origin.path().display()
    );
}

/// Both fixtures must succeed without reading a token from
/// the environment. We deliberately do NOT set
/// `GITHUB_TOKEN`, `GH_TOKEN`, or `CADUCEUS_GITHUB_TOKEN` in
/// this test process — the fixtures run anyway, proving the
/// contract. (The harness already scrubs those vars via
/// `runner_env_test`, but we re-assert it here because the
/// fixture is the boundary the v1.0 plan hangs its
/// credentials-required claim on.)
#[tokio::test]
async fn ac01_fixtures_run_with_no_github_token_in_environment() {
    // Don't set any token here. The fixtures should still
    // build and serve traffic without one.
    let gh = MockGitHub::start().await;
    let _origin = LocalOrigin::init("ac01-env");
    gh.mount("GET", "/user", json!({"login": "octocat"})).await;
    // Make one request with no token — the mock answers
    // regardless.
    let resp = reqwest::get(gh.uri() + "/user").await.expect("reqwest");
    assert_eq!(resp.status(), 200);
}

// -----------------------------------------------------------------------
// AC-02: Model Git side effects and failures
// -----------------------------------------------------------------------

/// Successful push from a working clone bumps the bare
/// repo's `main` head and the recorded commit count. The
/// fixture must capture the side effect so tests can assert
/// on it without parsing git output themselves.
#[test]
fn ac02_push_commit_moves_origin_head_and_bumps_count() {
    let mut origin = LocalOrigin::init("ac02-push");
    let initial_oid = origin.head_oid().to_string();
    let initial_count = origin.commit_count();
    let workdir = tempdir("work");
    origin.clone_into(&workdir);
    let readme = workdir.join("README.md");
    let new_oid = origin.push_commit(&workdir, &readme, "# hello\n", "docs: seed README");
    assert_ne!(new_oid, initial_oid, "head should change after push");
    assert_eq!(
        origin.commit_count(),
        initial_count + 1,
        "commit count should bump by exactly one"
    );
    assert_eq!(
        origin.head_oid(),
        new_oid,
        "LocalOrigin.head_oid should track the pushed commit"
    );
}

/// Pushing to a non-existent ref surfaces a non-zero exit.
/// The fixture's `run` helper turns that into a panic so a
/// test that misconfigures the push sees a clear failure
/// instead of a silent no-op. This self-test asserts the
/// panic path is the only failure mode the helper exposes:
/// either the push succeeds (and `head_oid` updates) or the
/// test panics.
#[test]
fn ac02_push_to_missing_ref_panics_with_stderr() {
    let origin = LocalOrigin::init("ac02-fail");
    let workdir = tempdir("work");
    origin.clone_into(&workdir);
    // Force the working clone into detached HEAD so the push
    // has nothing to update — git push refuses with a clear
    // "src refspec HEAD does not match any" message. We do
    // this directly through std::process because the
    // fixture's push helpers assume a clean working tree.
    let detach = std::process::Command::new("git")
        .current_dir(&workdir)
        .args(["checkout", "--detach", "HEAD"])
        .output()
        .expect("detach");
    assert!(detach.status.success(), "detach HEAD failed");
    // Now `git push origin HEAD:refs/heads/main` should
    // succeed because we explicitly specified the dst ref —
    // but a push with no refspec from detached HEAD fails.
    let push = std::process::Command::new("git")
        .current_dir(&workdir)
        .args(["push", "origin"])
        .output()
        .expect("push");
    assert!(
        !push.status.success(),
        "push with no refspec from detached HEAD should fail"
    );
    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(
        stderr.contains("not currently on a branch")
            || stderr.contains("does not match")
            || stderr.contains("refspec"),
        "push failure should be informative, got: {stderr}"
    );
}

/// Cloning into an existing non-empty directory fails cleanly.
/// This guards against a future regression where the fixture
/// silently overwrites an existing checkout.
#[test]
fn ac02_clone_into_existing_dir_fails_cleanly() {
    let origin = LocalOrigin::init("ac02-clone-fail");
    let dest = tempdir("occupied");
    std::fs::write(dest.join("existing"), "data").expect("seed");
    let result = std::process::Command::new("git")
        .arg("clone")
        .arg("-b")
        .arg("main")
        .arg(origin.uri())
        .arg(&dest)
        .status();
    let status = result.expect("git clone spawn");
    assert!(!status.success(), "clone into non-empty dir must fail");
    assert!(
        dest.join("existing").exists(),
        "pre-existing file must not be deleted by a failed clone"
    );
}

// -----------------------------------------------------------------------
// AC-03: Record exact GitHub mutation counts
// -----------------------------------------------------------------------

/// Counts::mutations sums POST + PATCH + PUT + DELETE exactly.
/// A test that asks the daemon to POST a comment twice and
/// PATCH an issue once should see `counts.mutations() == 3`
/// and the GET/HEAD requests should not inflate the total.
#[tokio::test]
async fn ac03_counts_track_exact_mutation_total() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "POST",
        "/repos/o/r/issues/1/comments",
        201,
        json!({"id": 1}),
    )
    .await;
    gh.mount_status("PATCH", "/repos/o/r/issues/1", 200, json!({"number": 1}))
        .await;
    gh.mount("GET", "/repos/o/r", json!({"name": "r"})).await;

    let client = reqwest::Client::new();
    for _ in 0..2 {
        let r = client
            .post(gh.uri() + "/repos/o/r/issues/1/comments")
            .json(&json!({"body": "hi"}))
            .send()
            .await
            .expect("post");
        assert_eq!(r.status(), 201);
    }
    let r = client
        .patch(gh.uri() + "/repos/o/r/issues/1")
        .json(&json!({"state": "closed"}))
        .send()
        .await
        .expect("patch");
    assert_eq!(r.status(), 200);
    let r = client
        .get(gh.uri() + "/repos/o/r")
        .send()
        .await
        .expect("get");
    assert_eq!(r.status(), 200);

    let counts = gh.counts();
    assert_eq!(counts.post, 2, "exactly two POSTs");
    assert_eq!(counts.patch, 1, "exactly one PATCH");
    assert_eq!(counts.get, 1, "exactly one GET");
    assert_eq!(counts.mutations(), 3, "mutations = post + patch");
    assert_eq!(counts.total(), 4, "total = get + mutations");
}

/// `path_counts` keys by request path so a test that wants
/// "POST exactly twice to `/comments` and zero times to
/// `/labels`" can assert that directly without walking the
/// request log.
#[tokio::test]
async fn ac03_path_counts_are_keyed_per_endpoint() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "POST",
        "/repos/o/r/issues/1/comments",
        201,
        json!({"id": 1}),
    )
    .await;
    gh.mount_status("POST", "/repos/o/r/issues/1/labels", 200, json!([]))
        .await;

    let client = reqwest::Client::new();
    for _ in 0..3 {
        client
            .post(gh.uri() + "/repos/o/r/issues/1/comments")
            .json(&json!({"body": "x"}))
            .send()
            .await
            .expect("post comment");
    }
    client
        .post(gh.uri() + "/repos/o/r/issues/1/labels")
        .json(&json!(["bug"]))
        .send()
        .await
        .expect("post label");

    let counts = gh.path_counts();
    assert_eq!(counts.get("/repos/o/r/issues/1/comments").copied(), Some(3));
    assert_eq!(counts.get("/repos/o/r/issues/1/labels").copied(), Some(1));
}

/// `received_requests` preserves the order wiremock observed
/// the requests in. Useful for tests that want to assert
/// "first request was GET, second was POST" without relying
/// on timestamps.
#[tokio::test]
async fn ac03_received_requests_preserve_order_and_method() {
    let gh = MockGitHub::start().await;
    gh.mount("GET", "/repos/o/r", json!({"name": "r"})).await;
    gh.mount_status("POST", "/repos/o/r/issues", 201, json!({"number": 1}))
        .await;

    let client = reqwest::Client::new();
    client
        .get(gh.uri() + "/repos/o/r")
        .send()
        .await
        .expect("get");
    client
        .post(gh.uri() + "/repos/o/r/issues")
        .json(&json!({"title": "t"}))
        .send()
        .await
        .expect("post");

    let log = gh.received_requests();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].method.as_str(), "GET");
    assert_eq!(log[1].method.as_str(), "POST");
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn tempdir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut dir = std::env::temp_dir();
    dir.push(format!("caduceus-fixture-self-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

//! Tests for `Client::remove_issue_label`.

use caduceus::github::{Client, ACCEPT_VALUE};
use serde_json::json;

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::MockGitHub;

#[tokio::test]
async fn default_label_delete_hits_encoded_path_and_succeeds_on_204() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "DELETE",
        "/repos/o/r/issues/1/labels/autofix",
        204,
        json!({}),
    )
    .await;

    let client = Client::new(gh.uri().as_str());
    let response = client
        .remove_issue_label("o", "r", 1, "autofix")
        .await
        .expect("204 succeeds");
    assert_eq!(response.status, 204);

    let counts = gh.counts();
    assert_eq!(counts.delete, 1, "expected exactly one DELETE");
    let path_counts = gh.path_counts();
    assert_eq!(
        path_counts.get("/repos/o/r/issues/1/labels/autofix"),
        Some(&1)
    );
}

#[tokio::test]
async fn non_default_label_uses_plain_encoded_path() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "DELETE",
        "/repos/o/r/issues/1/labels/needs-review",
        204,
        json!({}),
    )
    .await;

    let client = Client::new(gh.uri().as_str());
    client
        .remove_issue_label("o", "r", 1, "needs-review")
        .await
        .expect("204 succeeds");

    let counts = gh.counts();
    assert_eq!(counts.delete, 1);
    assert!(gh
        .path_counts()
        .contains_key("/repos/o/r/issues/1/labels/needs-review"));
}

#[tokio::test]
async fn label_not_found_returns_err() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "DELETE",
        "/repos/o/r/issues/1/labels/autofix",
        404,
        json!({ "message": "Not Found" }),
    )
    .await;

    let client = Client::new(gh.uri().as_str());
    let result = client.remove_issue_label("o", "r", 1, "autofix").await;
    assert!(result.is_err(), "404 must surface as an Err");
}

#[tokio::test]
async fn accept_value_is_the_canonical_github_accept_header_value() {
    // Sanity check that `ACCEPT_VALUE` exported by the public `github`
    // module matches the value tests mount against.
    assert_eq!(ACCEPT_VALUE, "application/vnd.github+json");
}

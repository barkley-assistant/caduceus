//! Tests for `enqueue_summaries` error propagation.
//!
//! Verifies that a StateStore returning an error from `enqueue`
//! causes `enqueue_summaries` to propagate that error rather than
//! silently swallowing it.

use std::path::PathBuf;

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-awaiting-review-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

#[tokio::test]
async fn enqueue_summaries_propagates_state_error() {
    let state_dir = tempdir("enqueue-error");
    let store = caduceus::state::queue::StateStore::open(&state_dir).expect("store opens");

    // Write a corrupt state.json so that enqueue fails on load.
    std::fs::write(state_dir.join("state.json"), b"not valid json").expect("write corrupt state");

    let summary = caduceus::poll::IssueSummary {
        key: caduceus::issue::IssueKey::parse("owner/repo#1").unwrap(),
        title: "Test".to_string(),
        labels: vec!["bug".to_string()],
        ticket_type: caduceus::queue::TicketType::Code,
        updated_at: chrono::Utc::now(),
    };

    let result =
        caduceus::daemon::tick::awaiting_review::enqueue_summaries(&store, &[summary], false);
    assert!(result.is_err(), "expected Err from corrupt state, got Ok");
}

#[test]
fn enqueue_summaries_empty_returns_ok_none() {
    let state_dir = tempdir("enqueue-empty");
    let store = caduceus::state::queue::StateStore::open(&state_dir).expect("store opens");

    let result = caduceus::daemon::tick::awaiting_review::enqueue_summaries(&store, &[], false);
    assert!(result.is_ok(), "expected Ok for empty summaries");
    assert!(result.unwrap().is_none(), "expected None for empty input");
}

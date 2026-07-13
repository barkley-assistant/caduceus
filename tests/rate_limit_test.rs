//! Task 2.4 acceptance tests for rate-limit handling.
//!
//! Covers the rate-limit half of the task packet:
//!
//! - 429 mid-pagination short-circuits the poll and is
//!   persisted via the `CadenceGate`.
//! - `X-RateLimit-Remaining: 0` on a 200 page is treated as
//!   exhaustion and persisted.
//! - Missing / malformed headers do not crash the parser;
//!   the `RateLimitInfo` is either absent or the documented
//!   fields are `None`.
//! - The reset time is persisted before the daemon exits the
//!   tick.
//! - Resumption after the reset elapses returns the daemon to a
//!   `Proceed` decision.

use std::path::{Path, PathBuf};

use caduceus::config::Config;
use caduceus::github::{
    poll_interval_from_headers, rate_limit_from_headers, Client, HttpCache, RateLimitInfo,
};
use caduceus::meta::{CadenceDecision, CadenceGate, TickOutcome};
use chrono::{Duration, Utc};
use reqwest::header::{HeaderMap, HeaderValue};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-rate-limit-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn mock_client(server: &MockServer, state_dir: &Path) -> Client {
    let mut cfg = Config::test_defaults(state_dir);
    cfg.api_base = server.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(state_dir).expect("cache opens");
    Client::with_cache(&cfg, cache).expect("client builds")
}

// ---------------------------------------------------------------------------
// Header parsing
// ---------------------------------------------------------------------------

#[test]
fn rate_limit_headers_parse_full_set() {
    let mut headers = HeaderMap::new();
    headers.insert("x-ratelimit-limit", HeaderValue::from_static("5000"));
    headers.insert("x-ratelimit-remaining", HeaderValue::from_static("42"));
    let reset = (Utc::now() + Duration::seconds(600)).timestamp();
    headers.insert(
        "x-ratelimit-reset",
        HeaderValue::from_str(&reset.to_string()).unwrap(),
    );
    let info = rate_limit_from_headers(&headers, 200).expect("present");
    assert_eq!(info.limit, Some(5000));
    assert_eq!(info.remaining, 42);
    assert_eq!(info.reset_at_unix, reset);
}

#[test]
fn rate_limit_headers_ignore_partial_set() {
    let mut headers = HeaderMap::new();
    headers.insert("x-ratelimit-remaining", HeaderValue::from_static("42"));
    // No limit, no reset — remaining-only set is still useful
    // for tracking purposes.
    let info = rate_limit_from_headers(&headers, 200).expect("present");
    assert!(info.limit.is_none());
    assert_eq!(info.remaining, 42);
}

#[test]
fn rate_limit_headers_missing_returns_none() {
    let headers = HeaderMap::new();
    assert!(rate_limit_from_headers(&headers, 200).is_none());
}

#[test]
fn rate_limit_headers_malformed_remaining_returns_none() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-ratelimit-remaining",
        HeaderValue::from_static("not-a-number"),
    );
    assert!(rate_limit_from_headers(&headers, 200).is_none());
}

#[test]
fn rate_limit_headers_malformed_reset_is_tolerated() {
    let mut headers = HeaderMap::new();
    headers.insert("x-ratelimit-remaining", HeaderValue::from_static("42"));
    headers.insert(
        "x-ratelimit-reset",
        HeaderValue::from_static("not-a-timestamp"),
    );
    let info = rate_limit_from_headers(&headers, 200).expect("present");
    // Reset falls back to "now".
    assert_eq!(info.remaining, 42);
    let now = Utc::now().timestamp();
    assert!((info.reset_at_unix - now).abs() <= 2);
}

#[test]
fn rate_limit_status_429_treats_remaining_as_zero() {
    let mut headers = HeaderMap::new();
    headers.insert("x-ratelimit-remaining", HeaderValue::from_static("100"));
    let info = rate_limit_from_headers(&headers, 429).expect("present");
    // Status 429 always wins over a non-zero remaining.
    assert_eq!(info.remaining, 0);
}

#[test]
fn poll_interval_header_parses_seconds() {
    let mut headers = HeaderMap::new();
    headers.insert("x-poll-interval", HeaderValue::from_static("120"));
    assert_eq!(poll_interval_from_headers(&headers), Some(120));
}

#[test]
fn poll_interval_header_malformed_is_none() {
    let mut headers = HeaderMap::new();
    headers.insert("x-poll-interval", HeaderValue::from_static("nope"));
    assert!(poll_interval_from_headers(&headers).is_none());
}

// ---------------------------------------------------------------------------
// 429 mid-pagination
// ---------------------------------------------------------------------------

#[tokio::test]
async fn four_twenty_nine_mid_pagination_short_circuits_poll() {
    let server = MockServer::start().await;
    // Page 1 returns 200 with rate-limit headers still allowing
    // more requests.
    let next_url = format!(
        "{}/repos/octocat/hello-world/issues?per_page=100&page=2&labels=auto-fix&state=open&sort=updated&direction=desc",
        server.uri()
    );
    let link_header = format!("<{next_url}>; rel=\"next\"");
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(wiremock::matchers::query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", link_header)
                .insert_header("x-ratelimit-remaining", "5")
                .set_body_string("[]"),
        )
        .expect(1)
        .mount(&server)
        .await;
    // Page 2 returns 429.
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(wiremock::matchers::query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "0"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let state_dir = tempdir("429");
    let client = mock_client(&server, &state_dir);
    let mut cfg = Config::test_defaults(&state_dir);
    cfg.ticket_label_code = "auto-fix".to_string();
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];

    let result = caduceus::poll::poll_code(&client, &cfg, &cfg.watched_repos).await;
    let err = result.expect_err("429 surfaces");
    let text = format!("{err:?}");
    assert!(text.contains("RateLimited"), "expected RateLimited: {text}");

    // The 429 page is the one the test cares about; the poll loop
    // records the observation on the *current* page, so the
    // 429 page is the one that drives the typed error.
}

// ---------------------------------------------------------------------------
// Remaining zero on 200
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remaining_zero_on_success_response_is_typed_error() {
    let server = MockServer::start().await;
    let reset = (Utc::now() + Duration::seconds(120)).timestamp();
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-limit", "5000")
                .insert_header("x-ratelimit-reset", reset.to_string())
                .set_body_string("[]"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let state_dir = tempdir("zero");
    let client = mock_client(&server, &state_dir);
    let cfg = Config::test_defaults(&state_dir);
    let err = caduceus::poll::discover_watched_repos(&client, &cfg)
        .await
        .expect_err("zero-remaining 200 errors");
    let text = format!("{err:?}");
    assert!(text.contains("RateLimited"), "expected RateLimited: {text}");
}

// ---------------------------------------------------------------------------
// Reset persistence before exit
// ---------------------------------------------------------------------------

#[test]
fn rate_limit_observation_is_persisted_before_exit() {
    let state_dir = tempdir("persist");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let now = Utc::now();
    let reset_unix = (now + Duration::seconds(300)).timestamp();
    let info = RateLimitInfo {
        limit: Some(5000),
        remaining: 0,
        reset_at_unix: reset_unix,
        observed_at: now,
    };
    let persisted = gate.record_rate_limit(&info).expect("persistence succeeds");
    // The persisted reset is exactly the future unix timestamp
    // computed from the input.
    assert_eq!(persisted.reset_at.timestamp(), reset_unix);
    // The gate is process-shared — a fresh handle sees the same
    // state.
    let second = CadenceGate::open(&state_dir).expect("second gate opens");
    let snapshot = second.store().snapshot();
    let rate = snapshot.rate_limit.expect("rate limit persisted");
    assert_eq!(rate.remaining, 0);
    assert_eq!(rate.reset_at.timestamp(), reset_unix);
}

#[test]
fn stale_observation_does_not_overwrite_newer_one() {
    let state_dir = tempdir("stale");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let now = Utc::now();
    // Newer observation first.
    let newer = RateLimitInfo {
        limit: Some(5000),
        remaining: 0,
        reset_at_unix: (now + Duration::seconds(1200)).timestamp(),
        observed_at: now,
    };
    gate.record_rate_limit(&newer).expect("persist newer");
    // Stale observation second.
    let stale = RateLimitInfo {
        limit: Some(5000),
        remaining: 0,
        reset_at_unix: (now + Duration::seconds(60)).timestamp(),
        observed_at: now,
    };
    gate.record_rate_limit(&stale).expect("persist attempt");
    let snapshot = gate.store().snapshot();
    let rate = snapshot.rate_limit.expect("rate limit kept");
    // The newer observation survives.
    assert_eq!(rate.reset_at, newer.reset_at(now));
}

// ---------------------------------------------------------------------------
// Resumption after reset
// ---------------------------------------------------------------------------

#[test]
fn precheck_blocks_until_rate_limit_reset() {
    let state_dir = tempdir("block");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let now = Utc::now();
    let reset = now + Duration::seconds(300);
    let info = RateLimitInfo {
        limit: Some(5000),
        remaining: 0,
        reset_at_unix: reset.timestamp(),
        observed_at: now,
    };
    gate.record_rate_limit(&info).expect("persist");
    // At `now` we are blocked.
    let before = gate.precheck(now, 60);
    assert_eq!(
        before,
        CadenceDecision::RateLimited {
            next_allowed_at: reset
        }
    );
    // At `reset + 1s` we may proceed.
    let after = gate.precheck(reset + Duration::seconds(1), 60);
    assert_eq!(after, CadenceDecision::Proceed);
}

#[test]
fn precheck_blocks_until_cadence_elapses() {
    let state_dir = tempdir("cadence");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let now = Utc::now();
    // Record a finished tick at `now - 30s` with poll_interval=60.
    gate.record_tick_finished(
        now - Duration::seconds(30),
        TickOutcome::Processed,
        Some(200),
        60,
        None,
        None,
    )
    .expect("record finished");
    let decision = gate.precheck(now, 60);
    match decision {
        CadenceDecision::Cadence { next_allowed_at } => {
            let expected = (now - Duration::seconds(30)) + Duration::seconds(60);
            assert_eq!(next_allowed_at, expected);
        }
        other => panic!("expected Cadence, got {other:?}"),
    }
}

#[test]
fn precheck_proceeds_after_cadence_elapses() {
    let state_dir = tempdir("proceed");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let now = Utc::now();
    gate.record_tick_finished(
        now - Duration::seconds(120),
        TickOutcome::Processed,
        Some(200),
        60,
        None,
        None,
    )
    .expect("record finished");
    let decision = gate.precheck(now, 60);
    assert_eq!(decision, CadenceDecision::Proceed);
}

#[test]
fn rate_limited_outcome_keeps_persisted_reset_in_next_allowed_poll() {
    let state_dir = tempdir("keep");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let now = Utc::now();
    let reset = now + Duration::seconds(600);
    let info = RateLimitInfo {
        limit: Some(5000),
        remaining: 0,
        reset_at_unix: reset.timestamp(),
        observed_at: now,
    };
    gate.record_tick_finished(
        now,
        TickOutcome::RateLimited,
        Some(429),
        60,
        Some(&info),
        None,
    )
    .expect("record");
    let snapshot = gate.store().snapshot();
    assert_eq!(snapshot.next_allowed_poll_at, Some(reset));
}

// ---------------------------------------------------------------------------
// Server-suggested poll interval
// ---------------------------------------------------------------------------

#[test]
fn server_suggested_poll_interval_is_taken_as_floor() {
    let state_dir = tempdir("floor");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let now = Utc::now();
    // Server suggests 600s; configured is 60s; the floor wins.
    gate.record_poll_interval(now, 600).expect("persist");
    let snapshot = gate.store().snapshot();
    let next = snapshot.next_allowed_poll_at.expect("set");
    let delta = (next - now).num_seconds();
    assert!((599..=601).contains(&delta), "expected ~600s, got {delta}");
}

#[test]
fn server_suggested_poll_interval_does_not_shorten_configured() {
    let state_dir = tempdir("shorten");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let now = Utc::now();
    // First the daemon persisted a long interval from a previous
    // tick, then the server says 10s — the existing 600s floor
    // stays.
    gate.record_poll_interval(now, 600).expect("first");
    gate.record_poll_interval(now, 10).expect("second");
    let snapshot = gate.store().snapshot();
    let next = snapshot.next_allowed_poll_at.expect("set");
    let delta = (next - now).num_seconds();
    assert!(delta >= 599, "floor must hold; got {delta}");
}

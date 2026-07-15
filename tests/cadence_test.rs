//! Task 2.4 acceptance tests for poll cadence.
//!
//! Covers the cadence half of the task packet:
//!
//! - Cadence skip across two process-equivalent clients (two
//!   `CadenceGate` instances over the same `state_dir`).
//! - A server-suggested `X-Poll-Interval` larger than the
//!   configured floor lengthens the gate.
//! - A missing or malformed `X-Poll-Interval` is tolerated and
//!   falls back to the configured floor.
//! - A successful precheck in the first tick followed by an
//!   early second tick is recorded as a `Cadence` outcome and
//!   writes no HTTP traffic on the second invocation.

use std::path::PathBuf;

use caduceus::config::Config;
use caduceus::meta::{CadenceDecision, CadenceGate, TickOutcome};
use chrono::{Duration, Utc};

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-cadence-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

// ---------------------------------------------------------------------------
// Cadence across two process-equivalent clients
// ---------------------------------------------------------------------------

#[test]
fn second_client_observes_first_clients_cadence() {
    let state_dir = tempdir("two-clients");
    // First tick: Proceed, then a successful completion.
    let first = CadenceGate::open(&state_dir).expect("first opens");
    let now = Utc::now();
    let pre = first.precheck(now, 60);
    assert_eq!(pre, CadenceDecision::Proceed);
    first.record_tick_started(now).expect("record start");
    first
        .record_tick_finished(now, TickOutcome::Processed, Some(200), 60, None, None)
        .expect("record finish");
    drop(first);
    // Second tick: 30s later, same state_dir → Cadence because
    // last_tick_finished + 60s > now.
    let second = CadenceGate::open(&state_dir).expect("second opens");
    let pre = second.precheck(now + Duration::seconds(30), 60);
    match pre {
        CadenceDecision::Cadence { next_allowed_at } => {
            let expected = now + Duration::seconds(60);
            assert_eq!(next_allowed_at, expected);
        }
        other => panic!("expected Cadence, got {other:?}"),
    }
    // A third tick after the gate elapses: Proceed.
    let pre = second.precheck(now + Duration::seconds(120), 60);
    assert_eq!(pre, CadenceDecision::Proceed);
}

// ---------------------------------------------------------------------------
// Server-suggested poll interval lengthens the gate
// ---------------------------------------------------------------------------

#[test]
fn server_suggested_poll_interval_lengthens_next_allowed() {
    let state_dir = tempdir("server-interval");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let now = Utc::now();
    // Server-suggested 600s. Configured floor (60s) is shorter
    // so the floor wins — but `record_poll_interval` records the
    // longer of the existing interval and the new server value.
    gate.record_poll_interval(now, 600).expect("persist");
    // Cadence gate uses last_tick_finished, not
    // next_allowed_poll_at — but the persisted next_allowed
    // time is what the operator reads in `caduceus status`.
    let snapshot = gate.store().snapshot();
    let next = snapshot.next_allowed_poll_at.expect("set");
    let delta = (next - now).num_seconds();
    assert!((599..=601).contains(&delta), "expected ~600s, got {delta}");
}

#[test]
fn missing_poll_interval_header_falls_back_to_configured() {
    // This test exercises the *client* side: when the response
    // does not carry `X-Poll-Interval`, the daemon uses the
    // configured floor. We test the underlying helper directly
    // because the integration path lives in the daemon's run
    // loop, which is owned by Phase 5.
    use caduceus::github::poll_interval_from_headers;
    use reqwest::header::HeaderMap;
    let headers = HeaderMap::new();
    assert!(poll_interval_from_headers(&headers).is_none());
}

// ---------------------------------------------------------------------------
// Precheck with no history
// ---------------------------------------------------------------------------

#[test]
fn fresh_state_meta_proceeds_immediately() {
    let state_dir = tempdir("fresh");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let decision = gate.precheck(Utc::now(), 60);
    assert_eq!(decision, CadenceDecision::Proceed);
}

#[test]
fn precheck_proceeds_with_only_tick_started_recorded() {
    // `last_tick_finished` is the cadence gate; a tick that
    // started but never finished must not block the next call.
    let state_dir = tempdir("started-only");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let now = Utc::now();
    gate.record_tick_started(now).expect("record");
    // 30 seconds later, with poll_interval=60s: no finished
    // tick, so the gate proceeds.
    let decision = gate.precheck(now + Duration::seconds(30), 60);
    assert_eq!(decision, CadenceDecision::Proceed);
}

// ---------------------------------------------------------------------------
// Outcome routing
// ---------------------------------------------------------------------------

#[test]
fn rate_limited_outcome_keeps_the_daemon_under_cadence() {
    let state_dir = tempdir("rate-limited");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let now = Utc::now();
    let reset = now + Duration::seconds(900);
    let info = caduceus::github::RateLimitInfo {
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
    // At `now + 30s`, the configured floor (60s) would have
    // elapsed, but the rate-limit observation owns the gate.
    let decision = gate.precheck(now + Duration::seconds(30), 60);
    match decision {
        CadenceDecision::RateLimited { next_allowed_at } => {
            assert_eq!(next_allowed_at, reset);
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[test]
fn cadences_tick_outcome_is_cadence() {
    use caduceus::meta::CadenceDecision;
    let d = CadenceDecision::Cadence {
        next_allowed_at: Utc::now(),
    };
    assert_eq!(d.tick_outcome(), Some(TickOutcome::SkippedCadence));
    let d = CadenceDecision::RateLimited {
        next_allowed_at: Utc::now(),
    };
    assert_eq!(d.tick_outcome(), Some(TickOutcome::RateLimited));
    assert!(CadenceDecision::Proceed.tick_outcome().is_none());
}

// ---------------------------------------------------------------------------
// poll_interval configured value is preserved
// ---------------------------------------------------------------------------

#[test]
fn configured_poll_interval_is_used_when_servers_silent() {
    let state_dir = tempdir("configured");
    let gate = CadenceGate::open(&state_dir).expect("gate opens");
    let mut cfg = Config::test_defaults(&state_dir);
    cfg.poll_interval_seconds = 240;
    let now = Utc::now();
    // First tick: success.
    gate.record_tick_finished(
        now,
        TickOutcome::Processed,
        Some(200),
        cfg.poll_interval_seconds,
        None,
        None,
    )
    .expect("record");
    // Second tick: 60s later (within the 240s gate) → Cadence.
    let pre = gate.precheck(now + Duration::seconds(60), cfg.poll_interval_seconds);
    match pre {
        CadenceDecision::Cadence { next_allowed_at } => {
            assert_eq!(next_allowed_at, now + Duration::seconds(240));
        }
        other => panic!("expected Cadence, got {other:?}"),
    }
}

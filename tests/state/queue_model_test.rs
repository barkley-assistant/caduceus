//! "Issue identity and queue schema" and the contract that a future
//! queue-file version produces a `StateCorrupt`-style error rather
//! than best-effort parsing.

use caduceus::config::Config;
use caduceus::error::CaduceusError;
use caduceus::issue::IssueKey;
use caduceus::queue::{
    parse_queue_state, serialize_queue_state, Phase, QueueEntry, QueueState, TicketType,
    QUEUE_FILE_VERSION,
};
use chrono::{TimeZone, Utc};

fn tempdir(label: &str) -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-queue-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn sample_key() -> IssueKey {
    IssueKey {
        owner: "BarkleyAssistant".to_string(),
        repo: "sandbox".to_string(),
        number: 42,
    }
}

fn sample_entry() -> QueueEntry {
    QueueEntry {
        key: sample_key(),
        phase: Phase::Queued,
        ticket_type: TicketType::Code,
        attempts: 0,
        last_error: None,
        last_run_id: None,
        next_attempt_at: None,
        finalization: None,
        queued_at: Utc.with_ymd_and_hms(2026, 7, 13, 14, 0, 0).unwrap(),
        updated_at: Utc.with_ymd_and_hms(2026, 7, 13, 14, 0, 0).unwrap(),
        generation: 1,
    }
}

fn sample_state() -> QueueState {
    let mut entries = std::collections::BTreeMap::new();
    entries.insert(sample_entry().key.display_key(), sample_entry());
    QueueState {
        version: QUEUE_FILE_VERSION,
        entries,
    }
}

// IssueKey parsing

#[test]
fn issue_key_parse_accepts_canonical_form() {
    let key = IssueKey::parse("BarkleyAssistant/sandbox#42").expect("valid");
    assert_eq!(key.owner, "BarkleyAssistant");
    assert_eq!(key.repo, "sandbox");
    assert_eq!(key.number, 42);
}

#[test]
fn issue_key_parse_preserves_original_case() {
    let key = IssueKey::parse("MixedCase/Repo#7").expect("valid");
    assert_eq!(key.owner, "MixedCase");
    assert_eq!(key.repo, "Repo");
    assert_eq!(key.number, 7);
}

#[test]
fn issue_key_display_key_is_lowercased() {
    let key = IssueKey::parse("Camel/Case#1").expect("valid");
    assert_eq!(key.display_key(), "camel/case#1");
}

#[test]
fn issue_key_parse_rejects_missing_hash() {
    let err = IssueKey::parse("owner/repo").expect_err("missing #");
    let msg = format!("{err:?}");
    assert!(msg.contains("missing '#'"), "got: {msg}");
}

#[test]
fn issue_key_parse_rejects_missing_slash() {
    let err = IssueKey::parse("owner#42").expect_err("missing /");
    let msg = format!("{err:?}");
    assert!(msg.contains("missing '/'"), "got: {msg}");
}

#[test]
fn issue_key_parse_rejects_empty_owner_or_repo() {
    let err = IssueKey::parse("/repo#1").expect_err("empty owner");
    let msg = format!("{err:?}");
    assert!(msg.contains("empty owner"), "got: {msg}");

    let err = IssueKey::parse("owner/#1").expect_err("empty repo");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("empty owner or repo") || msg.contains("empty repo"),
        "got: {msg}"
    );
}

#[test]
fn issue_key_parse_rejects_zero_number() {
    let err = IssueKey::parse("owner/repo#0").expect_err("zero number");
    let msg = format!("{err:?}");
    assert!(msg.contains("positive"), "got: {msg}");
}

#[test]
fn issue_key_parse_rejects_invalid_owner() {
    let err = IssueKey::parse("-leading-hyphen/repo#1").expect_err("bad owner");
    let msg = format!("{err:?}");
    assert!(msg.contains("owner"), "got: {msg}");
}

#[test]
fn issue_key_parse_rejects_invalid_repo() {
    let err = IssueKey::parse("owner/..#1").expect_err("bad repo");
    let msg = format!("{err:?}");
    assert!(msg.contains("repo"), "got: {msg}");
}

#[test]
fn issue_key_parse_rejects_non_numeric_number() {
    let err = IssueKey::parse("owner/repo#notanumber").expect_err("non-numeric");
    let msg = format!("{err:?}");
    assert!(msg.contains("number parse"), "got: {msg}");
}

// Phase / TicketType JSON

#[test]
fn phase_serializes_as_snake_case() {
    let json = serde_json::to_string(&Phase::InProgress).unwrap();
    assert_eq!(json, "\"in_progress\"");
}

#[test]
fn ticket_type_serializes_as_snake_case() {
    assert_eq!(
        serde_json::to_string(&TicketType::Investigation).unwrap(),
        "\"investigation\""
    );
}

#[test]
fn phase_deserializes_snake_case() {
    let phase: Phase = serde_json::from_str("\"previewed\"").unwrap();
    assert_eq!(phase, Phase::Previewed);
}

// QueueEntry / QueueState round-trip

#[test]
fn queue_state_round_trip_through_serialize_parse() {
    let state = sample_state();
    let rendered = serialize_queue_state(&state).expect("serialize");
    let parsed = parse_queue_state(&rendered).expect("parse");
    assert_eq!(parsed, state);
    assert_eq!(parsed.version, QUEUE_FILE_VERSION);
}

#[test]
fn queue_state_display_key_is_lowercase_in_serialized_form() {
    let state = sample_state();
    let rendered = serialize_queue_state(&state).expect("serialize");
    // The map key in the JSON must be lowercase.
    assert!(
        rendered.contains("barkleyassistant/sandbox#42"),
        "got: {rendered}"
    );
    assert!(
        !rendered.contains("BarkleyAssistant/sandbox#42"),
        "got: {rendered}"
    );
}

#[test]
fn queue_state_round_trip_preserves_phase_and_ticket_type() {
    let mut state = sample_state();
    let entry = state.entries.values_mut().next().unwrap();
    entry.phase = Phase::Previewed;
    entry.ticket_type = TicketType::Investigation;
    let rendered = serialize_queue_state(&state).expect("serialize");
    assert!(rendered.contains("\"previewed\""), "got: {rendered}");
    assert!(rendered.contains("\"investigation\""), "got: {rendered}");

    let parsed = parse_queue_state(&rendered).expect("parse");
    let entry = parsed.entries.values().next().unwrap();
    assert_eq!(entry.phase, Phase::Previewed);
    assert_eq!(entry.ticket_type, TicketType::Investigation);
}

#[test]
fn queue_state_round_trip_preserves_timestamps_as_rfc3339_utc() {
    let state = sample_state();
    let rendered = serialize_queue_state(&state).expect("serialize");
    // chrono emits ``2026-07-13T14:00:00Z`` for DateTime<Utc>.
    assert!(rendered.contains("2026-07-13T14:00:00"), "got: {rendered}");
    // No timezone offset other than the trailing ``Z``.
    assert!(!rendered.contains("+00:00"), "got: {rendered}");
}

// Schema-stability: deny_unknown_fields

#[test]
fn queue_state_rejects_unknown_field() {
    let bad = r#"{"version":1,"entries":{},"rogue":"leak"}"#;
    let err = parse_queue_state(bad).expect_err("deny_unknown_fields");
    let msg = format!("{err:?}");
    assert!(msg.contains("StateCorrupt"), "got: {msg}");
    assert!(msg.contains("queue state JSON parse"), "got: {msg}");
}

#[test]
fn queue_entry_rejects_unknown_field() {
    // Insert an unknown top-level field; the rest of the document is
    // a valid serialized state.
    let good = serialize_queue_state(&sample_state()).expect("serialize");
    // The serialized form is ``{"version":1,"entries":{...}}``;
    // inject an unknown key before the closing brace.
    let trimmed = good.trim_end_matches('}');
    let bad = format!("{trimmed},\"rogue\":\"x\"}}");
    let err = parse_queue_state(&bad).expect_err("unknown field rejected");
    let msg = format!("{err:?}");
    assert!(msg.contains("StateCorrupt"), "got: {msg}");
    assert!(msg.contains("rogue"), "got: {msg}");
}

#[test]
fn queue_state_missing_required_field_is_corrupt() {
    let bad = r#"{"version":1}"#;
    let err = parse_queue_state(bad).expect_err("missing entries");
    let msg = format!("{err:?}");
    assert!(msg.contains("StateCorrupt"), "got: {msg}");
}

// Unsupported version rejection

#[test]
fn queue_state_with_future_version_is_rejected() {
    let bad = r#"{"version":999,"entries":{}}"#;
    let err = parse_queue_state(bad).expect_err("future version");
    match err {
        CaduceusError::StateCorrupt { message, .. } => {
            assert!(
                message.contains("unsupported queue state version"),
                "got: {message}"
            );
            assert!(message.contains("999"), "got: {message}");
            assert!(message.contains("expected 1"), "got: {message}");
        }
        other => panic!("expected StateCorrupt; got: {other:?}"),
    }
}

#[test]
fn queue_state_with_version_zero_is_rejected() {
    let bad = r#"{"version":0,"entries":{}}"#;
    let err = parse_queue_state(bad).expect_err("version 0");
    let msg = format!("{err:?}");
    assert!(msg.contains("StateCorrupt"), "got: {msg}");
    assert!(msg.contains("unsupported"), "got: {msg}");
}

// Map key / entry invariant

#[test]
fn queue_state_rejects_mismatched_map_key() {
    // Build a JSON document where the map key is uppercase but the
    // entry key is the canonical lowercase form.
    let entry_json = r#"{"key":{"owner":"BarkleyAssistant","repo":"sandbox","number":42},"phase":"queued","ticket_type":"code","attempts":0,"last_error":null,"last_run_id":null,"next_attempt_at":null,"finalization":null,"queued_at":"2026-07-13T14:00:00Z","updated_at":"2026-07-13T14:00:00Z","generation":1}"#;
    let bad =
        format!(r#"{{"version":1,"entries":{{"BarkleyAssistant/sandbox#42":{entry_json}}}}}"#);
    let err = parse_queue_state(&bad).expect_err("map key mismatch");
    let msg = format!("{err:?}");
    assert!(msg.contains("StateCorrupt"), "got: {msg}");
    assert!(msg.contains("does not match entry"), "got: {msg}");
}

// QueueState helpers

#[test]
fn queue_state_empty_is_constructable() {
    let empty = QueueState::empty();
    assert_eq!(empty.version, QUEUE_FILE_VERSION);
    assert!(empty.entries.is_empty());

    let rendered = serialize_queue_state(&empty).expect("empty serialises");
    assert_eq!(rendered, "{\"version\":1,\"entries\":{}}");
}

#[test]
fn queue_state_entry_lookup_is_case_insensitive_via_display_key() {
    let state = sample_state(); // uses BarkleyAssistant/sandbox
    let lc = IssueKey::parse("barkleyassistant/sandbox#42").expect("valid");
    let canonical = IssueKey::parse("BarkleyAssistant/sandbox#42").expect("valid");
    assert!(state.entry(&lc).is_some());
    assert!(state.entry(&canonical).is_some());
}

#[test]
fn queue_state_entry_lookup_skips_unvalidated_keys() {
    let state = sample_state();
    // Construct an invalid key directly (bypass parse validation).
    let bad = IssueKey {
        owner: "..".to_string(),
        repo: "sandbox".to_string(),
        number: 1,
    };
    assert!(state.entry(&bad).is_none());
}

// Settings integration (load the sample config to anchor at-handle)

#[test]
fn loaded_config_supports_the_queue_test_harness() {
    let root = tempdir("queue-empty");
    let _cfg = Config::test_defaults(&root);
    // Just ensure it builds; the queue's loads/saves against a path
    // are owned by Phase 3 — this sanity check confirms the type
    // graph is intact.
}

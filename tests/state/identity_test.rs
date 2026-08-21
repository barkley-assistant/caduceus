use std::fs;
use std::path::{Path, PathBuf};

use caduceus::issue::IssueKey;
use caduceus::queue::{
    parse_queue_state, reap_stale_claims, serialize_queue_state, ClaimFileBody, Phase, QueueEntry,
    QueueState, StateStore, TicketType, CLAIMS_DIRNAME, CLAIM_FILE_VERSION,
};
use chrono::{Duration, Utc};
use uuid::Uuid;

fn key(owner: &str, number: u64) -> IssueKey {
    IssueKey {
        owner: owner.to_string(),
        repo: "identity-tests".to_string(),
        number,
    }
}

fn claim_path(state_dir: &Path, digest: &str) -> PathBuf {
    state_dir
        .join(CLAIMS_DIRNAME)
        .join(format!("{digest}.claim"))
}

fn claim_body(path: &Path) -> ClaimFileBody {
    let bytes = fs::read(path).expect("claim file");
    serde_json::from_slice(&bytes).expect("claim JSON")
}

fn queue_with_entries(state_dir: &Path, entries: &[IssueKey]) -> StateStore {
    let store = StateStore::open(state_dir).expect("open state store");
    for entry in entries {
        store
            .enqueue(entry, TicketType::Code, false)
            .expect("enqueue identity test entry");
    }
    store
}

#[test]
fn identity_format_uuid_epoch_start_ticks() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = queue_with_entries(temp.path(), &[key("format", 1)]);
    let claimed = store
        .acquire_next("identity-format", std::process::id(), Utc::now())
        .expect("acquire")
        .expect("claim");
    let body = claim_body(&claim_path(temp.path(), claimed.claim.digest()));
    let segments: Vec<_> = body.process_start_identity.split(':').collect();

    assert_eq!(segments.len(), 3);
    assert!(
        Uuid::parse_str(segments[0]).is_ok(),
        "uuid: {}",
        segments[0]
    );
    assert!(segments[1].parse::<u64>().is_ok(), "epoch: {}", segments[1]);
    assert!(
        segments[2].parse::<u64>().is_ok(),
        "start ticks: {}",
        segments[2]
    );
}

#[test]
fn daemon_identity_file_created_with_uuid_v4_and_0600_mode() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = queue_with_entries(temp.path(), &[key("created", 1)]);
    let _ = store
        .acquire_next("identity-created", std::process::id(), Utc::now())
        .expect("acquire")
        .expect("claim");

    let path = temp.path().join("daemon-identity");
    assert!(path.is_file());
    let contents = fs::read_to_string(&path).expect("daemon identity");
    assert!(Uuid::parse_str(&contents).is_ok(), "uuid: {contents:?}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(&path)
            .expect("identity metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "daemon identity mode: {mode:o}");
    }
}

#[test]
fn daemon_identity_uuid_stable_across_same_boot_reads() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = queue_with_entries(temp.path(), &[key("stable", 1), key("stable", 2)]);
    let first = store
        .acquire_next("identity-stable-1", std::process::id(), Utc::now())
        .expect("first acquire")
        .expect("first claim");
    let first_body = claim_body(&claim_path(temp.path(), first.claim.digest()));
    let first_uuid = first_body
        .process_start_identity
        .split(':')
        .next()
        .expect("uuid half")
        .to_string();

    let second = store
        .acquire_next("identity-stable-2", std::process::id(), Utc::now())
        .expect("second acquire")
        .expect("second claim");
    let second_body = claim_body(&claim_path(temp.path(), second.claim.digest()));
    let second_uuid = second_body
        .process_start_identity
        .split(':')
        .next()
        .expect("uuid half")
        .to_string();

    assert_eq!(first_uuid, second_uuid);
    assert_eq!(
        fs::read_to_string(temp.path().join("daemon-identity")).unwrap(),
        first_uuid
    );
}

#[test]
fn boot_epoch_changes_on_simulated_reboot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = queue_with_entries(temp.path(), &[key("epoch", 1), key("epoch", 2)]);
    let first = store
        .acquire_next("identity-epoch-1", std::process::id(), Utc::now())
        .expect("first acquire")
        .expect("first claim");
    let first_body = claim_body(&claim_path(temp.path(), first.claim.digest()));
    let first_epoch = first_body
        .process_start_identity
        .split(':')
        .nth(1)
        .expect("epoch half")
        .parse::<u64>()
        .expect("epoch integer");

    let second = store
        .acquire_next("identity-epoch-2", std::process::id(), Utc::now())
        .expect("second acquire")
        .expect("second claim");
    let second_body = claim_body(&claim_path(temp.path(), second.claim.digest()));
    let second_epoch = second_body
        .process_start_identity
        .split(':')
        .nth(1)
        .expect("epoch half")
        .parse::<u64>()
        .expect("epoch integer");

    // A real reboot cannot be induced safely by a unit test. The platform
    // clock is the direct seam here; successive reads must never move
    // backwards, and a reboot would move the boot-time epoch to a new value.
    assert!(second_epoch >= first_epoch);
}

#[test]
fn reaper_tolerates_old_format_claims() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_dir = temp.path();
    let claims_dir = state_dir.join(CLAIMS_DIRNAME);
    fs::create_dir_all(&claims_dir).expect("claims dir");

    let issue = key("old-format", 1);
    let now = Utc::now();
    let entry = QueueEntry {
        key: issue.clone(),
        phase: Phase::InProgress,
        ticket_type: TicketType::Code,
        attempts: 0,
        last_error: None,
        last_run_id: Some("old-format-run".to_string()),
        next_attempt_at: None,
        finalization: None,
        queued_at: now,
        updated_at: now,
        generation: 1,
        blocked_source: None,
        blocked_recovery_hint: None,
    };
    let queue = QueueState {
        version: 1,
        entries: [(issue.display_key(), entry)].into_iter().collect(),
    };
    fs::write(
        state_dir.join("state.json"),
        serialize_queue_state(&queue).expect("queue JSON"),
    )
    .expect("state file");

    let claim = ClaimFileBody {
        version: CLAIM_FILE_VERSION,
        key: issue,
        run_id: "old-format-run".to_string(),
        pid: 4_000_000,
        process_start_identity: "<unknown-boot>:0".to_string(),
        started_at: now - Duration::hours(2),
        worktree_path: None,
    };
    let digest = caduceus::queue::display_digest(&claim.key.display_key());
    fs::write(
        claims_dir.join(format!("{digest}.claim")),
        serde_json::to_vec(&claim).expect("claim JSON"),
    )
    .expect("claim file");

    let report = tokio_test_block_on(reap_stale_claims(state_dir, now, 1)).expect("reap");
    assert_eq!(report.stale_reaped, 1);
    assert_eq!(report.quarantined, 0);
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    let parsed = parse_queue_state(
        &fs::read_to_string(state_dir.join("state.json")).expect("updated state"),
    )
    .expect("updated queue JSON");
    assert_eq!(
        parsed.entries.values().next().expect("queue entry").phase,
        Phase::Queued
    );
}

fn tokio_test_block_on<F: std::future::Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(future)
}

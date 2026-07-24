//! Lease tests (T-10).
//!
//! - stale_owner_test: holder A's lease expires, holder B acquires
//!   (token increments), A's old token triggers
//!   FencingTokenRegression.
//! - renewal_test: holder A renews before expiry; non-holder
//!   cannot renew.
//! - recovery_test: holder A releases definitively, new holder
//!   acquires without waiting for natural expiry, token is
//!   monotonic.

use std::time::Duration;

use caduceus::infra::error::CaduceusError;
use caduceus::scheduler::LeaseStore;
use caduceus::state::store;

fn temp_state_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

fn open_lease_store(dir: &std::path::Path) -> LeaseStore {
    // Ensure the state dir exists and the DB is initialized.
    let conn = store::open_in(dir).expect("open SQLite store");
    LeaseStore::new(conn)
}

#[test]
fn stale_owner_test() {
    let dir = temp_state_dir();
    let mut ls = open_lease_store(dir.path());

    // Holder A acquires a lease with a very short TTL.
    let lease_a = ls
        .acquire("owner/repo#1", "holder-A", Duration::from_secs(1))
        .expect("A acquires lease");
    assert_eq!(lease_a.fencing_token, 1, "first acquire gets token 1");

    // Simulate expiry by directly updating the expires_at column
    // to a past timestamp via a separate connection.
    {
        let conn = store::open_in(dir.path()).expect("open for force-expiry");
        conn.execute(
            "UPDATE leases SET expires_at = 0 WHERE issue_key = 'owner/repo#1'",
            [],
        )
        .expect("force expiry");
    }

    // Holder B acquires the now-expired lease. Token must increment.
    let lease_b = ls
        .acquire("owner/repo#1", "holder-B", Duration::from_secs(60))
        .expect("B acquires lease after A expired");
    assert_eq!(lease_b.fencing_token, 2, "B's token must be 2");

    // Holder A tries to verify with the old token — must fail
    // with FencingTokenRegression or LeaseStale.
    let result = ls.verify_fencing_token("owner/repo#1", "holder-A", 1);
    match result {
        Err(CaduceusError::FencingTokenRegression {
            issue_key,
            stale_token,
            current_token,
        }) => {
            assert_eq!(issue_key, "owner/repo#1");
            assert_eq!(stale_token, 1);
            assert_eq!(current_token, 2);
        }
        Err(CaduceusError::LeaseStale { .. }) => {
            // Also acceptable — A is no longer the owner.
        }
        other => {
            panic!(
                "expected FencingTokenRegression or LeaseStale, got {:?}",
                other
            );
        }
    }
}

#[test]
fn renewal_test() {
    let dir = temp_state_dir();
    let mut ls = open_lease_store(dir.path());

    // Holder A acquires a lease.
    let lease_a = ls
        .acquire("owner/repo#2", "holder-A", Duration::from_secs(60))
        .expect("A acquires lease");
    assert_eq!(lease_a.fencing_token, 1);

    // Holder A renews successfully.
    ls.renew("owner/repo#2", "holder-A", Duration::from_secs(120))
        .expect("A renews lease");

    // Non-holder B cannot renew.
    let result = ls.renew("owner/repo#2", "holder-B", Duration::from_secs(120));
    assert!(
        matches!(result, Err(CaduceusError::LeaseStale { context, .. }) if context == "renew"),
        "non-holder renewal must fail with LeaseStale, got {:?}",
        result
    );
}

#[test]
fn recovery_test() {
    let dir = temp_state_dir();
    let mut ls = open_lease_store(dir.path());

    // Holder A acquires a lease.
    let lease_a = ls
        .acquire("owner/repo#3", "holder-A", Duration::from_secs(60))
        .expect("A acquires lease");
    assert_eq!(lease_a.fencing_token, 1);

    // Holder A releases definitively.
    ls.release_definitively_dead("owner/repo#3", "holder-A", 1)
        .expect("A releases definitively");

    // New holder B acquires without waiting for natural expiry.
    let lease_b = ls
        .acquire("owner/repo#3", "holder-B", Duration::from_secs(60))
        .expect("B acquires after A released");
    assert_eq!(
        lease_b.fencing_token, 2,
        "token must be monotonically increasing after release+reacquire"
    );

    // Verify token monotonicity: B's token > A's old token.
    assert!(lease_b.fencing_token > lease_a.fencing_token);
}

#[test]
fn acquire_new_lease_gets_token_one() {
    let dir = temp_state_dir();
    let mut ls = open_lease_store(dir.path());

    let lease = ls
        .acquire("owner/repo#99", "holder-X", Duration::from_secs(60))
        .expect("acquire new lease");
    assert_eq!(lease.fencing_token, 1);
    assert_eq!(lease.issue_key, "owner/repo#99");
    assert_eq!(lease.owner_id, "holder-X");
}

#[test]
fn acquire_held_unexpired_lease_returns_contention() {
    let dir = temp_state_dir();
    let mut ls = open_lease_store(dir.path());

    // First acquire succeeds.
    let _lease = ls
        .acquire("owner/repo#100", "holder-A", Duration::from_secs(600))
        .expect("first acquire");

    // Second acquire on the same key while held and unexpired
    // must fail with LeadershipContended.
    let result = ls.acquire("owner/repo#100", "holder-B", Duration::from_secs(60));
    assert!(
        matches!(
            result,
            Err(CaduceusError::LeadershipContended { context, .. }) if context == "acquire"
        ),
        "acquiring a held unexpired lease must return LeadershipContended, got {:?}",
        result
    );
}

#[test]
fn release_with_wrong_token_fails() {
    let dir = temp_state_dir();
    let mut ls = open_lease_store(dir.path());

    let _lease = ls
        .acquire("owner/repo#101", "holder-A", Duration::from_secs(60))
        .expect("acquire");

    // Release with a wrong fencing token must fail.
    let result = ls.release_definitively_dead("owner/repo#101", "holder-A", 999);
    assert!(
        matches!(result, Err(CaduceusError::LeaseStale { context, .. }) if context == "release"),
        "release with wrong token must fail with LeaseStale, got {:?}",
        result
    );

    // The lease must still be held — verify via a separate
    // connection.
    {
        let conn = store::open_in(dir.path()).expect("open for check");
        let state: String = conn
            .query_row(
                "SELECT state FROM leases WHERE issue_key = 'owner/repo#101'",
                [],
                |row| row.get(0),
            )
            .expect("read state");
        assert_eq!(state, "held");
    }
}

#[test]
fn verify_fencing_token_with_current_token_succeeds() {
    let dir = temp_state_dir();
    let mut ls = open_lease_store(dir.path());

    let lease = ls
        .acquire("owner/repo#102", "holder-A", Duration::from_secs(60))
        .expect("acquire");

    // Verify with the current token must succeed.
    ls.verify_fencing_token("owner/repo#102", "holder-A", lease.fencing_token)
        .expect("verify with current token must succeed");
}

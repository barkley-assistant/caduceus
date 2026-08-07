//! Regression tests for the per-tick claim cap (issue #108).
//!
//! These tests pin the contract that a single tick claims at most
//! `max_issues_per_tick` queue entries before returning, while `0`
//! restores the unbounded drain-the-queue behavior. The cap counts
//! the claim itself — circuit-blocked and pool-saturated entries are
//! requeued via `continue 'dispatch` but still consume quota, so a
//! flood of blocked entries cannot starve real work.
//!
//! The full `tick()` body would exercise many other concerns
//! (cadence gate, GitHub client, lease store). This file mirrors the
//! production dispatch shape from `src/daemon/tick/mod.rs` with a
//! `Pool`-level stub: cap check at the top of the loop, the claim
//! counter incremented after a successful acquire and before the
//! circuit probe, and the JoinSet drained to completion on exit.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;

use caduceus::scheduler::{DrainConfig, Pool};

/// Small helper that builds a `DrainConfig` with a short
/// backpressure budget and a generous drain window — the test
/// doesn't exercise drain, only admission concurrency.
fn drain_config() -> DrainConfig {
    DrainConfig::from_seconds_and_ms(60, 5_000) // 60s drain, 5s backpressure
}

/// A "claimed entry" stand-in: the only fields the dispatch loop
/// needs are the repo key and a stable index for simulating the
/// circuit-breaker probe. The real `StateStore::acquire_next`
/// returns a `ClaimedEntry`; the stub uses this trivial struct to
/// keep the test focused on the cap shape.
#[derive(Clone)]
struct MockClaim {
    index: usize,
    repo_key: String,
}

/// Observable dispatch counts after a run.
struct DispatchCounts {
    claimed: usize,
    spawned: usize,
    completed: usize,
}

/// Mirrors the production `'dispatch` loop shape from
/// `src/daemon/tick/mod.rs`: cap check at the top, claim counter
/// incremented after a successful acquire (before the circuit probe
/// and pool admit), and the JoinSet drained to completion on exit.
/// Entries before `blocked_prefix` are treated as circuit-blocked
/// and requeued via `continue 'dispatch` without spawning.
async fn run_dispatch(
    queue: Vec<MockClaim>,
    cap: u32,
    parallelism: usize,
    blocked_prefix: usize,
) -> DispatchCounts {
    let pool = Arc::new(Pool::new(parallelism as u32, drain_config()));
    let queue = Arc::new(std::sync::Mutex::new(queue.into_iter()));

    let claimed = Arc::new(AtomicUsize::new(0));
    let spawned = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let mut set: JoinSet<()> = JoinSet::new();
    let mut processed: u32 = 0;

    'dispatch: loop {
        // Per-tick claim cap — identical to production: `0` is
        // unbounded, otherwise break before the next acquire once the
        // cap is reached.
        if cap != 0 && processed >= cap {
            break 'dispatch;
        }

        // 6.1. Acquire the next entry (mocked via a queue iterator).
        let claimed_entry = match queue.lock().unwrap().next() {
            Some(c) => c,
            None => break 'dispatch,
        };
        // Count the claim immediately: circuit-blocked and
        // pool-saturated entries are requeued via `continue 'dispatch`
        // below, but they still consume quota (issue #108).
        processed += 1;
        claimed.fetch_add(1, Ordering::SeqCst);

        // 6.2. Simulated circuit-breaker probe. Entries before
        // `blocked_prefix` are requeued without spawning, mirroring
        // the production `continue 'dispatch` path.
        if claimed_entry.index < blocked_prefix {
            continue 'dispatch;
        }

        // 6.3. Admit the entry to the worker pool.
        let admit = pool
            .admit(&claimed_entry.repo_key, &claimed_entry.repo_key)
            .await
            .expect("admit succeeds for distinct repos under cap");

        // 6.4. Spawn the stub worker into the JoinSet.
        spawned.fetch_add(1, Ordering::SeqCst);
        let completed_for_task = Arc::clone(&completed);
        set.spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            completed_for_task.fetch_add(1, Ordering::SeqCst);
            // admit drops here, releasing the permit.
            drop(admit);
        });

        // 6.5. When the JoinSet is at the cap, await one completion
        // before pulling the next claim.
        if set.len() >= parallelism {
            if let Some(joined) = set.join_next().await {
                joined.expect("task did not panic or abort");
            }
        }
    }

    // 6.6. Final drain — unchanged, runs to completion.
    while set.join_next().await.is_some() {}

    DispatchCounts {
        claimed: claimed.load(Ordering::SeqCst),
        spawned: spawned.load(Ordering::SeqCst),
        completed: completed.load(Ordering::SeqCst),
    }
}

fn queue_of(n: usize) -> Vec<MockClaim> {
    (0..n)
        .map(|i| MockClaim {
            index: i,
            repo_key: format!("owner/repo-{i}"),
        })
        .collect()
}

#[tokio::test]
async fn dispatch_loop_breaks_at_cap() {
    // GIVEN a cap of 4 and a queue of 12 distinct repos
    let cap: u32 = 4;
    let queue = queue_of(12);

    // WHEN the dispatch loop runs with the production cap shape
    let counts = run_dispatch(
        queue.clone(),
        cap,
        /* parallelism */ 2,
        /* blocked */ 0,
    )
    .await;

    // THEN exactly `cap` entries were claimed
    assert_eq!(
        counts.claimed, cap as usize,
        "loop must claim exactly max_issues_per_tick entries"
    );

    // AND every claimed entry was spawned and completed
    assert_eq!(counts.spawned, cap as usize);
    assert_eq!(counts.completed, cap as usize);

    // AND the queue still holds the remaining entries for the next tick
    let remaining = queue.len() - counts.claimed;
    assert_eq!(
        remaining, 8,
        "unclaimed entries stay queued for the next tick"
    );
}

#[tokio::test]
async fn dispatch_loop_zero_is_unbounded() {
    // GIVEN cap = 0 (unbounded) and a queue of 12 distinct repos
    let cap: u32 = 0;
    let queue = queue_of(12);

    // WHEN the dispatch loop runs
    let counts = run_dispatch(
        queue.clone(),
        cap,
        /* parallelism */ 3,
        /* blocked */ 0,
    )
    .await;

    // THEN every entry is claimed, spawned, and completed — `0` is
    // the documented escape hatch that restores the pre-#108
    // drain-the-queue behavior.
    assert_eq!(counts.claimed, 12, "0 must be unbounded");
    assert_eq!(counts.spawned, 12);
    assert_eq!(counts.completed, 12);
}

#[tokio::test]
async fn circuit_blocked_entry_consumes_quota() {
    // GIVEN a cap of 3 and a queue of 6 where the first 2 entries
    // are circuit-blocked and requeued without spawning
    let cap: u32 = 3;
    let queue = queue_of(6);

    // WHEN the dispatch loop runs
    let counts = run_dispatch(
        queue.clone(),
        cap,
        /* parallelism */ 2,
        /* blocked */ 2,
    )
    .await;

    // THEN the loop claimed exactly 3 entries — the 2 blocked ones
    // consumed quota and were requeued, and only 1 real worker was
    // spawned before the cap stopped the drain.
    assert_eq!(
        counts.claimed, cap as usize,
        "circuit-blocked claims must count toward the cap"
    );
    assert_eq!(counts.spawned, 1, "only the unblocked entry spawns");
    assert_eq!(counts.completed, 1);

    // AND the remaining entries stay queued for the next tick
    let remaining = queue.len() - counts.claimed;
    assert_eq!(remaining, 3, "blocked flood must not starve the cap");
}

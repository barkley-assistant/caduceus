//! Regression tests for the bounded-dispatch JoinSet loop.
//!
//! These tests pin the contract that a single tick dispatches up to
//! `worker_parallelism` queue entries concurrently via a
//! `tokio::task::JoinSet` loop, while preserving the per-repo
//! exclusion and per-admission lifetime semantics introduced by
//! issue #91 / PR #104.
//!
//! The full `tick()` body would exercise many other concerns
//! (cadence gate, circuit breaker, GitHub client). This file
//! focuses on the dispatch shape itself: a `Pool`-level stub
//! mimics the `tick()` loop's claim → admit → spawn → join_next
//! sequence and asserts the `in_flight <= worker_parallelism`
//! invariant holds at every observable point.

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

/// A "claimed entry" stand-in: the only field the dispatch loop
/// needs to drive admission is the repo key. The real
/// `StateStore::acquire_next` returns a `ClaimedEntry`; the
/// stub uses this trivial struct to keep the test focused on
/// the JoinSet shape.
#[derive(Clone)]
struct MockClaim {
    repo_key: String,
}

#[tokio::test]
async fn joinset_dispatch_caps_in_flight_at_parallelism() {
    // GIVEN worker_parallelism = 3 and a queue of 8 distinct repos
    let parallelism: usize = 3;
    let total_entries: usize = 8;
    let pool = Arc::new(Pool::new(parallelism as u32, drain_config()));

    let queue: Vec<MockClaim> = (0..total_entries)
        .map(|i| MockClaim {
            repo_key: format!("owner/repo-{i}"),
        })
        .collect();
    let queue = Arc::new(std::sync::Mutex::new(queue.into_iter()));

    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    // WHEN the dispatch loop runs the new tick() shape:
    //   - acquire_next lazily per iteration (mocked)
    //   - pool.admit(&repo_key) per entry
    //   - spawn into JoinSet; join_next when at the cap
    let mut set: JoinSet<()> = JoinSet::new();

    loop {
        // Step 1: lazy acquire_next (mocked via a queue iterator).
        let claimed = match queue.lock().unwrap().next() {
            Some(c) => c,
            None => break,
        };

        // Step 2: pool.admit. With worker_parallelism permits and
        // distinct repo keys, every admit succeeds while the
        // in_flight count stays at or below the cap.
        let admit = pool
            .admit(&claimed.repo_key)
            .await
            .expect("admit succeeds for distinct repo under cap");

        // Step 3: spawn the stub into the JoinSet. The stub
        // instruments in_flight so we can observe the cap.
        let in_flight_for_task = Arc::clone(&in_flight);
        let peak_for_task = Arc::clone(&peak);
        let completed_for_task = Arc::clone(&completed);
        let repo_key = claimed.repo_key.clone();
        set.spawn(async move {
            let now = in_flight_for_task.fetch_add(1, Ordering::SeqCst) + 1;
            // Record peak concurrently — last writer wins, but
            // every reader sees at least the value at the moment
            // of its fetch_max.
            peak_for_task.fetch_max(now, Ordering::SeqCst);

            // Yield to give sibling tasks a chance to advance
            // so the test actually exercises concurrency.
            tokio::time::sleep(Duration::from_millis(20)).await;

            in_flight_for_task.fetch_sub(1, Ordering::SeqCst);
            completed_for_task.fetch_add(1, Ordering::SeqCst);
            // admit is dropped here (function end), releasing
            // both the semaphore permit and the per-repo
            // exclusion lock. We discard `repo_key` because the
            // dispatch loop doesn't need it after spawn.
            let _ = repo_key;
            drop(admit);
        });

        // Step 4: if the set is full, await one completion before
        // pulling the next entry.
        if set.len() >= parallelism {
            // join_next drops the JoinHandle; the Admission
            // owned by the spawned task has already been dropped
            // on its way out, releasing the permit.
            let joined = set
                .join_next()
                .await
                .expect("set has at least one task to join");
            joined.expect("task did not panic or abort");
        }
    }

    // Drain any remaining in-flight tasks.
    while set.join_next().await.is_some() {}

    // THEN the peak in_flight matches worker_parallelism
    assert_eq!(
        peak.load(Ordering::SeqCst),
        parallelism,
        "peak concurrency must equal worker_parallelism"
    );

    // AND every entry completed
    assert_eq!(
        completed.load(Ordering::SeqCst),
        total_entries,
        "every dispatched entry completed"
    );

    // AND the invariant held at every observable point: after
    // drain, in_flight must be zero (no leak).
    assert_eq!(
        in_flight.load(Ordering::SeqCst),
        0,
        "in_flight must return to zero after drain"
    );

    // AND the pool returned to Idle (every Admission's permits
    // were released via Drop).
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        pool.state(),
        caduceus::scheduler::PoolState::Idle,
        "pool must be Idle after all workers dropped their Admission"
    );
}

#[tokio::test]
async fn joinset_dispatch_never_exceeds_cap_under_backpressure() {
    // GIVEN worker_parallelism = 2 with a long backpressure budget
    // and 6 distinct repos queued.
    let parallelism: usize = 2;
    let total_entries: usize = 6;
    let pool = Arc::new(Pool::new(parallelism as u32, drain_config()));

    let queue: Vec<MockClaim> = (0..total_entries)
        .map(|i| MockClaim {
            repo_key: format!("owner/repo-bp-{i}"),
        })
        .collect();
    let queue = Arc::new(std::sync::Mutex::new(queue.into_iter()));

    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let mut set: JoinSet<()> = JoinSet::new();

    loop {
        let claimed = match queue.lock().unwrap().next() {
            Some(c) => c,
            None => break,
        };

        let admit = match pool.admit(&claimed.repo_key).await {
            Ok(a) => a,
            Err(_) => {
                // Pool saturation — back off briefly and try
                // again. The dispatch loop in `tick()` would
                // route this through `handle_infra_or_retry`;
                // here we just await one JoinSet completion to
                // free a permit before retrying.
                if let Some(joined) = set.join_next().await {
                    joined.expect("task did not panic");
                }
                continue;
            }
        };

        let in_flight_for_task = Arc::clone(&in_flight);
        let peak_for_task = Arc::clone(&peak);
        set.spawn(async move {
            let now = in_flight_for_task.fetch_add(1, Ordering::SeqCst) + 1;
            peak_for_task.fetch_max(now, Ordering::SeqCst);

            // The invariant we care about: while this task is
            // running, in_flight must not exceed the cap. The
            // atomic ops inside the spawned stub (and any
            // intermediate yields) are the observable points
            // the spec calls out.
            assert!(
                in_flight_for_task.load(Ordering::SeqCst) <= parallelism,
                "in_flight invariant violated: observed {} > {}",
                in_flight_for_task.load(Ordering::SeqCst),
                parallelism,
            );

            tokio::time::sleep(Duration::from_millis(30)).await;

            in_flight_for_task.fetch_sub(1, Ordering::SeqCst);
            drop(admit);
        });

        if set.len() >= parallelism {
            if let Some(joined) = set.join_next().await {
                joined.expect("task did not panic or abort");
            }
        }
    }

    while set.join_next().await.is_some() {}

    // THEN the cap was never exceeded (asserted inside each
    // spawned task) and the peak matches the cap.
    assert_eq!(
        peak.load(Ordering::SeqCst),
        parallelism,
        "peak concurrency must equal worker_parallelism"
    );
    assert_eq!(
        in_flight.load(Ordering::SeqCst),
        0,
        "in_flight must return to zero after drain"
    );
}

#[tokio::test]
async fn admission_drop_releases_permit_for_next_dispatch() {
    // Regression for the #91 / PR #104 contract: each spawned
    // task owns its own `Admission` and the permit must release
    // on task drop so the dispatch loop can admit the next entry
    // without ever exceeding the cap.
    let pool = Arc::new(Pool::new(2, drain_config()));

    let in_flight = Arc::new(AtomicUsize::new(0));
    let mut set: JoinSet<()> = JoinSet::new();

    for i in 0..6 {
        let repo = format!("owner/release-{i}");
        let admit = pool.admit(&repo).await.expect("admit under cap");

        let in_flight_for_task = Arc::clone(&in_flight);
        set.spawn(async move {
            in_flight_for_task.fetch_add(1, Ordering::SeqCst);
            // Short, deterministic work — long enough that two
            // workers will be in flight together, short enough
            // that the test stays fast.
            tokio::time::sleep(Duration::from_millis(10)).await;
            in_flight_for_task.fetch_sub(1, Ordering::SeqCst);
            // Drop admit here; the JoinSet join_next will
            // surface the JoinHandle drop below, releasing the
            // permit.
            drop(admit);
        });

        if set.len() >= 2 {
            let joined = set
                .join_next()
                .await
                .expect("set has at least one task to join");
            joined.expect("task did not panic or abort");
        }
    }

    while set.join_next().await.is_some() {}

    // Final state: pool is idle (all permits released), in_flight
    // is zero, and we dispatched all 6 entries.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(pool.state(), caduceus::scheduler::PoolState::Idle);
    assert_eq!(in_flight.load(Ordering::SeqCst), 0);
}

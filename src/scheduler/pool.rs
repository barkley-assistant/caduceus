//! Bounded concurrency worker pool with per-repository exclusion.
//!
//! The [`Pool`] gates all worker dispatch through a combination of:
//!
//! - A `tokio::sync::Semaphore` sized by `worker_parallelism` for global
//!   slot control.
//! - A [`RepoExclusionMap`](super::exclusion::RepoExclusionMap) that
//!   serialises admissions for the same repository inside the process.
//! - When configured, a host-wide [`LeaseStore`](super::leases::LeaseStore)
//!   consulted before the in-process exclusion; this is what promotes
//!   the per-repo "one worker at a time" contract from process-local to
//!   host-local so two overlapping cron ticks cannot exceed
//!   `worker_parallelism`.
//! - A draining flag that stops new admissions and awaits in-flight
//!   workers during graceful shutdown.
//!
//! The [`Admission`] guard releases the semaphore permit, the per-repo
//! mutex guard, and (when configured) the lease on drop.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};

use super::exclusion::RepoExclusionMap;
use super::leases::{LeaseGuard, LeaseStore};
use crate::infra::error::CaduceusError;

/// Configuration for pool drain behaviour.
#[derive(Clone, Copy, Debug)]
pub struct DrainConfig {
    /// Maximum time to wait for in-flight workers to complete during
    /// a drain, in seconds.
    pub drain_timeout: Duration,
    /// Maximum time to wait for a semaphore permit before returning
    /// `PoolSaturated`, in milliseconds.
    pub backpressure_budget: Duration,
}

impl DrainConfig {
    /// Build a `DrainConfig` from integer config values.
    pub fn from_seconds_and_ms(drain_timeout_seconds: u64, backpressure_budget_ms: u64) -> Self {
        Self {
            drain_timeout: Duration::from_secs(drain_timeout_seconds),
            backpressure_budget: Duration::from_millis(backpressure_budget_ms),
        }
    }
}

/// Observable state of the worker pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PoolState {
    /// No workers are currently active.
    Idle,
    /// `n` workers are active but capacity remains.
    Active(u32),
    /// All permits are held; no further slots available.
    Saturated,
    /// Drain has been triggered; new admissions are rejected.
    Draining,
}

impl std::fmt::Display for PoolState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolState::Idle => write!(f, "idle"),
            PoolState::Active(n) => write!(f, "active({n})"),
            PoolState::Saturated => write!(f, "saturated"),
            PoolState::Draining => write!(f, "draining"),
        }
    }
}

/// Outcome of [`Pool::admit`].
///
/// `Admitted` holds a permit guard and an exclusion guard — both are
/// released on drop. The error variants mirror the corresponding
/// [`CaduceusError`] variants but carry owned guard fields so the
/// caller can inspect them without a reference to the pool.
pub enum Admission {
    /// Admission succeeded. The caller holds a semaphore permit, a
    /// per-repo exclusion lock, and (when the pool was built with a
    /// `LeaseStore`) a host-wide per-repo lease. All three are
    /// released when this value is dropped.
    Admitted {
        _permit: OwnedSemaphorePermit,
        _exclusion: OwnedMutexGuard<()>,
        _lease: Option<LeaseGuard>,
    },
    /// Pool is saturated or admission timed out.
    PoolSaturated { current_depth: u32, max_depth: u32 },
    /// Drain is in progress; admission rejected.
    DrainTimeout { timed_out_run_ids: Vec<String> },
}

impl std::fmt::Debug for Admission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Admission::Admitted { .. } => f.debug_struct("Admission::Admitted").finish(),
            Admission::PoolSaturated {
                current_depth,
                max_depth,
            } => f
                .debug_struct("Admission::PoolSaturated")
                .field("current_depth", current_depth)
                .field("max_depth", max_depth)
                .finish(),
            Admission::DrainTimeout { timed_out_run_ids } => f
                .debug_struct("Admission::DrainTimeout")
                .field("timed_out_run_ids", timed_out_run_ids)
                .finish(),
        }
    }
}

/// Bounded concurrency worker pool with per-repository exclusion.
///
/// The pool is shared across tick dispatches via `Arc<Pool>`. The
/// design is in-memory only for the semaphore and exclusion map —
/// they reset on daemon restart, which is safe because scheduler
/// leases already guard against concurrent scheduler transactions.
/// When configured with a `LeaseStore`, the per-repo lease persists
/// across restarts so overlapping ticks on different processes (or
/// the same process after a crash before TTL expiry) cannot exceed
/// `worker_parallelism` host-wide.
pub struct Pool {
    /// Global slot counter. Sized by `worker_parallelism`.
    semaphore: Arc<Semaphore>,
    /// Maximum number of permits (worker_parallelism).
    max_permits: u32,
    /// Per-repository exclusion locks.
    excl_map: RepoExclusionMap,
    /// Draining flag. Once set, `admit` returns `DrainTimeout`.
    draining: std::sync::Mutex<bool>,
    /// Drain configuration.
    drain_config: DrainConfig,
    /// Optional host-wide lease store. When `Some`, `admit` consults
    /// the lease store before the in-process exclusion and semaphore
    /// so the per-repo "one worker at a time" contract is enforced
    /// across all processes sharing the store.
    lease_store: Option<Arc<Mutex<LeaseStore>>>,
    /// TTL for leases acquired from `lease_store`. Default 600s.
    worker_lease_ttl: Duration,
    /// Identity used as the lease `owner_id`. Composed once per pool
    /// from `pid@start_unix_secs` so a recycled PID cannot collide
    /// with a stale lease within the TTL window.
    worker_owner_id: String,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("max_permits", &self.semaphore.available_permits())
            .field("drain_config", &self.drain_config)
            .finish()
    }
}

impl Pool {
    /// Create a new pool with `parallelism` slots and the given drain
    /// configuration. No lease store is configured — the in-process
    /// exclusion map is the only per-repo gate. Production cron ticks
    /// build the pool via [`Pool::with_lease_store`] so the per-repo
    /// "one worker at a time" contract is enforced host-wide.
    pub fn new(parallelism: u32, drain_config: DrainConfig) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(parallelism as usize)),
            max_permits: parallelism,
            excl_map: RepoExclusionMap::new(),
            draining: std::sync::Mutex::new(false),
            drain_config,
            lease_store: None,
            worker_lease_ttl: Duration::from_secs(600),
            worker_owner_id: format!("{}@{}", std::process::id(), chrono::Utc::now().timestamp()),
        }
    }

    /// Builder: attach a host-wide lease store so `admit` enforces
    /// the per-repo contract across all processes sharing the store.
    /// The supplied `worker_lease_ttl` is the TTL on every acquired
    /// lease and bounds the worst-case leak when a worker panics
    /// between acquire and the RAII Drop of the `LeaseGuard`.
    pub fn with_lease_store(
        mut self,
        store: Arc<Mutex<LeaseStore>>,
        worker_lease_ttl: Duration,
    ) -> Self {
        self.lease_store = Some(store);
        self.worker_lease_ttl = worker_lease_ttl;
        self
    }

    /// Attempt to admit a new worker for the given `issue_key` and
    /// `repo_key`.
    ///
    /// The two arguments are intentionally distinct: `issue_key` is
    /// what gets recorded in the lease store (callers use a synthetic
    /// `repo:<owner>/<repo>` prefix to keep the row self-documenting
    /// in `sqlite3` inspection), and `repo_key` is the raw
    /// `<owner>/<repo>` used by the in-process exclusion map.
    ///
    /// The admission flow:
    /// 1. Check the draining flag — if set, return `DrainTimeout`.
    /// 2. If a `LeaseStore` is configured, acquire the per-repo
    ///    lease. `LeadershipContended` maps to `PoolSaturated` so the
    ///    dispatch loop's existing retry path is reused unchanged;
    ///    `LeaseStale` is propagated as-is so the operator sees a
    ///    hard infrastructure failure.
    /// 3. Acquire the per-repo exclusion lock (via `excl_map`).
    /// 4. Acquire a semaphore permit within `backpressure_budget`.
    /// 5. On timeout, return `PoolSaturated`.
    /// 6. On success, return `Admitted` with permit + exclusion +
    ///    optional lease guards.
    ///
    /// The lease is acquired before the exclusion lock and the
    /// semaphore permit so the host-wide gate fails fast without
    /// touching the heavier in-process primitives on contention. The
    /// lease is released by `LeaseGuard`'s `Drop` impl (RAII).
    pub async fn admit(&self, issue_key: &str, repo_key: &str) -> Result<Admission, CaduceusError> {
        // 1. Check draining flag.
        {
            let draining = self.draining.lock().expect("draining lock");
            if *draining {
                return Err(CaduceusError::DrainTimeout {
                    timed_out_run_ids: vec![],
                });
            }
        }

        // 2. Host-wide per-repo lease (when configured).
        let lease_guard = if let Some(store_arc) = &self.lease_store {
            let lease = {
                let mut store = store_arc.lock().expect("lease store lock");
                match store.acquire(issue_key, &self.worker_owner_id, self.worker_lease_ttl) {
                    Ok(l) => l,
                    Err(CaduceusError::LeadershipContended { .. }) => {
                        // Map to PoolSaturated so the dispatch loop's
                        // existing handle_infra_or_retry + classify_error
                        // retry path handles this case unchanged.
                        return Err(CaduceusError::PoolSaturated {
                            current_depth: self.current_depth(),
                            max_depth: self.max_permits,
                        });
                    }
                    Err(e) => return Err(e),
                }
            };
            Some(LeaseGuard::new(
                Arc::clone(store_arc),
                lease.issue_key,
                lease.owner_id,
                lease.fencing_token,
            ))
        } else {
            None
        };

        // 3. Get the per-repo exclusion lock.
        let excl_lock = self.excl_map.get_or_init(repo_key);

        // We need to acquire the exclusion lock. We use lock_owned on
        // the Arc<Mutex<()>> to get an OwnedMutexGuard that is not
        // tied to the local borrow.
        let excl_guard = excl_lock.lock_owned().await;

        // 4. Acquire a semaphore permit within the backpressure budget.
        let max_permits = self.max_permits;
        match tokio::time::timeout(
            self.drain_config.backpressure_budget,
            self.semaphore.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => Ok(Admission::Admitted {
                _permit: permit,
                _exclusion: excl_guard,
                _lease: lease_guard,
            }),
            Ok(Err(_)) => {
                // Semaphore closed — treat as saturated. The
                // lease_guard (if any) is dropped here, releasing
                // the per-repo lease via its Drop impl.
                Err(CaduceusError::PoolSaturated {
                    current_depth: self.current_depth(),
                    max_depth: max_permits,
                })
            }
            Err(_elapsed) => {
                // Timeout — backpressure budget exceeded. The
                // lease_guard (if any) is dropped here, releasing
                // the per-repo lease via its Drop impl.
                Err(CaduceusError::PoolSaturated {
                    current_depth: self.current_depth(),
                    max_depth: max_permits,
                })
            }
        }
    }

    /// Trigger a graceful drain. Sets the draining flag so new
    /// admissions are rejected, then waits for all in-flight workers
    /// to release their permits by acquiring every permit from the
    /// semaphore.
    ///
    /// Returns the list of run IDs that timed out (currently empty
    /// since we don't have a lease store reference here; the caller
    /// manages the lease cancellation).
    pub async fn drain(&self) -> Vec<String> {
        // Set the draining flag.
        {
            let mut draining = self.draining.lock().expect("draining lock");
            *draining = true;
        }

        // Acquire all permits to wait for in-flight workers to complete.
        // tokio::sync::Semaphore does not have a "wait for zero" API, so
        // we acquire all permits sequentially. Each acquire blocks until
        // a permit is released by an in-flight worker.
        let max_permits = self.max_permits;
        let deadline = tokio::time::Instant::now() + self.drain_config.drain_timeout;

        for _ in 0..max_permits {
            if tokio::time::Instant::now() >= deadline {
                // Drain timeout reached; stop waiting.
                break;
            }
            let _ = tokio::time::timeout(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                self.semaphore.clone().acquire_owned(),
            )
            .await;
        }

        // All acquired permits are dropped at end of scope, restoring
        // the semaphore's capacity. The draining flag remains set so
        // new admits are still rejected.
        Vec::new()
    }

    /// Observe the current pool state without blocking.
    pub fn state(&self) -> PoolState {
        let draining = self.draining.lock().expect("draining lock");
        if *draining {
            return PoolState::Draining;
        }
        let max_permits = self.max_permits;
        let available = self.semaphore.available_permits() as u32;
        let active = max_permits.saturating_sub(available);

        if active == 0 {
            PoolState::Idle
        } else if active >= max_permits {
            PoolState::Saturated
        } else {
            PoolState::Active(active)
        }
    }

    /// Number of permits currently held (active workers).
    fn current_depth(&self) -> u32 {
        let max = self.max_permits;
        let available = self.semaphore.available_permits() as u32;
        max.saturating_sub(available)
    }
}

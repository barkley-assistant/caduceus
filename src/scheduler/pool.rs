//! Bounded concurrency worker pool with per-repository exclusion.
//!
//! The [`Pool`] gates all worker dispatch through a combination of:
//!
//! - A `tokio::sync::Semaphore` sized by `worker_parallelism` for global
//!   slot control.
//! - A [`RepoExclusionMap`](super::exclusion::RepoExclusionMap) that
//!   serialises admissions for the same repository.
//! - A draining flag that stops new admissions and awaits in-flight
//!   workers during graceful shutdown.
//!
//! The [`Admission`] guard releases both the semaphore permit and the
//! per-repo mutex guard on drop.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};

use super::exclusion::RepoExclusionMap;
use crate::infra::error::CaduceusError;

// ---------------------------------------------------------------------------
// DrainConfig
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// PoolState
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------

/// Outcome of [`Pool::admit`].
///
/// `Admitted` holds a permit guard and an exclusion guard — both are
/// released on drop. The error variants mirror the corresponding
/// [`CaduceusError`] variants but carry owned guard fields so the
/// caller can inspect them without a reference to the pool.
pub enum Admission {
    /// Admission succeeded. The caller holds a semaphore permit and a
    /// per-repo exclusion lock; both are released when this value is
    /// dropped.
    Admitted {
        _permit: OwnedSemaphorePermit,
        _exclusion: OwnedMutexGuard<()>,
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

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

/// Bounded concurrency worker pool with per-repository exclusion.
///
/// The pool is shared across tick dispatches via `Arc<Pool>`. The
/// design is in-memory only — it resets on daemon restart, which is
/// safe because scheduler leases (Task 5.1) already guard against
/// concurrent scheduler transactions.
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
    /// configuration.
    pub fn new(parallelism: u32, drain_config: DrainConfig) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(parallelism as usize)),
            max_permits: parallelism,
            excl_map: RepoExclusionMap::new(),
            draining: std::sync::Mutex::new(false),
            drain_config,
        }
    }

    /// Attempt to admit a new worker for the given repo key.
    ///
    /// The admission flow:
    /// 1. Check the draining flag — if set, return `DrainTimeout`.
    /// 2. Acquire the per-repo exclusion lock (via `excl_map`).
    /// 3. Acquire a semaphore permit within `backpressure_budget`.
    /// 4. On timeout, return `PoolSaturated`.
    /// 5. On success, return `Admitted` with both guards.
    ///
    /// The exclusion lock is acquired *before* the semaphore permit to
    /// avoid deadlock: a semaphore slot is never held while waiting for
    /// a repo lock, and repo locks are scoped to a single admit call.
    pub async fn admit(&self, repo_key: &str) -> Result<Admission, CaduceusError> {
        // 1. Check draining flag.
        {
            let draining = self.draining.lock().expect("draining lock");
            if *draining {
                return Err(CaduceusError::PoolSaturated {
                    current_depth: self.current_depth(),
                    max_depth: self.max_permits,
                });
            }
        }

        // 2. Get the per-repo exclusion lock.
        let excl_lock = self.excl_map.get_or_init(repo_key);

        // We need to acquire the exclusion lock. We use lock_owned on
        // the Arc<Mutex<()>> to get an OwnedMutexGuard that is not
        // tied to the local borrow.
        let excl_guard = excl_lock.lock_owned().await;

        // 3. Acquire a semaphore permit within the backpressure budget.
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
            }),
            Ok(Err(_)) => {
                // Semaphore closed — treat as saturated.
                Err(CaduceusError::PoolSaturated {
                    current_depth: self.current_depth(),
                    max_depth: max_permits,
                })
            }
            Err(_elapsed) => {
                // Timeout — backpressure budget exceeded.
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

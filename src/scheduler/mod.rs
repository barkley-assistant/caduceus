//! Scheduler leadership, fenced leases, bounded concurrency pool,
//! and per-repository exclusion.
//!
//! The scheduler replaces the full-tick global flock with short
//! transactional leadership and per-issue leases with fencing
//! tokens. See [`leadership`] for the leader-election primitive
//! and [`leases`] for the per-issue lease store.
//!
//! The [`pool`] module provides bounded concurrency with per-repo
//! exclusion and graceful drain. The [`exclusion`] module provides
//! the per-repo exclusion map used by the pool.

pub mod circuit;
pub mod exclusion;
mod leadership;
mod leases;
pub mod pool;

pub use circuit::CircuitStore;
pub use exclusion::RepoExclusionMap;
pub use leadership::LeaderToken;
pub use leases::LeaseStore;
pub use pool::{Admission, DrainConfig, Pool, PoolState};

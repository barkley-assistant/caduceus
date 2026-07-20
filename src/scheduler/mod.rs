//! Scheduler leadership and fenced leases.
//!
//! The scheduler replaces the full-tick global flock with short
//! transactional leadership and per-issue leases with fencing
//! tokens. See [`leadership`] for the leader-election primitive
//! and [`leases`] for the per-issue lease store.

mod leadership;
mod leases;

pub use leadership::LeaderToken;
pub use leases::LeaseStore;

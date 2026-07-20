//! Per-issue lease store with fencing tokens.
//!
//! Leases are backed by the SQLite `leases` table. Fencing tokens
//! are monotonically increasing per `issue_key` and are
//! atomically incremented via the database — the DB is the source
//! of truth across restarts.

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use rusqlite::{params, Connection};

use crate::infra::error::{CaduceusError, CaduceusResult};

/// A per-issue lease returned by [`LeaseStore::acquire`].
#[derive(Clone, Debug)]
pub struct Lease {
    /// The issue key (owner/repo#number).
    pub issue_key: String,
    /// The worker that owns this lease.
    pub owner_id: String,
    /// Monotonically increasing fencing token.
    pub fencing_token: u64,
    /// Unix timestamp in seconds at which this lease expires.
    pub expires_at: i64,
    /// Current state of the lease.
    pub state: LeaseState,
}

/// State of a lease in the database.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseState {
    /// Lease is currently held by an owner.
    Held,
    /// Lease was explicitly released via `release_definitively_dead`.
    Released,
    /// Lease expired naturally.
    Expired,
}

impl LeaseState {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "held" => Some(Self::Held),
            "released" => Some(Self::Released),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

/// Per-issue lease store backed by SQLite.
#[derive(Debug)]
pub struct LeaseStore {
    conn: Connection,
}

impl LeaseStore {
    /// Open a lease store using an existing SQLite connection.
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// Open a lease store at the given state directory.
    pub fn open(state_dir: &Path) -> CaduceusResult<Self> {
        let conn = crate::state::store::open_in(state_dir)?;
        Ok(Self { conn })
    }

    /// Acquire a lease for the given issue key. If the lease does
    /// not exist or is in `released`/`expired` state, a new lease
    /// is created with fencing_token = 1. If the lease is held
    /// and not expired, returns `LeadershipContended`.
    ///
    /// The fencing token is atomically incremented in the database.
    pub fn acquire(
        &mut self,
        issue_key: &str,
        owner_id: &str,
        ttl: Duration,
    ) -> CaduceusResult<Lease> {
        let now = Utc::now().timestamp();
        let expires_at = now + ttl.as_secs() as i64;

        // Use a transaction with a single atomic UPSERT that
        // increments the fencing token.
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| CaduceusError::LeaseStale {
                context: "acquire",
                stderr: format!("begin tx: {e}"),
            })?;

        // Check current state.
        let current: Option<(String, i64, i64, String)> = tx
            .query_row(
                "SELECT owner_id, fencing_token, expires_at, state FROM leases WHERE issue_key = ?1",
                params![issue_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .ok();

        if let Some((_current_owner, _current_token, stored_expires_at, state)) = current {
            let state_enum = LeaseState::from_str(&state).unwrap_or(LeaseState::Expired);

            match state_enum {
                LeaseState::Held if stored_expires_at > now => {
                    // Lease is held and not expired — contention.
                    return Err(CaduceusError::LeadershipContended {
                        context: "acquire",
                        stderr: format!(
                            "lease for {issue_key} held by {_current_owner} until {stored_expires_at}"
                        ),
                    });
                }
                _ => {
                    // Lease is released, expired, or held but expired — can acquire.
                }
            }
        }

        // UPSERT: insert or update, incrementing fencing token.
        tx.execute(
            "INSERT INTO leases (issue_key, owner_id, fencing_token, expires_at, state)
             VALUES (?1, ?2, 1, ?3, 'held')
             ON CONFLICT(issue_key) DO UPDATE SET
                 owner_id = excluded.owner_id,
                 fencing_token = leases.fencing_token + 1,
                 expires_at = excluded.expires_at,
                 state = 'held'",
            params![issue_key, owner_id, expires_at],
        )
        .map_err(|e| CaduceusError::LeaseStale {
            context: "acquire upsert",
            stderr: format!("{e}"),
        })?;

        // Read back the new fencing token.
        let new_token: i64 = tx
            .query_row(
                "SELECT fencing_token FROM leases WHERE issue_key = ?1",
                params![issue_key],
                |row| row.get(0),
            )
            .map_err(|e| CaduceusError::LeaseStale {
                context: "acquire readback",
                stderr: format!("{e}"),
            })?;

        tx.commit().map_err(|e| CaduceusError::LeaseStale {
            context: "acquire commit",
            stderr: format!("{e}"),
        })?;

        Ok(Lease {
            issue_key: issue_key.to_string(),
            owner_id: owner_id.to_string(),
            fencing_token: new_token as u64,
            expires_at,
            state: LeaseState::Held,
        })
    }

    /// Verify that the caller's fencing token is >= the stored
    /// token for the given issue key. Returns `Ok(())` if the
    /// token is valid, or `FencingTokenRegression` if the token
    /// is stale.
    pub fn verify_fencing_token(
        &self,
        issue_key: &str,
        owner_id: &str,
        token: u64,
    ) -> CaduceusResult<()> {
        let row: Option<(String, i64, String)> = self
            .conn
            .query_row(
                "SELECT owner_id, fencing_token, state FROM leases WHERE issue_key = ?1",
                params![issue_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .ok();

        match row {
            Some((stored_owner, stored_token, state)) => {
                if stored_owner != owner_id {
                    return Err(CaduceusError::LeaseStale {
                        context: "verify_fencing_token",
                        stderr: format!(
                            "caller {owner_id} does not match lease owner {stored_owner}"
                        ),
                    });
                }
                let state_enum = LeaseState::from_str(&state).unwrap_or(LeaseState::Expired);
                if state_enum != LeaseState::Held {
                    return Err(CaduceusError::LeaseStale {
                        context: "verify_fencing_token",
                        stderr: format!("lease is in state {state}"),
                    });
                }
                let stored = stored_token as u64;
                if token < stored {
                    return Err(CaduceusError::FencingTokenRegression {
                        issue_key: issue_key.to_string(),
                        stale_token: token,
                        current_token: stored,
                    });
                }
                Ok(())
            }
            None => Err(CaduceusError::LeaseStale {
                context: "verify_fencing_token",
                stderr: format!("no lease found for {issue_key}"),
            }),
        }
    }

    /// Renew a lease. Only the current holder can renew. Returns
    /// `LeaseStale` if the caller is not the holder.
    pub fn renew(&mut self, issue_key: &str, owner_id: &str, ttl: Duration) -> CaduceusResult<()> {
        let now = Utc::now().timestamp();
        let expires_at = now + ttl.as_secs() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE leases SET expires_at = ?1 WHERE issue_key = ?2 AND owner_id = ?3 AND state = 'held'",
                params![expires_at, issue_key, owner_id],
            )
            .map_err(|e| CaduceusError::LeaseStale {
                context: "renew",
                stderr: format!("{e}"),
            })?;

        if updated == 0 {
            return Err(CaduceusError::LeaseStale {
                context: "renew",
                stderr: format!("no held lease for {issue_key} with owner {owner_id}"),
            });
        }
        Ok(())
    }

    /// Release a lease definitively. Uses compare-and-swap on the
    /// fencing token to prevent two releases from both succeeding.
    /// Returns `LeaseStale` if the token does not match.
    pub fn release_definitively_dead(
        &mut self,
        issue_key: &str,
        owner_id: &str,
        current_fencing_token: u64,
    ) -> CaduceusResult<()> {
        let updated = self
            .conn
            .execute(
                "UPDATE leases SET state = 'released' WHERE issue_key = ?1 AND owner_id = ?2 AND fencing_token = ?3",
                params![issue_key, owner_id, current_fencing_token as i64],
            )
            .map_err(|e| CaduceusError::LeaseStale {
                context: "release",
                stderr: format!("{e}"),
            })?;

        if updated == 0 {
            return Err(CaduceusError::LeaseStale {
                context: "release",
                stderr: format!("CAS failed for {issue_key} at token {current_fencing_token}"),
            });
        }
        Ok(())
    }
}

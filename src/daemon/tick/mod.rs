//! The single canonical tick.
//!
//! [`run`], [`run_with_config`], and [`tick`] together implement
//! the per-tick controller. The
//! controller is the only entry point the daemon's CLI exposes:
//! a no-argument `caduceus` invocation, the explicit `caduceus run`,
//! and the cron tick all funnel through [`run`].
//!
//! The order of operations is the contractually-documented
//! one:
//!
//! 1. Load + validate config, initialise structured logging.
//! 2. Take the whole-tick [`DaemonLock`]. On contention return
//!    [`TickOutcome::SkippedConcurrent`] / exit 0.
//! 3. Open [`StateStore`], [`MetaStore`], [`CadenceGate`], and
//!    enforce the rate-limit and cadence gates; persist
//!    `last_tick_started` and the gated outcome.
//! 4. Reap stale claims / abandoned worktrees.
//! 5. Build the typed GitHub [`Client`], discover watched
//!    repos, poll typed open issues, enqueue summaries.
//! 6. Acquire the next eligible entry. If no entry is
//!    eligible, finish as [`TickOutcome::Idle304`] (all
//!    responses were cached 304s) or [`TickOutcome::IdleEmpty`]
//!    otherwise.
//! 7. If the entry has a `FinalizationCheckpoint`, jump to
//!    the matching resume stage. Otherwise, verify the
//!    trigger label, fetch the issue detail, build context,
//!    discover the repo, create the worktree + branch, write
//!    the prompt.
//! 8. Spawn the worker through the canonical supervisor and
//!    classify every error into a [`FailureClass`].
//! 9. On success, run code / investigation / dry-run
//!    finalization; teardown always runs.
//! 10. Persist `last_tick_finished` and the final outcome.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::info;
use ulid::Ulid;

use crate::daemon::orchestration::{classify_error, ActiveRunGuard, Services, SystemClock};
use crate::github::poll::discover_watched_repos;
use crate::github::Client;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::logging;
use crate::scheduler::circuit::{AdmissionResult, CircuitConfig, CircuitStore};
use crate::scheduler::{DrainConfig, LeaderToken, Pool};
use crate::signals;
use crate::state::meta::{CadenceDecision, CadenceGate, MetaStore, TickOutcome};
use crate::state::queue::{DaemonLock, StateStore};
use crate::worktree::GitRunner;

use tokio::task::JoinSet;

// Public surface

/// Cron / no-argument entry point. Loads config from the
/// canonical resolver chain, initialises the structured log
/// stream, and runs a single tick under a fresh
/// [`CancellationToken`]. The exit code follows the
/// contract: 0 for processed / idle / concurrent / cadence /
/// rate-limited / cancelled outcomes; 1 for configuration,
/// corruption, invariant, or unrecovered pipeline failures.
pub fn run() -> CaduceusResult<u8> {
    // Block SIGINT/SIGTERM before any config / logging work so a
    // signal delivered during startup pends instead of hitting the
    // default disposition and killing the process (issue #270).
    // Idempotent; `run_blocking` blocks again and installs the
    // handlers / restores the mask.
    signals::block_idle_signals()
        .map_err(|err| CaduceusError::Other(format!("block idle signals: {err}")))?;
    let cfg = Config::load()?;
    let log_path = cfg.log_path.clone();
    let _log_guard = logging::init(&log_path)?;
    let outcome = run_blocking(cfg)?;
    Ok(exit_code_for(&outcome))
}

/// Run a single tick on a fresh `current_thread` runtime.
/// Exposed so `status` and the CLI's other subcommands can
/// drive a tick-style `async` driver without owning a runtime.
/// The signal listener runs concurrently with the tick and
/// shares the `CancellationToken` so a SIGINT or SIGTERM
/// cancels the in-flight work and the orchestrator returns
/// `TickOutcome::Cancelled` / exit 0.
pub fn run_blocking(cfg: Config) -> CaduceusResult<TickOutcome> {
    // Block SIGINT/SIGTERM before the runtime spawns its worker
    // threads (issue #270). Every worker thread inherits the blocked
    // mask and restores its own mask via the `on_thread_start` hook
    // below once `install_idle_handlers` has installed the tokio
    // handlers; the orchestrator thread restores its mask right after
    // the eager registration inside `block_on`. This closes the
    // pre-registration default-disposition window (a blocked signal
    // pends and is delivered to the installed handler on unblock,
    // never to the default disposition) and guarantees handler
    // installation precedes the tick arm. Idempotent with the block in
    // [`run`] / `main`.
    signals::block_idle_signals()
        .map_err(|err| CaduceusError::Other(format!("block idle signals: {err}")))?;
    // A multi-threaded runtime is required so the sync finalize
    // helpers (commit / push / status) can drive their async git
    // operations via `tokio::task::block_in_place` + `Handle::block_on`.
    // `block_in_place` is only valid on a multi-threaded runtime; a
    // `current_thread` runtime would panic there. The tick itself is a
    // single sequential async flow, so the worker pool only matters to
    // `block_in_place`, not to per-tick concurrency.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .on_thread_start(|| {
            // Workers inherit the blocked SIGINT/SIGTERM mask from the
            // orchestrator thread. Wait for the tokio handlers, then
            // restore this worker's mask so worker subprocesses never
            // inherit a blocked SIGTERM (supervisor TERM-to-KILL
            // contract).
            crate::daemon::signals::unblock_worker_after_handlers_installed();
        })
        .build()
        .map_err(|err| CaduceusError::Other(format!("build tokio runtime: {err}")))?;
    let cancellation = CancellationToken::new();
    // Open the host-wide lease store once per process. The pool
    // consults this before granting a permit so overlapping cron
    // ticks cannot exceed the configured `worker_parallelism`
    // (issue #106 — closes the SCHED-001 "bounded single-host
    // concurrency" contract gap). `LeaseStore::open` returns
    // `CaduceusError::State(...)` on I/O failure, which surfaces as
    // a non-zero exit per the CLI contract.
    // Lease store is opened lazily on first pool.admit() — idle ticks
    // never create state.db on disk.
    let pool = Arc::new(
        Pool::new(
            cfg.worker_parallelism,
            DrainConfig::from_seconds_and_ms(cfg.drain_timeout_seconds, cfg.backpressure_budget_ms),
        )
        .with_lease_store_dir(
            cfg.state_dir.clone(),
            std::time::Duration::from_secs(cfg.worker_lease_ttl_seconds),
        ),
    );
    rt.block_on(async move {
        // Wake the runtime workers even if registration fails or the
        // closure panics, so `Runtime::drop` never deadlocks on them.
        let _wake_workers = signals::WakeWorkersGuard;
        // Eagerly register both signal streams BEFORE the select! arms
        // are polled, so the tick arm is never polled with handlers
        // uninstalled. Registration is synchronous (tokio
        // `signal_enable` → `signal_hook_registry::register`), so once
        // this returns the OS-level handlers exist.
        let (int_stream, term_stream) = signals::install_idle_handlers()
            .map_err(|err| CaduceusError::Other(format!("install idle signal handlers: {err}")))?;
        // Restore this thread's mask immediately after registration —
        // a signal that was pending during the blocked window is now
        // delivered to the installed handler (graceful cancel), and
        // the second-signal escalation path stays fully unblocked.
        signals::unblock_idle_signals()
            .map_err(|err| CaduceusError::Other(format!("unblock idle signals: {err}")))?;
        tokio::select! {
        outcome = run_with_config(cfg, Arc::clone(&pool), cancellation.clone()) => outcome,
        // The signal listener's first signal drains the worker
        // pool and then cancels the shared token, so the tick
        // side returns on its own with `TickOutcome::Cancelled`.
        // The listener itself continues to await a possible
        // second signal so the orchestrator can escalate to
        // immediate kill.
        res = signals::listen_on(pool, cancellation.clone(), int_stream, term_stream) => {
        match res {
        Ok(()) => Ok(TickOutcome::Cancelled),
        Err(err) => Err(CaduceusError::Other(format!(
        "signal listener: {err}"
        ))),
        }
        }
        }
    })
}

/// Like [`run`] but accepts a pre-loaded [`Config`] and a
/// [`CancellationToken`]. Tests use this signature so they
/// can drive a tick with a custom config and cancel the
/// tick before it returns. Production code paths go through
/// [`run`].
pub async fn run_with_config(
    cfg: Config,
    pool: Arc<Pool>,
    cancellation: CancellationToken,
) -> CaduceusResult<TickOutcome> {
    let clock: Arc<dyn crate::daemon::orchestration::Clock> = Arc::new(SystemClock);
    let client = Arc::new(Client::with_config(&cfg)?);
    let git = GitRunner::new(&cfg);
    let disk = Arc::new(crate::infra::disk::DiskPressureGuard::from_config(&cfg));
    let services = Services::production(
        &cfg,
        clock,
        Arc::clone(&client),
        git,
        Arc::clone(&pool),
        disk,
    );
    tick(cfg, services, pool, cancellation).await
}

/// The canonical per-tick controller. Takes ownership of the
/// [`Config`] and a [`Services`] bundle so the tests can swap
/// fakes. The function follows the contractually-documented
/// order exactly and never panics on external input.
pub async fn tick(
    cfg: Config,
    services: Services,
    pool: Arc<Pool>,
    cancellation: CancellationToken,
) -> CaduceusResult<TickOutcome> {
    let state_dir = cfg.state_dir.clone();

    // 0. Initialize daemon-owned repository storage.
    //     This runs before any lock acquisition so the directories
    //     are guaranteed to exist before the first tick attempts
    //     to use them.
    let storage = crate::repo::Storage::new(cfg.repo_storage_root.clone());
    storage.ensure_dirs().map_err(|err| {
        tracing::error!(
            error = %err,
            "failed to initialize repo storage at {}",
            cfg.repo_storage_root.display()
        );
        err
    })?;

    // 0.5. Install the restrictive umask for private storage.
    //     The umask is set once at process start; GitRunner's
    //     with_worktree_umask temporarily switches to 0o022 for
    //     worktree mutations and restores 0o077.
    let _ = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));

    // 0.6. One-time bounded legacy worktree registration sweep.
    //     This runs non-fatally once per process and prunes stale
    //     `.git/worktrees/` registrations from before the move to
    //     the state directory.
    static LEGACY_SWEEP_RAN: AtomicBool = AtomicBool::new(false);
    if !LEGACY_SWEEP_RAN.swap(true, Ordering::SeqCst) {
        crate::worktree::gc::prune_legacy_registrations(&cfg).await;
    }

    // 0.7. Disk-pressure sampler (issue #245). When the watchdog is
    //      enabled, spawn a background loop that samples the free
    //      space of the device-ID-deduped filesystems hosting the
    //      state dir, repo storage, and worktree base every
    //      DISK_SAMPLE_INTERVAL_SECS and folds the result into the
    //      shared guard. A breach cancels the guard's token, which
    //      terminates in-flight OCI work via the existing stop path
    //      and (via try_acquire_oci) refuses new dispatch. The task
    //      dies with the tick: in-flight runs live only inside the
    //      tick's JoinSet drain, so coverage is exactly the
    //      in-flight window.
    if services.disk.enabled() {
        let disk_guard = Arc::clone(&services.disk);
        let sampler_cancellation = cancellation.clone();
        let sampler_cfg = cfg.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                crate::infra::disk::DISK_SAMPLE_INTERVAL_SECS,
            ));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = sampler_cancellation.cancelled() => break,
                    _ = interval.tick() => {
                        let paths = crate::infra::disk::watchdog_paths(&sampler_cfg);
                        match crate::infra::disk::sample_free_bytes(&paths) {
                            Ok(samples) => disk_guard.refresh(&samples),
                            Err(err) => {
                                // Sampling failure must never kill the
                                // tick; the next interval retries.
                                tracing::warn!(
                                    error = %err,
                                    "disk-pressure sampling failed; watchdog keeps previous state"
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    // 1. Check scheduler leadership. If another tick holds the
    //    scheduler lock, skip (concurrent). Unlike the old
    //    whole-tick DaemonLock, the scheduler lock is held only
    //    during short state-mutation transactions, not the
    //    entire tick.
    let _leader_guard = match LeaderToken::try_acquire(&state_dir)? {
        Some(token) => token,
        None => {
            info!("concurrent tick holds scheduler lock; skipping");
            return Ok(TickOutcome::SkippedConcurrent);
        }
    };
    // Drop the leader token immediately — we only checked for
    // contention. State-mutation sections below acquire the
    // lock again via `LeaderToken::with_lock`.
    drop(_leader_guard);

    // 2. Open the metadata + state stores and enforce the
    //    rate-limit and cadence gates.
    let use_sqlite = cfg.state_backend == "sqlite";
    let (meta, oci_daemon_id) = LeaderToken::with_lock(&state_dir, || {
        let meta = if use_sqlite {
            MetaStore::open_sqlite(&state_dir)?
        } else {
            MetaStore::open(&state_dir)?
        };
        // Establish the installation identity during daemon initialization,
        // before any OCI executor can render labels or create a container.
        // Keep generation under the scheduler lock so concurrent daemon
        // starts cannot replace the persisted identity between read/write.
        let daemon_id = if cfg.executor_mode == crate::executor::ExecutorKind::Oci {
            Some(meta.get_or_create_installation_uuid()?)
        } else {
            None
        };
        Ok((meta, daemon_id))
    })?;
    let gate = if use_sqlite {
        CadenceGate::open_with_store(MetaStore::open_sqlite(&state_dir)?)
    } else {
        LeaderToken::with_lock(&state_dir, || CadenceGate::open(&state_dir))?
    };
    let now = services.clock.now();
    gate.record_tick_started(now)?;
    let precheck = gate.precheck(now, cfg.poll_interval_seconds);
    if let Some(gate_outcome) = precheck.tick_outcome() {
        let rate_limit = if matches!(precheck, CadenceDecision::RateLimited { .. }) {
            meta.snapshot().rate_limit
        } else {
            None
        };
        let _ = gate.record_tick_finished(
            now,
            gate_outcome,
            None,
            cfg.poll_interval_seconds,
            rate_limit.as_ref().map(dummy_rate_limit_info).as_ref(),
            None,
        );
        info!(?gate_outcome, "tick skipped by gate");
        return Ok(gate_outcome);
    }

    // 3. Reap stale claims / abandoned worktrees.
    let store = Arc::new(LeaderToken::with_lock(&state_dir, || {
        if use_sqlite {
            StateStore::open_sqlite(&state_dir)
        } else {
            StateStore::open(&state_dir)
        }
    })?);

    // 3.1. OCI startup recovery is deliberately before stale-claim
    // handling and before the dispatcher can accept a new claim. The
    // identity is loaded first, then the same adapter teardown path is
    // used to converge labeled containers and durable run rows.
    if cfg.executor_mode == crate::executor::ExecutorKind::Oci {
        let daemon_id = oci_daemon_id
            .as_deref()
            .expect("OCI initialization must load installation UUID");
        let oci_state = Arc::new(crate::state::oci_run::OciRunDao::new(
            crate::state::store::open_in(&state_dir)?,
        ));
        crate::executor::oci_lifecycle::reconcile_installation(
            &cfg,
            oci_state,
            daemon_id,
            cancellation.clone(),
        )
        .await?;
    }

    let _ = crate::state::queue::reap_stale_claims(
        &state_dir,
        services.clock.now(),
        cfg.stale_run_hours,
    )
    .await;

    // 3.5. Reclaim stale, unclaimed worktrees (best-effort).
    if !cfg.worktree_gc_disabled {
        match DaemonLock::try_acquire(&state_dir) {
            Ok(Some(_gc_lock)) => {
                match crate::worktree::gc::gc(&cfg, cfg.worktree_gc_older_than_days, false).await {
                    Ok(removed) => info!(removed, "auto worktree gc completed"),
                    Err(err) => {
                        tracing::warn!(error = %err, "auto worktree gc failed; continuing tick")
                    }
                }
                // Review worktree sweep (#299): same lock, same
                // best-effort log-and-continue contract.
                let review_runner = GitRunner::new(&cfg);
                match crate::repo::review_worktree::gc_review_worktrees(
                    &cfg,
                    &review_runner,
                    cfg.worktree_gc_older_than_days,
                    false,
                )
                .await
                {
                    Ok(removed) => info!(removed, "review worktree gc completed"),
                    Err(err) => {
                        tracing::warn!(error = %err, "review worktree gc failed; continuing tick")
                    }
                }
            }
            Ok(None) => info!("auto worktree gc skipped; daemon lock is contended"),
            Err(err) => {
                tracing::warn!(error = %err, "auto worktree gc lock failed; continuing tick")
            }
        }
    }

    // 3.5a. Prune archived worktrees older than the retention cap.
    match crate::worktree::attic::sweep(&cfg).await {
        Ok(removed) => {
            if removed > 0 {
                info!(removed, "auto attic sweep completed")
            }
        }
        Err(err) => tracing::warn!(error = %err, "auto attic sweep failed; continuing tick"),
    }

    // 3.6. Open the SQLite state store for circuit breaker access.
    let sqlite_conn = crate::state::store::open_in(&state_dir)?;
    let circuit_store = CircuitStore::new(sqlite_conn, CircuitConfig::from_config(&cfg));

    // 3.7. Poll awaiting-review entries for PR merge status.
    let poll_client: Arc<Client> = Arc::clone(services.github.inner());
    if let Err(err) = poll_awaiting_review_entries(store.as_ref(), poll_client.as_ref(), &cfg).await
    {
        tracing::warn!(error = %err, "awaiting-review poll failed (best-effort)");
    }

    // 4. Build the GitHub client and discover watched repos.
    let client: Arc<Client> = Arc::clone(services.github.inner());
    let repos = match discover_watched_repos(client.as_ref(), &cfg).await {
        Ok(repos) => repos,
        Err(err) => {
            let class = classify_error(&err);
            // Rate-limit and other non-fatal infrastructure
            // errors must return `Ok` with the matching
            // `TickOutcome` so the cron contract's exit-0
            // mapping applies. The observation is already
            // persisted by `finish_tick_failure`.
            if let Some(outcome) = class.non_fatal_outcome() {
                finish_tick_failure(&gate, now, &cfg, &meta, class, Some(&err))?;
                return Ok(outcome);
            }
            finish_tick_failure(&gate, now, &cfg, &meta, class, Some(&err))?;
            return Err(err);
        }
    };
    if repos.is_empty() {
        finish_tick_outcome(&gate, &meta, now, TickOutcome::IdleEmpty, None, None)?;
        return Ok(TickOutcome::IdleEmpty);
    }

    // 5. Poll for the two trigger labels and enqueue summaries.
    let mut any_304 = false;
    let mut any_200 = false;
    let mut last_error: Option<CaduceusError> = None;
    for repo in &repos {
        match poll_repo(repo, &client, &cfg, store.as_ref()).await {
            Ok(Outcome304(true)) => {
                any_304 = true;
            }
            Ok(Outcome304(false)) => {
                any_200 = true;
            }
            Err(err) => {
                last_error = Some(err);
                break;
            }
        }
    }
    if let Some(err) = last_error {
        let class = classify_error(&err);
        // Same cron-contract rule: rate-limit and other
        // non-fatal errors return `Ok` with the matching
        // outcome so the CLI's exit-0 mapping applies.
        if let Some(outcome) = class.non_fatal_outcome() {
            finish_tick_failure(&gate, now, &cfg, &meta, class, Some(&err))?;
            return Ok(outcome);
        }
        finish_tick_failure(&gate, now, &cfg, &meta, class, Some(&err))?;
        return Err(err);
    }

    // 6. Drain the queue into a bounded `JoinSet` dispatch loop.
    //
    // Each iteration: (a) `acquire_next` under `LeaderToken::with_lock`
    // (existing API, no change); (b) the circuit-breaker probe inlined
    // from the synchronous version (preserves NeedsAttention routing);
    // (c) `pool.admit(&repo_key)` — a lazy, per-iteration acquire that
    // keeps `in_flight <= worker_parallelism`; (d) `run_claim` is
    // spawned into a `JoinSet` that owns its own `_admit: Admission`
    // (RAII on drop), preserving the per-task admission lifetime
    // contract from #91 / PR #104; (e) when the JoinSet reaches
    // `worker_parallelism`, `join_next` is awaited and the result is
    // folded into the outer `http_status` / `last_error`. The loop
    // drains until `acquire_next` returns `None`; a final drain loop
    // ensures no spawned task is orphaned.
    //
    // `services` and `meta` are wrapped in `Arc` so the spawned
    // closures can hold their own `'static` references; `store` and
    // `client` were already `Arc`-backed. Each spawned task gets its
    // own `Option<u16>` http-status slot, returned alongside the
    // `CaduceusResult<TickOutcome>` so the dispatch loop can fold
    // any non-`None` value into the outer `http_status`.
    let services = Arc::new(services);
    let meta = Arc::new(meta);
    let client: Arc<Client> = Arc::clone(services.github.inner());
    let mut http_status: Option<u16> = None;
    let mut last_error: Option<CaduceusError> = None;
    let worker_parallelism = cfg.worker_parallelism.max(1) as usize;
    // Per-tick claim cap (issue #108): bounds how many queue entries
    // one tick will claim before returning. 0 == unbounded (the
    // pre-#108 drain-the-queue behavior). The cap only stops claiming
    // new entries; the JoinSet drain below still runs to completion.
    let max_issues_per_tick = cfg.max_issues_per_tick;
    let mut processed: u32 = 0;

    let mut set: JoinSet<(CaduceusResult<TickOutcome>, Option<u16>)> = JoinSet::new();

    'dispatch: loop {
        // Per-tick claim cap: check before acquiring the next entry
        // so exactly `max_issues_per_tick` claims happen per tick.
        if max_issues_per_tick != 0 && processed >= max_issues_per_tick {
            break 'dispatch;
        }
        // 6.1. Acquire the next eligible entry under the
        //      scheduler lock. Existing API, preserved exactly.
        let run_id_candidate = Ulid::new().to_string();
        let store_clone = Arc::clone(&store);
        let clock_now = services.clock.now();
        let claimed = match LeaderToken::with_lock(&state_dir, || {
            store_clone.acquire_next(&run_id_candidate, std::process::id(), clock_now)
        })? {
            Some(c) => c,
            None => break 'dispatch,
        };
        // Count the claim immediately: circuit-blocked and
        // pool-saturated entries are requeued via `continue 'dispatch`
        // below, but they still consume quota so a flood of blocked
        // entries cannot starve real work (issue #108).
        processed += 1;

        // 6.2. Circuit-breaker probe (inlined from the prior
        //      synchronous path). On a circuit-blocked repo or
        //      provider, requeue the entry and continue the
        //      drain; the spec's "partial cancellation releases
        //      guards" scenario demands the tick continue past
        //      a single blocked entry.
        let repo_key = format!("{}/{}", claimed.entry.key.owner, claimed.entry.key.repo);
        let repo_admit =
            circuit_store.try_admit("repository", &repo_key, services.clock.as_ref())?;
        let provider_admit =
            circuit_store.try_admit("provider", "github", services.clock.as_ref())?;

        let circuit_blocked = matches!(
            (&repo_admit, &provider_admit),
            (
                AdmissionResult::CircuitOpen { .. } | AdmissionResult::MaxDegradedAgeExceeded,
                _
            ) | (
                _,
                AdmissionResult::CircuitOpen { .. } | AdmissionResult::MaxDegradedAgeExceeded
            )
        );

        if circuit_blocked {
            let log_path = state_dir.join("processor.log");
            let mut guard = ActiveRunGuard::new(
                claimed.claim.clone(),
                Arc::clone(&store),
                log_path,
                claimed.entry.key.clone(),
            );
            let err = CaduceusError::CircuitOpen {
                scope: "repository",
                scope_id: repo_key.clone(),
                retry_after: 1800,
                probe_in_flight: false,
            };
            let class = classify_error(&err);
            let outcome = handle_infra_or_retry(cfg.clone(), &mut guard, &err, class).await?;
            let _ = outcome;
            if last_error.is_none() {
                last_error = Some(err);
            }
            continue 'dispatch;
        }

        // 6.3. Admit the entry to the worker pool. The pool's
        //      lazy, per-iteration acquire ensures
        //      `in_flight <= worker_parallelism` whenever the
        //      dispatch loop pulls a new claim. The lease store
        //      wired into the pool at construction promotes the
        //      "one worker per repo" contract from process-local
        //      to host-wide (issue #106 / SCHED-001).
        let admit = match pool.admit(&format!("repo:{repo_key}"), &repo_key).await {
            Ok(a) => a,
            Err(err) => {
                // PoolSaturated / DrainTimeout is an
                // infrastructure failure: requeue with backoff
                // via the existing path and continue the drain.
                let log_path = state_dir.join("processor.log");
                let mut guard = ActiveRunGuard::new(
                    claimed.claim.clone(),
                    Arc::clone(&store),
                    log_path,
                    claimed.entry.key.clone(),
                );
                let class = classify_error(&err);
                let outcome = handle_infra_or_retry(cfg.clone(), &mut guard, &err, class).await?;
                let _ = outcome;
                if last_error.is_none() {
                    last_error = Some(err);
                }
                continue 'dispatch;
            }
        };

        // 6.4. Spawn `run_claim` into the JoinSet. The closure
        //      owns its own `Admission` (RAII on drop), an owned
        //      `ActiveRunGuard`, and an owned per-task
        //      `http_status` slot. The outer slot is folded on
        //      each `join_next` completion.
        let log_path = state_dir.join("processor.log");
        let guard = ActiveRunGuard::new(
            claimed.claim.clone(),
            Arc::clone(&store),
            log_path,
            claimed.entry.key.clone(),
        );
        let services_for_task = Arc::clone(&services);
        let pool_for_task = Arc::clone(&pool);
        let store_for_task = Arc::clone(&store);
        let meta_for_task = Arc::clone(&meta);
        let client_for_task = Arc::clone(&client);
        let cfg_for_task = cfg.clone();
        let cancellation_for_task = cancellation.clone();

        set.spawn(async move {
            let mut guard = guard;
            let mut task_http_status: Option<u16> = None;
            let outcome = run_claim(
                cfg_for_task,
                &services_for_task,
                pool_for_task,
                admit,
                store_for_task.as_ref(),
                &meta_for_task,
                client_for_task,
                claimed,
                &mut guard,
                cancellation_for_task,
                &mut task_http_status,
            )
            .await;
            (outcome, task_http_status)
        });

        // 6.5. When the JoinSet is at the cap, await one
        //      completion before pulling the next claim. This
        //      enforces `in_flight < worker_parallelism - 1`
        //      before each new acquire (Req 2 — lazy acquire
        //      maintains invariant).
        if set.len() >= worker_parallelism {
            if let Some(joined) = set.join_next().await {
                match joined {
                    Ok((Ok(_outcome), Some(status))) => {
                        if http_status.is_none() {
                            http_status = Some(status);
                        }
                    }
                    Ok((Ok(_outcome), None)) => {}
                    Ok((Err(err), status_opt)) => {
                        if let Some(s) = status_opt {
                            if http_status.is_none() {
                                http_status = Some(s);
                            }
                        }
                        if last_error.is_none() {
                            last_error = Some(err);
                        }
                    }
                    Err(join_err) => {
                        // Task panicked or was aborted. Surface
                        // as last_error but do not abort the
                        // tick — drain remaining workers.
                        if last_error.is_none() {
                            last_error = Some(CaduceusError::Other(format!(
                                "worker task join error: {join_err}"
                            )));
                        }
                    }
                }
            }
        }
    }

    // 6.6. Drain any remaining in-flight tasks. We pull from
    //      the JoinSet until empty; every per-task http_status
    //      gets folded into the outer slot, every per-task
    //      error is captured in `last_error`.
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((Ok(_outcome), Some(status))) => {
                if http_status.is_none() {
                    http_status = Some(status);
                }
            }
            Ok((Ok(_outcome), None)) => {}
            Ok((Err(err), status_opt)) => {
                if let Some(s) = status_opt {
                    if http_status.is_none() {
                        http_status = Some(s);
                    }
                }
                if last_error.is_none() {
                    last_error = Some(err);
                }
            }
            Err(join_err) => {
                if last_error.is_none() {
                    last_error = Some(CaduceusError::Other(format!(
                        "worker task join error: {join_err}"
                    )));
                }
            }
        }
    }

    // 7. Persist the final tick outcome. With the queue
    //    drained, the tick reports `Idle304` if every polled
    //    repo returned a cached 304, otherwise `IdleEmpty`.
    let outcome = if any_304 && !any_200 {
        TickOutcome::Idle304
    } else {
        TickOutcome::IdleEmpty
    };
    finish_tick_outcome(
        &gate,
        meta.as_ref(),
        now,
        outcome,
        http_status,
        last_error.as_ref(),
    )?;
    Ok(outcome)
}

// Submodule declarations and re-exports. These preserve the historical
// `crate::daemon::tick` public surface.

pub mod awaiting_review;
pub mod per_claim;
pub mod resume;

use self::awaiting_review::*;
use self::per_claim::*;
use self::resume::*;

pub use self::awaiting_review::{
    exit_code_for_tests, extract_http_status_for_tests, map_phase_to_outcome_for_tests,
    outcome_for_class_for_tests,
};

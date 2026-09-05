//! OCI executor — dispatches workers via Docker or Podman CLI.
//!
//! The executor runs the pre-flight probe (`engine_probe`) to collect
//! the runtime facts (worktree owner, `.git` type, engine mode) and
//! create the daemon-owned host artifacts, resolves a typed
//! [`SandboxSpec`] from the sandbox config and those facts, renders
//! the `create` argv with the pure renderer, acquires and verifies the
//! configured image, then delegates to [`oci_lifecycle::run_oci_lifecycle`]
//! for the single crash-safe container lifecycle. The state DAO is injected
//! through the config's state directory.
//!
//! The renderer is the sole argv producer in the crate: `resolve` owns
//! every host-path and identity decision, `engine_probe` owns runtime facts
//! and daemon-owned artifact creation, `oci_image` owns image acquisition,
//! and `oci_lifecycle` only consumes already-rendered argv.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::executor::{
    engine_probe, oci_env_file, oci_image, oci_lifecycle, oci_platform, sandbox_renderer,
    sandbox_spec, Executor, ExecutorOutcome, ExecutorSpec,
};
use crate::infra::config::Config;
use crate::infra::disk::DiskPressureGuard;
use crate::infra::error::CaduceusResult;
use crate::readiness;
use crate::state::meta::MetaStore;
use crate::state::oci_run::OciRunDao;
use crate::state::store;
use crate::worker::worker_contract::WORKER_RESULT_FILE;

/// Executor that dispatches workers via Docker or Podman CLI.
#[derive(Clone, Debug)]
pub struct OciExecutor {
    cfg: Config,
    /// Host disk-pressure watchdog: refuses new dispatch and
    /// terminates in-flight work on breach (issue #245).
    disk: Arc<DiskPressureGuard>,
    readiness_options: readiness::ProbeOptions,
}

impl OciExecutor {
    /// Wrap a config snapshot and the shared disk-pressure guard.
    pub fn new(cfg: Config, disk: Arc<DiskPressureGuard>) -> Self {
        Self {
            cfg,
            disk,
            readiness_options: readiness::ProbeOptions::default(),
        }
    }

    /// Construct an executor with an explicit readiness probe seam.
    /// Production callers should use [`Self::new`].
    pub fn new_with_readiness_options(
        cfg: Config,
        disk: Arc<DiskPressureGuard>,
        readiness_options: readiness::ProbeOptions,
    ) -> Self {
        Self {
            cfg,
            disk,
            readiness_options,
        }
    }
}

impl Executor for OciExecutor {
    fn run<'a>(
        &'a self,
        spec: &'a ExecutorSpec,
    ) -> Pin<Box<dyn Future<Output = CaduceusResult<ExecutorOutcome>> + Send + 'a>> {
        Box::pin(async move {
            // 0. Disk-pressure gate: refuse new OCI dispatch while the host
            //    watchdog is breached before any readiness probe or other
            //    per-run side effect (issue #245).
            self.disk.try_acquire_oci()?;

            // 1. Live readiness gate: no cached doctor result is consulted.
            let image_acquisition =
                readiness::assert_live_with_options(&self.cfg, &self.readiness_options).await?;

            // 2. Load and durably establish the installation identity
            //    before any labels or create argv are rendered.
            let meta = if self.cfg.state_backend == "sqlite" {
                MetaStore::open_sqlite(&self.cfg.state_dir)?
            } else {
                MetaStore::open(&self.cfg.state_dir)?
            };
            let daemon_id = meta.get_or_create_installation_uuid()?;

            // 3. Pre-flight probe: collect the runtime facts (worktree
            //    owner uid/gid, host `.git` type, engine mode) and
            //    create the daemon-owned host artifacts. Every
            //    unsupported-configuration refusal (typed
            //    `OciIdentityUnsupported`) is raised here — before any
            //    `create` argv exists, so `oci_lifecycle` is never
            //    reached on a refusal path.
            let runtime =
                engine_probe::probe_runtime_facts_with_daemon_id(&self.cfg, spec, &daemon_id)
                    .await?;

            // 4. Resolve the closed typed spec. All host-path,
            //    identity, and mount decisions happen here; the
            //    renderer invents nothing.
            let resolved = sandbox_spec::resolve(self.cfg.sandbox(), &runtime, spec)?;

            // 5. Open the state database.
            let db_path = self.cfg.state_dir.join(store::DB_FILENAME);
            let conn = store::open(&db_path)?;
            let dao = OciRunDao::new(conn);

            // 6. Write the assembled environment (canonical + compat +
            //    resolved `pass_env` from `spec.environment`) to the
            //    daemon-private, randomly named, mode-0600 env file
            //    under `state_dir/oci-runs/<run_id>` (issue #249). Its
            //    path is the ONLY env surface that reaches argv: the
            //    renderer emits one `--env-file` token and zero `-e`
            //    tokens. A creation failure fails the run pre-create —
            //    there is no `-e` fallback (design D4/D5). The
            //    lifecycle owns the deletion guard and drops it
            //    immediately after `create` returns.
            let engine = self.cfg.sandbox().engine;
            let run_dir = runtime.state_dir.join("oci-runs").join(&runtime.run_id);
            let env_map: BTreeMap<String, String> =
                resolved.environment().iter().cloned().collect();
            let env_file = oci_env_file::OciEnvFile::create(&run_dir, &env_map)?;
            let argv = sandbox_renderer::render_with_env_files(
                &resolved,
                engine,
                &[env_file.path().to_path_buf()],
            );

            // 7. Record the immutable worker image facts acquired by the
            //    readiness gate before the lifecycle can insert its Created
            //    state or invoke create. The run directory is already
            //    created by the probe and keeps provenance scoped to this run.
            let host = oci_platform::host_platform();
            oci_image::write_acquisition_provenance(
                &run_dir,
                engine,
                resolved.image_ref(),
                self.cfg.sandbox().pull_policy,
                &host,
                &runtime.run_id,
                &image_acquisition,
            );

            // 8. Run the lifecycle with the rendered argv. The run's
            //    lifecycle token is linked with the watchdog token so
            //    a disk-pressure breach terminates in-flight work via
            //    the existing stop path (issue #245). The adapter
            //    carries the target-neutral display identity
            //    (`WorkTarget::display()`) — issue runs
            //    `owner/repo#N`, PR runs `owner/repo#pr/N` (DAR §6.1).
            let issue_id = spec.target.display();
            let adapter = oci_lifecycle::OciAdapter::new(
                engine,
                Arc::new(dao),
                self.cfg.state_dir.clone(),
                daemon_id,
                issue_id,
                sha256_of(&spec.worker_command.join(" ")),
                argv,
                Some(env_file),
            );
            let outcome = oci_lifecycle::run_oci_lifecycle(
                &resolved,
                &adapter,
                &oci_lifecycle::LifecycleTimeouts::from_config(&self.cfg),
                spec.cancellation.child_token(),
                self.disk.watchdog_token(),
            )
            .await?;
            Ok(ExecutorOutcome {
                outcome,
                result_path: runtime.output_dir.join(WORKER_RESULT_FILE),
            })
        })
    }
}

fn sha256_of(input: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

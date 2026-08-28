//! Host disk-pressure watchdog (issue #245).
//!
//! Samples the free space of the DISTINCT filesystems hosting the
//! daemon's write surfaces (state dir, repo storage / worktrees, and
//! the OCI output dirs — which share the state-dir filesystem by
//! construction), deduplicated by device ID so a single underlying
//! filesystem is sampled exactly once.
//!
//! On breach (any sampled filesystem's free space falls below the
//! configured `reserved_host_disk_mb` floor) the guard:
//!
//! 1. cancels its internal [`CancellationToken`] — linked into
//!    in-flight OCI run lifecycles so breach terminates in-flight
//!    work via the existing stop → kill → rm path; and
//! 2. refuses new OCI dispatch with the typed
//!    [`CaduceusError::OciDiskPressure`] until the reserve recovers.
//!
//! Recovery applies a hysteresis margin: free space must exceed the
//! floor by [`DISK_HYSTERESIS_BYTES`] before new work is re-enabled,
//! so the state does not flap at the threshold.
//!
//! This is a host-level mitigation, NOT a per-container byte quota:
//! `/workspace` remains a host bind mount with no per-container byte
//! quota; a runaway run can still fill the filesystem between
//! samples (bounded by [`DISK_SAMPLE_INTERVAL_SECS`] detection
//! latency plus the stop/kill timeouts).
//!
//! The pure state machine ([`transition`]) and the sampler
//! ([`sample_free_bytes`]) are separated so the math is unit-tested
//! against synthetic layouts without any filesystem.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use tokio_util::sync::CancellationToken;

use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Recovery hysteresis margin: after a breach, free space must exceed
/// the reserved floor by this many bytes before new OCI dispatch is
/// re-enabled. A fixed absolute margin stays meaningful for small
/// reserves where a percentage would collapse to noise; erring
/// toward *harder* recovery is the fail-safe direction.
pub const DISK_HYSTERESIS_BYTES: u64 = 256 * 1024 * 1024;

/// Sampling cadence, in seconds. Bounds detection latency for a
/// breach to this interval plus the stop/kill timeouts, satisfying
/// the "terminated within a bounded time" requirement.
pub const DISK_SAMPLE_INTERVAL_SECS: u64 = 30;

/// One filesystem-level free-space observation, deduplicated by
/// device ID. `representative_path` is the first input path (or its
/// nearest existing ancestor) that mapped to this device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskSample {
    /// `st_dev` of the sampled filesystem (`MetadataExt::dev()`).
    pub device_id: u64,
    /// Bytes available to unprivileged processes
    /// (`f_bavail * f_frsize`).
    pub free_bytes: u64,
    /// First path that mapped to this device.
    pub representative_path: PathBuf,
}

/// Watchdog verdict for one sampling round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PressureState {
    /// Free space is above the reserve everywhere (or the watchdog
    /// has not yet observed a breach).
    Healthy,
    /// At least one sampled filesystem is below the reserve. The
    /// recorded device/path/free/reserved describe the breaching
    /// sample.
    Breached {
        device_id: u64,
        path: String,
        free_bytes: u64,
        reserved_bytes: u64,
    },
}

/// Resolve *path* to its nearest existing ancestor (bounded walk up
/// to the root) so not-yet-created roots (e.g. `workdir_base` before
/// the first worktree) still sample the filesystem that will host
/// them. Returns the input unchanged when it exists.
fn nearest_existing(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return current;
        }
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent.to_path_buf(),
            _ => return current,
        }
    }
}

/// Sample the free space of the distinct filesystems hosting *paths*.
///
/// Each path is resolved to its nearest existing ancestor, its device
/// ID read via [`std::os::unix::fs::MetadataExt::dev()`], and free
/// bytes read via `nix::sys::statvfs::statvfs` (`f_bavail *
/// f_frsize` — bytes available to unprivileged processes). Samples
/// are deduplicated by device ID (first path wins for
/// `representative_path`); the output order is stable on input order,
/// making the pure transition deterministic.
pub fn sample_free_bytes(paths: &[PathBuf]) -> CaduceusResult<Vec<DiskSample>> {
    let mut samples: Vec<DiskSample> = Vec::new();
    for path in paths {
        let resolved = nearest_existing(path);
        let device_id = std::fs::metadata(&resolved)
            .map_err(|e| {
                CaduceusError::Io(std::io::Error::other(format!(
                    "disk watchdog: stat {}: {e}",
                    resolved.display()
                )))
            })?
            .dev();
        // Device-ID dedup: same physical filesystem is never sampled
        // twice, even across bind mounts.
        if samples.iter().any(|s| s.device_id == device_id) {
            continue;
        }
        let stat = nix::sys::statvfs::statvfs(&resolved).map_err(|e| {
            CaduceusError::Io(std::io::Error::other(format!(
                "disk watchdog: statvfs {}: {e}",
                resolved.display()
            )))
        })?;
        // Bytes available to unprivileged processes: `f_bavail *
        // f_frsize` via the nix accessor methods.
        let free_bytes = stat.blocks_available() as u64 * stat.fragment_size() as u64;
        samples.push(DiskSample {
            device_id,
            free_bytes,
            representative_path: resolved,
        });
    }
    Ok(samples)
}

/// The filesystem roots the watchdog samples for a config: the state
/// dir, the repo storage root, and the worktree base. OCI output dirs
/// live at `<state_dir>/oci-runs/<run_id>/output` and therefore share
/// the state-dir filesystem sample by construction — per-run path
/// registration is unnecessary.
pub fn watchdog_paths(cfg: &Config) -> Vec<PathBuf> {
    vec![
        cfg.state_dir.clone(),
        cfg.repo_storage_root.clone(),
        cfg.workdir_base.clone(),
    ]
}

/// Pure watchdog transition (no shared state, deterministic).
///
/// * `Healthy` → `Breached` when ANY sample has
///   `free_bytes < reserved_bytes` (the breaching device is
///   recorded).
/// * `Breached` → `Healthy` only when EVERY sample has
///   `free_bytes > reserved_bytes + hysteresis_bytes` (strict `>` —
///   recovery at exactly the floor does not re-enable). An empty
///   sample set never recovers a breach.
/// * Otherwise the state persists.
pub fn transition(
    state: &PressureState,
    samples: &[DiskSample],
    reserved_bytes: u64,
    hysteresis_bytes: u64,
) -> PressureState {
    match state {
        PressureState::Healthy => {
            for sample in samples {
                if sample.free_bytes < reserved_bytes {
                    return PressureState::Breached {
                        device_id: sample.device_id,
                        path: sample.representative_path.display().to_string(),
                        free_bytes: sample.free_bytes,
                        reserved_bytes,
                    };
                }
            }
            PressureState::Healthy
        }
        breached @ PressureState::Breached { .. } => {
            let recovered = !samples.is_empty()
                && samples
                    .iter()
                    .all(|s| s.free_bytes > reserved_bytes + hysteresis_bytes);
            if recovered {
                PressureState::Healthy
            } else {
                breached.clone()
            }
        }
    }
}

/// Arc-shareable watchdog guard. The only shared state is the
/// `RwLock<PressureState>` verdict and the internal root
/// [`CancellationToken`]; `refresh` is idempotent and [`transition`]
/// is pure, so concurrent OCI runs observe a consistent verdict.
#[derive(Debug)]
pub struct DiskPressureGuard {
    enabled: bool,
    reserved_bytes: u64,
    state: RwLock<PressureState>,
    root_token: CancellationToken,
}

impl DiskPressureGuard {
    /// Construct the guard from a config. Enabled iff
    /// `cfg.sandbox` is present and `sandbox.reserved_host_disk_mb >
    /// 0` (`0` disables the watchdog — no sampling, no enforcement).
    pub fn from_config(cfg: &Config) -> Self {
        let (enabled, reserved_mb) = match &cfg.sandbox {
            Some(sandbox) => {
                let mb = sandbox.reserved_host_disk_mb;
                (mb > 0, mb)
            }
            None => (false, 0),
        };
        Self {
            enabled,
            reserved_bytes: reserved_mb.saturating_mul(1024 * 1024),
            state: RwLock::new(PressureState::Healthy),
            root_token: CancellationToken::new(),
        }
    }

    /// Disabled guard for tests and TrustedHost-mode wiring.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            reserved_bytes: 0,
            state: RwLock::new(PressureState::Healthy),
            root_token: CancellationToken::new(),
        }
    }

    /// True when the watchdog samples and enforces.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Fold a fresh sample set into the verdict. On a
    /// Healthy→Breached transition the internal root token is
    /// cancelled, which terminates in-flight OCI work through the
    /// linked per-run watchdog tokens. Tokens are never un-cancelled:
    /// recovery only re-enables *new* dispatch; already-terminated
    /// runs stay terminated.
    pub fn refresh(&self, samples: &[DiskSample]) {
        if !self.enabled {
            return;
        }
        let mut state = self.state.write().expect("disk watchdog lock");
        let next = transition(&state, samples, self.reserved_bytes, DISK_HYSTERESIS_BYTES);
        if matches!(
            (&*state, &next),
            (PressureState::Healthy, PressureState::Breached { .. })
        ) {
            self.root_token.cancel();
        }
        *state = next;
    }

    /// Refuse new OCI dispatch while breached. Returns
    /// [`CaduceusError::OciDiskPressure`] populated from the stored
    /// breach state; `Ok(())` when healthy or disabled.
    pub fn try_acquire_oci(&self) -> CaduceusResult<()> {
        if !self.enabled {
            return Ok(());
        }
        let state = self.state.read().expect("disk watchdog lock");
        match &*state {
            PressureState::Healthy => Ok(()),
            PressureState::Breached {
                device_id,
                path,
                free_bytes,
                reserved_bytes,
            } => Err(CaduceusError::OciDiskPressure {
                path: path.clone(),
                device_id: *device_id,
                free_bytes: *free_bytes,
                reserved_bytes: *reserved_bytes,
            }),
        }
    }

    /// A child of the internal root token, for linking into an OCI
    /// run's lifecycle. Cancelling the root (on breach) cancels every
    /// child; cancelling a child never affects the root or siblings.
    pub fn watchdog_token(&self) -> CancellationToken {
        self.root_token.child_token()
    }
}

//! Unit tests for the host disk-pressure watchdog (issue #245):
//! pure transition math across same/multi-device layouts, statvfs
//! sampling with device-ID dedup, and guard behavior (breach refusal,
//! hysteresis recovery, disabled no-op).

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use caduceus::executor::sandbox_spec::is_engine_socket_path;
use caduceus::infra::config::Config;
use caduceus::infra::disk::{
    transition, DiskPressureGuard, DiskSample, PressureState, DISK_HYSTERESIS_BYTES,
};
use caduceus::infra::error::CaduceusError;

/// Synthetic sample helper.
fn sample(device_id: u64, free_bytes: u64, path: &str) -> DiskSample {
    DiskSample {
        device_id,
        free_bytes,
        representative_path: PathBuf::from(path),
    }
}

// ---------------------------------------------------------------------------
// transition — pure state machine (tasks 12.2)
// ---------------------------------------------------------------------------

/// (a) Healthy → Breached when ANY device is under the reserve;
/// the breaching device is recorded.
#[test]
fn transition_breaches_on_any_device() {
    let reserved = 2048 * 1024 * 1024;

    // Single device.
    let next = transition(
        &PressureState::Healthy,
        &[sample(1, reserved - 1, "/state")],
        reserved,
        DISK_HYSTERESIS_BYTES,
    );
    assert_eq!(
        next,
        PressureState::Breached {
            device_id: 1,
            path: "/state".to_string(),
            free_bytes: reserved - 1,
            reserved_bytes: reserved,
        }
    );

    // Three devices; only the second breaches.
    let next = transition(
        &PressureState::Healthy,
        &[
            sample(1, reserved + 10 * 1024 * 1024, "/state"),
            sample(2, reserved - 1, "/repos"),
            sample(3, reserved + 10 * 1024 * 1024, "/workdirs"),
        ],
        reserved,
        DISK_HYSTERESIS_BYTES,
    );
    match next {
        PressureState::Breached {
            device_id,
            path,
            free_bytes,
            reserved_bytes,
        } => {
            assert_eq!(device_id, 2);
            assert_eq!(path, "/repos");
            assert_eq!(free_bytes, reserved - 1);
            assert_eq!(reserved_bytes, reserved);
        }
        other => panic!("expected Breached; got {other:?}"),
    }
}

/// (b) A breach persists while any device is below the margin — even
/// when every device is above the bare reserve.
#[test]
fn breach_persists_below_margin() {
    let reserved = 1024 * 1024 * 1024;
    let breached = PressureState::Breached {
        device_id: 2,
        path: "/repos".to_string(),
        free_bytes: reserved - 1,
        reserved_bytes: reserved,
    };

    // Still below the bare reserve on one device.
    let next = transition(
        &breached,
        &[
            sample(1, reserved + DISK_HYSTERESIS_BYTES * 4, "/state"),
            sample(2, reserved - 1, "/repos"),
        ],
        reserved,
        DISK_HYSTERESIS_BYTES,
    );
    assert_eq!(next, breached);

    // Above the reserve but NOT above reserve + margin.
    let next = transition(
        &breached,
        &[
            sample(1, reserved + DISK_HYSTERESIS_BYTES * 4, "/state"),
            sample(2, reserved + DISK_HYSTERESIS_BYTES, "/repos"),
        ],
        reserved,
        DISK_HYSTERESIS_BYTES,
    );
    assert_eq!(next, breached, "exact margin is not recovery (strict >)");

    // Two-device layout: one device well above the margin, the other
    // only just above the reserve — still breached.
    let next = transition(
        &breached,
        &[
            sample(1, reserved + DISK_HYSTERESIS_BYTES * 4, "/state"),
            sample(2, reserved + 1, "/repos"),
        ],
        reserved,
        DISK_HYSTERESIS_BYTES,
    );
    assert_eq!(next, breached);
}

/// (c) Recovery only when EVERY device exceeds `reserved + 256 MiB`;
/// (d) recovery at exactly the floor does NOT re-enable (strict `>`).
#[test]
fn recovery_requires_margin_on_every_device() {
    let reserved = 512 * 1024 * 1024;
    let breached = PressureState::Breached {
        device_id: 3,
        path: "/workdirs".to_string(),
        free_bytes: 0,
        reserved_bytes: reserved,
    };

    // Every device above the margin → recovered.
    let next = transition(
        &breached,
        &[
            sample(1, reserved + DISK_HYSTERESIS_BYTES + 1, "/state"),
            sample(2, reserved + DISK_HYSTERESIS_BYTES + 1, "/repos"),
            sample(3, reserved + DISK_HYSTERESIS_BYTES + 1, "/workdirs"),
        ],
        reserved,
        DISK_HYSTERESIS_BYTES,
    );
    assert_eq!(next, PressureState::Healthy);

    // Exact floor on one device → NOT recovered.
    let next = transition(
        &breached,
        &[
            sample(1, reserved + DISK_HYSTERESIS_BYTES + 1, "/state"),
            sample(2, reserved + DISK_HYSTERESIS_BYTES, "/repos"),
            sample(3, reserved + DISK_HYSTERESIS_BYTES + 1, "/workdirs"),
        ],
        reserved,
        DISK_HYSTERESIS_BYTES,
    );
    assert_eq!(next, breached);

    // Same-device layout collapsing to one sample: recovery above
    // the margin works from a single-sample breach too.
    let breached_single = PressureState::Breached {
        device_id: 1,
        path: "/state".to_string(),
        free_bytes: 0,
        reserved_bytes: reserved,
    };
    let next = transition(
        &breached_single,
        &[sample(1, reserved + DISK_HYSTERESIS_BYTES + 1, "/state")],
        reserved,
        DISK_HYSTERESIS_BYTES,
    );
    assert_eq!(next, PressureState::Healthy);
}

/// An empty sample set never recovers a breach and never breaches a
/// healthy state (state_dir always exists, so this is defensive).
#[test]
fn empty_samples_persist_state() {
    let reserved = 1024 * 1024;
    assert_eq!(
        transition(
            &PressureState::Healthy,
            &[],
            reserved,
            DISK_HYSTERESIS_BYTES
        ),
        PressureState::Healthy
    );
    let breached = PressureState::Breached {
        device_id: 1,
        path: "/state".to_string(),
        free_bytes: 0,
        reserved_bytes: reserved,
    };
    assert_eq!(
        transition(&breached, &[], reserved, DISK_HYSTERESIS_BYTES),
        breached
    );
}

// ---------------------------------------------------------------------------
// sample_free_bytes — device-ID dedup + ancestor resolution (12.3)
// ---------------------------------------------------------------------------

#[test]
fn same_filesystem_paths_dedup_to_one_sample() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a");
    let b = dir.path().join("b").join("c");
    std::fs::create_dir_all(&a).expect("create dirs");
    std::fs::create_dir_all(&b).expect("create dirs");

    let samples =
        caduceus::infra::disk::sample_free_bytes(&[a.clone(), b.clone()]).expect("sample");
    assert_eq!(samples.len(), 1, "same-device paths must dedup");
    let s = &samples[0];
    // The dedup key is the device ID from MetadataExt::dev().
    let expected_dev = std::fs::metadata(&a).expect("stat").dev();
    assert_eq!(s.device_id, expected_dev);
    assert_eq!(s.representative_path, a, "first path wins");
    assert!(s.free_bytes > 0, "statvfs must report positive free space");
}

#[test]
fn missing_path_resolves_to_existing_ancestor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("not-yet-created").join("deep");

    let samples =
        caduceus::infra::disk::sample_free_bytes(std::slice::from_ref(&missing)).expect("sample");
    assert_eq!(samples.len(), 1);
    assert_eq!(
        samples[0].representative_path,
        dir.path(),
        "nearest existing ancestor must be sampled"
    );
    let expected_dev = std::fs::metadata(dir.path()).expect("stat").dev();
    assert_eq!(samples[0].device_id, expected_dev);

    // Dedup with the existing ancestor: same device → one sample.
    let samples = caduceus::infra::disk::sample_free_bytes(&[dir.path().to_path_buf(), missing])
        .expect("sample");
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].representative_path, dir.path());
}

/// The watchdog path set covers the three config roots.
#[test]
fn watchdog_paths_cover_config_roots() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::test_defaults(tmp.path());
    let paths = caduceus::infra::disk::watchdog_paths(&cfg);
    assert_eq!(
        paths,
        vec![
            cfg.state_dir.clone(),
            cfg.repo_storage_root.clone(),
            cfg.workdir_base.clone(),
        ]
    );
}

// ---------------------------------------------------------------------------
// DiskPressureGuard — breach refusal, recovery, disabled no-op (12.4)
// ---------------------------------------------------------------------------

fn guard_with_reserved_mb(mb: u64) -> (Config, DiskPressureGuard) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::test_defaults(tmp.path());
    cfg.sandbox.as_mut().expect("sandbox").reserved_host_disk_mb = mb;
    let guard = DiskPressureGuard::from_config(&cfg);
    (cfg, guard)
}

#[test]
fn guard_breach_refuses_with_typed_error() {
    let (cfg, guard) = guard_with_reserved_mb(64);
    assert!(guard.enabled(), "reserved > 0 enables the watchdog");
    let state_dir = cfg.state_dir.display().to_string();
    let reserved_bytes = 64 * 1024 * 1024;

    guard.refresh(&[sample(7, reserved_bytes - 1, &state_dir)]);
    let err = guard
        .try_acquire_oci()
        .expect_err("breached guard must refuse");
    match err {
        CaduceusError::OciDiskPressure {
            path,
            device_id,
            free_bytes,
            reserved_bytes: reserved,
        } => {
            assert_eq!(path, state_dir);
            assert_eq!(device_id, 7);
            assert_eq!(free_bytes, reserved_bytes - 1);
            assert_eq!(reserved, reserved_bytes);
        }
        other => panic!("expected OciDiskPressure; got {other:?}"),
    }
    // Breach cancels the watchdog token (in-flight termination).
    assert!(guard.watchdog_token().is_cancelled());
}

#[test]
fn guard_recovery_requires_margin() {
    let (_, guard) = guard_with_reserved_mb(64);
    let reserved_bytes = 64 * 1024 * 1024;

    // Breach.
    guard.refresh(&[sample(1, 0, "/state")]);
    assert!(guard.try_acquire_oci().is_err());

    // Recovery at exactly the floor does not re-enable.
    guard.refresh(&[sample(1, reserved_bytes, "/state")]);
    assert!(
        guard.try_acquire_oci().is_err(),
        "recovery at exactly the floor must not re-enable"
    );

    // Above floor but below margin — still refused.
    guard.refresh(&[sample(1, reserved_bytes + DISK_HYSTERESIS_BYTES, "/state")]);
    assert!(guard.try_acquire_oci().is_err());

    // Above floor + margin — new dispatch is accepted again.
    guard.refresh(&[sample(
        1,
        reserved_bytes + DISK_HYSTERESIS_BYTES + 1,
        "/state",
    )]);
    guard.try_acquire_oci().expect("recovered guard accepts");
    // Tokens are never un-cancelled: the terminated in-flight runs
    // stay terminated; only NEW dispatch is re-enabled.
    assert!(guard.watchdog_token().is_cancelled());
}

#[test]
fn disabled_guard_never_refuses() {
    let guard = DiskPressureGuard::disabled();
    assert!(!guard.enabled());
    guard.refresh(&[sample(1, 0, "/state")]);
    guard
        .try_acquire_oci()
        .expect("disabled guard never refuses");
    assert!(!guard.watchdog_token().is_cancelled());
}

#[test]
fn from_config_without_sandbox_is_disabled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::test_defaults(tmp.path());
    cfg.sandbox = None;
    let guard = DiskPressureGuard::from_config(&cfg);
    assert!(!guard.enabled());
    guard.try_acquire_oci().expect("no sandbox → disabled");
}

/// The guard is Arc-shareable and refresh is idempotent under
/// concurrent acquire attempts.
#[test]
fn guard_is_arc_shareable() {
    let (_, guard) = guard_with_reserved_mb(64);
    let shared: Arc<DiskPressureGuard> = Arc::new(guard);
    let a = Arc::clone(&shared);
    let b = Arc::clone(&shared);
    a.refresh(&[sample(1, 0, "/state")]);
    assert!(b.try_acquire_oci().is_err());
}

// ---------------------------------------------------------------------------
// is_engine_socket_path (12.5)
// ---------------------------------------------------------------------------

#[test]
fn engine_socket_path_positives_and_negatives() {
    for positive in [
        "/var/run/docker.sock",
        "/run/podman/podman.sock",
        "docker.sock",
        "podman.sock",
    ] {
        assert!(
            is_engine_socket_path(Path::new(positive)),
            "{positive} must be detected"
        );
    }
    for negative in [
        "/workspace",
        "/output",
        "/workspace/.git",
        "/var/run/docker.sock.bak",
        "socker",
        "",
    ] {
        assert!(
            !is_engine_socket_path(Path::new(negative)),
            "{negative:?} must not be detected"
        );
    }
}

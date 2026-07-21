//! Tests for the EphemeralSecretFile — secret transport with mode-0600
//! and Drop cleanup on every exit path including panic (AC-04).

use std::os::unix::fs::PermissionsExt;
use std::panic;

use caduceus::executor::secret_transport::EphemeralSecretFile;

// ---------------------------------------------------------------------------
// no_secret_leak (AC-04)
// ---------------------------------------------------------------------------

#[test]
fn no_secret_leak() {
    let values = vec![
        (
            "GITHUB_TOKEN".to_string(),
            "SUPERSECRET_leak_test".to_string(),
        ),
        ("FOO".to_string(), "another_secret_value".to_string()),
    ];
    let handle = EphemeralSecretFile::write(&values).expect("write must succeed");
    let debug = format!("{handle:?}");
    let display = format!("{handle}");
    assert!(
        !debug.contains("SUPERSECRET_leak_test"),
        "secret leaked into Debug: {debug}"
    );
    assert!(
        !display.contains("SUPERSECRET_leak_test"),
        "secret leaked into Display: {display}"
    );
    for path in handle.paths() {
        let argv_token = path.to_string_lossy().to_string();
        assert!(
            !argv_token.contains("SUPERSECRET_leak_test"),
            "secret leaked into path string: {argv_token}"
        );
    }
}

// ---------------------------------------------------------------------------
// drop_deletes_file
// ---------------------------------------------------------------------------

#[test]
fn drop_deletes_file() {
    let values = vec![("DROP_DEL".to_string(), "V".to_string())];
    let handle = EphemeralSecretFile::write(&values).expect("write");
    let paths: Vec<_> = handle.paths().to_vec();
    for p in &paths {
        assert!(
            p.exists(),
            "secret file must exist before drop: {}",
            p.display()
        );
    }
    drop(handle);
    for p in &paths {
        assert!(
            !p.exists(),
            "secret file must NOT exist after drop: {}",
            p.display()
        );
    }
}

// ---------------------------------------------------------------------------
// drop_runs_on_panic (AC-03)
// ---------------------------------------------------------------------------

#[test]
fn drop_runs_on_panic() {
    let values = vec![("PANIC_DEL".to_string(), "V".to_string())];
    let path = {
        let handle = EphemeralSecretFile::write(&values).expect("write");
        let p = handle.paths()[0].to_path_buf();
        assert!(p.exists(), "file must exist before panic");
        // Panic while holding the handle.
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _h = handle;
            panic!("simulated panic");
        }));
        assert!(result.is_err(), "panic must propagate");
        p
    };
    // Drop ran during unwind — the file must be gone.
    assert!(
        !path.exists(),
        "secret file must be deleted on panic-drop: {}",
        path.display()
    );
}

// ---------------------------------------------------------------------------
// file_mode_0600 (AC-04)
// ---------------------------------------------------------------------------

#[test]
fn file_mode_0600() {
    let values = vec![("MODE_TEST".to_string(), "V".to_string())];
    let handle = EphemeralSecretFile::write(&values).expect("write");
    for path in handle.paths() {
        let meta = std::fs::metadata(path).expect("metadata");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "secret file mode must be 0600; got {:o} at {}",
            mode,
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// handle_path_only (AC-04)
// ---------------------------------------------------------------------------

#[test]
fn handle_path_only() {
    let values = vec![("HANDLE_PATH".to_string(), "secret_value_xyz".to_string())];
    let handle = EphemeralSecretFile::write(&values).expect("write");
    // SecretHandle exposes only paths, never values.
    let debug = format!("{handle:?}");
    assert!(
        !debug.contains("secret_value_xyz"),
        "Debug must not contain secret value: {debug}"
    );
    // Paths reference files, not values
    for p in handle.paths() {
        let s = format!("{p:?}");
        assert!(!s.contains("secret_value_xyz"));
    }
}

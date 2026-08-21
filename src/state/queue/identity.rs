//! Portable process identity helpers.
//!
//! A claim identity is formatted as `<uuid>:<epoch>:<start_ticks>`:
//!
//! * `uuid` is a persistent UUID v4 stored in
//!   `<state_dir>/daemon-identity`;
//! * `epoch` is the platform boot-time epoch; and
//! * `start_ticks` is the per-process start value supplied by the P1
//!   [`ProcessIdentity`] provider.
//!
//! The UUID file is never rotated. After a reboot, old claims are therefore
//! distinguishable by `(uuid-mismatch OR epoch-mismatch)`, the same shape as a
//! daemon crash-loop. The reaper still decides whether to reap a claim using
//! process liveness and claim age.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use uuid::Uuid;

use crate::worker::supervisor::process_lifecycle::IDENTITY;

const DAEMON_IDENTITY_FILENAME: &str = "daemon-identity";

/// Return the persistent daemon UUID, creating it on first use.
///
/// The final file is installed with a same-directory hard link from a fully
/// written and synced temporary file. A hard link avoids replacing a UUID
/// another daemon may have installed concurrently, while preserving the
/// atomic-install property of the identity file.
pub(crate) fn load_or_create_daemon_uuid(state_dir: &Path) -> String {
    let path = state_dir.join(DAEMON_IDENTITY_FILENAME);
    match fs::read_to_string(&path) {
        Ok(contents) => return contents,
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            tracing::debug!(
                error = %err,
                path = %path.display(),
                "daemon identity read failed; using an ephemeral fallback"
            );
            return Uuid::new_v4().to_string();
        }
        Err(_) => {}
    }

    let generated = Uuid::new_v4().to_string();
    if let Err(err) = install_daemon_uuid(&path, state_dir, &generated) {
        tracing::debug!(
            error = %err,
            path = %path.display(),
            "daemon identity creation failed; using an ephemeral fallback"
        );
        return generated;
    }

    fs::read_to_string(&path).unwrap_or(generated)
}

fn install_daemon_uuid(path: &Path, state_dir: &Path, uuid: &str) -> std::io::Result<()> {
    fs::create_dir_all(state_dir)?;
    let tmp = state_dir.join(format!(
        ".{DAEMON_IDENTITY_FILENAME}.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));

    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        set_mode_0600(&file)?;
        file.write_all(uuid.as_bytes())?;
        file.sync_all()?;
        drop(file);

        match fs::hard_link(&tmp, path) {
            Ok(()) => {
                let _ = sync_dir(state_dir);
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(err) => Err(err),
        }
    })();

    let cleanup = fs::remove_file(&tmp);
    if result.is_ok() {
        cleanup.map(|_| ())
    } else {
        let _ = cleanup;
        result
    }
}

fn set_mode_0600(file: &File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    match File::open(dir) {
        Ok(file) => file.sync_all(),
        Err(err) => Err(err),
    }
}

/// Read the platform boot-time epoch in seconds.
pub(crate) fn boot_epoch() -> u64 {
    platform_boot_epoch().unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn platform_boot_epoch() -> Option<u64> {
    let timespec = match nix::time::clock_gettime(nix::time::ClockId::CLOCK_BOOTTIME) {
        Ok(timespec) => timespec,
        Err(err) => {
            tracing::debug!(error = %err, "clock_gettime(CLOCK_BOOTTIME) failed");
            return None;
        }
    };
    if timespec.tv_sec() < 0 {
        tracing::debug!(
            seconds = timespec.tv_sec(),
            "clock_gettime(CLOCK_BOOTTIME) returned a negative epoch"
        );
        return None;
    }
    u64::try_from(timespec.tv_sec()).ok()
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "linux")))]
fn platform_boot_epoch() -> Option<u64> {
    tracing::debug!("boot epoch is unavailable on this Unix platform");
    None
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn platform_boot_epoch() -> Option<u64> {
    const CTL_KERN: libc::c_int = 1;
    const KERN_BOOTTIME: libc::c_int = 21;

    extern "C" {
        fn sysctl(
            name: *mut libc::c_int,
            namelen: libc::c_uint,
            oldp: *mut libc::c_void,
            oldlenp: *mut libc::size_t,
            newp: *mut libc::c_void,
            newlen: libc::size_t,
        ) -> libc::c_int;
    }

    let mut name = [CTL_KERN, KERN_BOOTTIME];
    let mut boot_time = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut old_len = std::mem::size_of::<libc::timeval>() as libc::size_t;
    let result = unsafe {
        sysctl(
            name.as_mut_ptr(),
            name.len() as libc::c_uint,
            (&mut boot_time as *mut libc::timeval).cast(),
            &mut old_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        tracing::debug!(
            error = %std::io::Error::last_os_error(),
            "sysctl(KERN_BOOTTIME) failed"
        );
        return None;
    }
    if old_len != std::mem::size_of::<libc::timeval>() as libc::size_t {
        tracing::debug!(
            old_len,
            expected = std::mem::size_of::<libc::timeval>(),
            "sysctl(KERN_BOOTTIME) returned an unexpected size"
        );
        return None;
    }
    u64::try_from(boot_time.tv_sec).ok()
}

#[cfg(not(unix))]
fn platform_boot_epoch() -> Option<u64> {
    tracing::debug!("boot epoch is unavailable on this platform");
    None
}

/// Return the current process start ticks through P1's platform provider.
/// Unsupported platforms and unobservable processes degrade to zero, which
/// preserves the old helper's best-effort behavior.
pub(crate) fn identity_start_ticks(pid: u32) -> u64 {
    let Ok(pid) = i32::try_from(pid) else {
        tracing::debug!(pid, "process id does not fit the identity provider");
        return 0;
    };
    IDENTITY.start_ticks(pid).unwrap_or(0)
}

/// Compose the opaque process identity stored in a claim file.
pub(crate) fn process_start_identity(state_dir: &Path, pid: u32) -> String {
    format!(
        "{}:{}:{}",
        load_or_create_daemon_uuid(state_dir),
        boot_epoch(),
        identity_start_ticks(pid)
    )
}

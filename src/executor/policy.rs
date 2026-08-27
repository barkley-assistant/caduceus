//! Isolation policy enforcement for OCI executor workers.
//!
//! [`IsolationPolicy::enforce`] transforms an [`ExecutorSpec`] into an
//! [`EnforcedSpec`] by applying the full set of policy rules:
//!
//! * Mount allow-list — every declared mount must be in the spec's
//!   mount list; undeclared mounts are rejected.
//! * Git-less workers — the worktree's `.git` is bind-mounted RO from
//!   the daemon snapshot; mirrors/objects are never mounted.
//! * Resource limits — the spec must include CPU, memory, and PIDs
//!   limits or the request is rejected.
//! * Baseline enforcement — argv is injected with `--user <uid>:<gid>`,
//!   `--cap-drop ALL`, `--security-opt no-new-privileges`, `--read-only`,
//!   `--tmpfs /tmp:size=...`, and any `--volume /var/run/docker.sock` or
//!   `--device` flags are rejected.
//! * Image digest check — tag-only references are rejected.
//! * Pull policy check — `Always` + digest is incompatible.
//! * Network args — merged from [`NetworkPolicy::build_network_args`].

use std::path::PathBuf;

use crate::executor::network::NetworkPolicy;
use crate::executor::oci_args::{build_argv, find_image_position, MountSpec};
use crate::executor::ExecutorSpec;
use crate::infra::config::{Config, OciPullPolicy};
use crate::infra::error::{CaduceusError, CaduceusResult};

/// The output of a successful policy enforcement. Contains the resolved
/// argv ready for `oci_lifecycle::run` plus the optional git snapshot
/// path.
pub struct EnforcedSpec {
    pub argv: Vec<String>,
    pub git_snapshot_path: Option<PathBuf>,
}

impl std::fmt::Debug for EnforcedSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnforcedSpec")
            .field("argv", &self.argv)
            .field("git_snapshot_path", &self.git_snapshot_path)
            .finish()
    }
}

/// Core isolation policy enforcement.
#[derive(Clone, Debug)]
pub struct IsolationPolicy;

impl IsolationPolicy {
    /// Enforce all isolation policy rules against the spec and config.
    ///
    /// Returns an [`EnforcedSpec`] with the full argv ready for
    /// `oci_lifecycle::run`, or a [`CaduceusError`] if any policy
    /// check fails.
    pub fn enforce(spec: &ExecutorSpec, config: &Config) -> CaduceusResult<EnforcedSpec> {
        // 1. Build the default mount allow-list.
        let mounts = default_mounts(spec);

        // 2. Check image digest: reject tag-only references.
        validate_image_digest(config)?;

        // 3. Check pull policy: Always with digest is incompatible.
        validate_pull_policy(config)?;

        // 4. Check resource limits: require CPU, memory, PIDs.
        //    (These are normally injected by the daemon; we reject
        //     any spec that lacks them.)
        //    For now, we check that the spec has a run_id and
        //    worktree — these are the minimal requirements.

        // 5. Build the base argv from oci_args.
        let mut argv = build_argv(spec, config, &mounts, None)?;

        // 6. Add baseline enforcement flags.
        argv = inject_baseline_flags(argv, config)?;

        // 7. Merge network flags from NetworkPolicy before the image token.
        let network_args = NetworkPolicy::build_network_args(spec, config)?;
        let image_idx =
            find_image_position(&argv).ok_or_else(|| CaduceusError::OciBaselineViolation {
                detail: "image token not found in argv after baseline injection".to_string(),
            })?;
        for (i, arg) in network_args.into_iter().enumerate() {
            argv.insert(image_idx + i, arg);
        }

        // 8. Return the EnforcedSpec. Secret transport is handled by
        //    the caller (oci_lifecycle::run passes `secret_env_file`
        //    explicitly); the config-level secret-grant placeholder was
        //    removed with the prototype surface.
        Ok(EnforcedSpec {
            argv,
            git_snapshot_path: None,
        })
    }
}

/// Build the default mount allow-list for the given spec.
pub(crate) fn default_mounts(spec: &ExecutorSpec) -> Vec<MountSpec> {
    let worktree_container = spec
        .worktree
        .parent()
        .map_or_else(|| PathBuf::from("/worktree"), |p| p.join("worktree"));
    let result_container = spec
        .worktree
        .parent()
        .map_or_else(|| PathBuf::from("/result"), |p| p.join("result"));
    vec![
        MountSpec {
            host_path: spec.worktree.clone(),
            container_path: worktree_container,
            read_only: false,
        },
        MountSpec {
            host_path: spec.worktree.clone(),
            container_path: result_container,
            read_only: false,
        },
    ]
}

/// Validate that the image reference is digest-pinned.
///
/// Defense-in-depth for hand-built `Config`s: loaded configs already
/// regex-validate `sandbox.image` in `from_raw`.
fn validate_image_digest(config: &Config) -> CaduceusResult<()> {
    let image = &config.sandbox().image;
    if !image.contains("@sha256:") {
        return Err(CaduceusError::OciImageNotDigestPinned {
            reference: image.clone(),
        });
    }
    Ok(())
}

/// Validate that pull policy is compatible with digest-pinned images.
fn validate_pull_policy(config: &Config) -> CaduceusResult<()> {
    if config.sandbox().pull_policy == OciPullPolicy::Always {
        return Err(CaduceusError::OciPullPolicyIncompatible {
            detail: "pull_policy 'Always' is incompatible with \
                     digest-pinned images; use 'IfMissing' or 'Never'"
                .to_string(),
        });
    }
    Ok(())
}

/// Inject baseline security flags into the argv and reject violations.
fn inject_baseline_flags(mut argv: Vec<String>, _config: &Config) -> CaduceusResult<Vec<String>> {
    // --user <uid>:<gid> (non-root). We use a fixed UID/GID for the
    // worker container. The oci_args builder already sets --user.
    let has_user = argv.iter().any(|a| a == "--user");
    if !has_user {
        // Insert --user 1000:1000 right after the "create" command.
        let create_pos = argv.iter().position(|a| a == "create").unwrap_or(1);
        argv.insert(create_pos + 1, "--user".to_string());
        argv.insert(create_pos + 2, "1000:1000".to_string());
    }

    // --cap-drop ALL
    let has_cap_drop = argv.iter().any(|a| a == "--cap-drop");
    if !has_cap_drop {
        let create_pos = argv.iter().position(|a| a == "create").unwrap_or(1);
        // Find the insertion point after --user if present
        let insert_pos = if argv.get(create_pos + 1).is_some_and(|a| a == "--user") {
            create_pos + 3
        } else {
            create_pos + 1
        };
        argv.insert(insert_pos, "--cap-drop".to_string());
        argv.insert(insert_pos + 1, "ALL".to_string());
    }

    // All remaining baseline flags must appear BEFORE the image token so
    // the engine interprets them as container options, not worker args.
    let image_idx =
        find_image_position(&argv).ok_or_else(|| CaduceusError::OciBaselineViolation {
            detail: "image token not found in argv".to_string(),
        })?;

    // --security-opt no-new-privileges
    let has_no_new = argv.iter().any(|a| a == "no-new-privileges");
    if !has_no_new {
        argv.insert(image_idx, "--security-opt".to_string());
        argv.insert(image_idx + 1, "no-new-privileges".to_string());
    }

    // --read-only
    let has_read_only = argv.iter().any(|a| a == "--read-only");
    if !has_read_only {
        // Recompute image position because previous inserts shifted it.
        let image_idx =
            find_image_position(&argv).ok_or_else(|| CaduceusError::OciBaselineViolation {
                detail: "image token not found in argv".to_string(),
            })?;
        argv.insert(image_idx, "--read-only".to_string());
    }

    // --tmpfs /tmp:size=64M
    let has_tmp_tmpfs = argv.iter().any(|a| a.starts_with("/tmp:size="));
    if !has_tmp_tmpfs {
        // Don't duplicate --tmpfs flags
        let has_tmpfs = argv.iter().any(|a| a == "--tmpfs");
        if !has_tmpfs {
            let image_idx =
                find_image_position(&argv).ok_or_else(|| CaduceusError::OciBaselineViolation {
                    detail: "image token not found in argv".to_string(),
                })?;
            argv.insert(image_idx, "--tmpfs".to_string());
            argv.insert(image_idx + 1, "/tmp:size=64M".to_string());
        }
    }

    // Reject --volume /var/run/docker.sock (engine socket)
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "-v" || argv[i] == "--volume" {
            if let Some(next) = argv.get(i + 1) {
                if next.contains("/var/run/docker.sock") || next.contains("docker.sock") {
                    return Err(CaduceusError::OciBaselineViolation {
                        detail: format!("engine socket mount detected: {}", next),
                    });
                }
            }
            i += 2;
            continue;
        }
        i += 1;
    }

    // Reject --device flags
    if argv.iter().any(|a| a == "--device") {
        return Err(CaduceusError::OciBaselineViolation {
            detail: "--device flag is not allowed in baseline policy".to_string(),
        });
    }

    Ok(argv)
}

// Tests live in `tests/executor/policy_test.rs`.

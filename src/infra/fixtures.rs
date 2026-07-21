//! Cross-document fixtures — the canonical lists that documentation,
//! the Python bridge, and the Rust daemon must agree on.
//!
//! These lists are the **single source of truth** for cross-document
//! pinning under Task 8.1. Public docs, the bridge Python module, and
//! the daemon's `Config` all derive their names from this module.
//!
//! The companion test (`tests/docs_contract_test.rs`) loads
//! `plugin-assets/worker-bridge.py`, `skills/caduceus/SKILL.md`,
//! `README.md`, and the `__init__.py` adapter docs, then asserts that
//! each public artifact references these names verbatim. The set is
//! exhaustive on the daemon side: every key listed here has a matching
//! `pub` field on `crate::infra::config::Config` and every env var listed here
//! is emitted (or denied) by `crate::worker::sanitized_env`.
//!
//! **Edit discipline.** These fixtures are part of the v0.1 normative
//! contract. Changing them requires updating the cross-document test in
//! `tests/docs_contract_test.rs` *and* the related public
//! documentation, and verifying the Python bridge test suite
//! `tests/bridge_test.py` still passes its own mirror of these names.
//! Don't add to this list casually.

/// Canonical `Config` field names. Mirrors the public surface of
/// `crate::infra::config::Config`. These names appear in
/// `~/.config/caduceus/config.yaml`, `~/.hermes/config.yaml` under a
/// `caduceus:` section, and the daemon's documentation. The list is
/// sorted lexicographically so operators grep-ing docs find every key.
pub const CANONICAL_CONFIG_KEYS: &[&str] = &[
    "api_base",
    "comment_forbidden_strings",
    "comment_ignore_patterns",
    "discovery_max_pages",
    "dry_run",
    "executor_mode",
    "feedback_author_allowlist",
    "github_token",
    "http_timeout_seconds",
    "log_path",
    "max_retries_per_issue",
    "oci_cli",
    "oci_image_digest",
    "oci_kill_timeout_seconds",
    "oci_pull_policy",
    "oci_reconcile_timeout_seconds",
    "oci_stop_timeout_seconds",
    "poll_interval_seconds",
    "reduced_containment_acknowledged",
    "retry_backoff_seconds",
    "run_retention_days",
    "stale_run_hours",
    "state_dir",
    "ticket_label_code",
    "ticket_label_investigation",
    "watched_repos",
    "workdir_base",
    "worker_command",
    "worker_env_allowlist",
    "worker_timeout_seconds",
    "transcript_max_bytes",
];

/// Canonical worker environment variable names exported by the daemon
/// for every worker invocation. Each name is mirrored in the Python
/// bridge's `REQUIRED_ENV_VARS` tuple. The contract requires the daemon
/// to set every one of these; if the bridge ever needs a new field
/// the listing here is the contract bump.
pub const CANONICAL_WORKER_ENV_VARS: &[&str] = &[
    "CADUCEUS_BRANCH_NAME",
    "CADUCEUS_CONTEXT_JSON",
    "CADUCEUS_ISSUE_BODY",
    "CADUCEUS_ISSUE_LABELS_JSON",
    "CADUCEUS_ISSUE_NUMBER",
    "CADUCEUS_ISSUE_REPO",
    "CADUCEUS_ISSUE_TITLE",
    "CADUCEUS_RUN_ID",
    "CADUCEUS_WORKTREE_PATH",
];

/// Default allowlist for the worker environment (CONTRACTS.md "Worker
/// environment and result"). Operators may extend
/// `worker_env_allowlist`; the daemon's `validate_worker_env_allowlist`
/// rejects partial matches and credential names. The bridge never reads
/// or writes these — they describe what the daemon *preserves* from the
/// parent environment when starting the worker.
pub const DEFAULT_ALLOWLIST_EXACT_ENV_NAMES: &[&str] = &[
    "HOME", "LANG", "LC_ALL", "PATH", "SHELL", "TERM", "TMPDIR", "USER",
];

/// Default allowlist prefix patterns (single terminal `*`). Mirrors
/// `crate::worker::DEFAULT_ALLOWLIST_PREFIXES`.
pub const DEFAULT_ALLOWLIST_PREFIX_ENV_PATTERNS: &[&str] =
    &["ANTHROPIC_*", "OPENAI_*", "OPENCODE_*", "OPENROUTER_*"];

/// Hard-deny env names: never reach the worker even when operators add
/// them to `worker_env_allowlist`. Mirrors
/// `crate::worker::DENIED_EXACT_VARS` plus the legacy
/// `AUTO_ISSUE_GITHUB_TOKEN` alias.
pub const DENIED_ENV_NAMES: &[&str] = &[
    "AUTO_ISSUE_GITHUB_TOKEN",
    "CADUCEUS_GITHUB_TOKEN",
    "GH_TOKEN",
    "GITHUB_TOKEN",
];

/// Pin to the v0.18.2 Hermes loader contract. The manifest is required
/// to use *only* the fields the loader actually reads. Anything outside
/// this list must be rejected by Caduceus's contract test before
/// reaching Hermes.
pub const HERMES_MANIFEST_FIELDS: &[&str] = &[
    "author",
    "description",
    "kind",
    "manifest_version",
    "name",
    "provides_hooks",
    "provides_tools",
    "requires_env",
    "version",
];

/// The plugin loader rejects unknown fields. Every name in this list
/// must be the *opposite* of "supported" — these are the historical
/// 0.18-era fields we MUST refuse to write into our own manifest, even
/// if a previous codepath once allowed them.
pub const HERMES_FORBIDDEN_MANIFEST_FIELDS: &[&str] = &[
    "binaries",
    "config",
    "cron_profiles",
    "files",
    "hooks",
    "profile_section",
];

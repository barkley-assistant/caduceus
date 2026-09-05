//! Cross-document fixtures — the canonical lists that documentation,
//! the Python bridge, and the Rust daemon must agree on.
//!
//! These lists are the **single source of truth** for cross-document
//! pinning. Public docs, the bridge Python module, and
//! the daemon's `Config` all derive their names from this module.
//!
//! The companion test (`tests/architecture/docs_contract_test.rs`) loads
//! `plugin-assets/worker-bridge.py`, `skills/caduceus/SKILL.md`,
//! `README.md`, and the `__init__.py` adapter docs, then asserts that
//! each public artifact references these names verbatim. The set is
//! exhaustive on the daemon side: every key listed here has a matching
//! `pub` field on `crate::infra::config::Config` and every env var listed here
//! is emitted (or denied) by `crate::worker::sanitized_env`.
//!
//! **Edit discipline.** These fixtures are part of the v0.1 normative
//! contract. Changing them requires updating the cross-document test in
//! `tests/architecture/docs_contract_test.rs` *and* the related public
//! documentation, and verifying the Python bridge test suite
//! `tests/integration/bridge_test.py` still passes its own mirror of these names.
//! Don't add to this list casually.

/// Canonical `Config` field names. Mirrors the public surface of
/// `crate::infra::config::Config`. These names appear in
/// `~/.config/caduceus/config.yaml`, `~/.hermes/config.yaml` under a
/// `caduceus:` section, and the daemon's documentation. The list is
/// sorted lexicographically so operators grep-ing docs find every key.
pub const CANONICAL_CONFIG_KEYS: &[&str] = &[
    "api_base",
    "auto_review",
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
    "max_reviews_per_tick",
    "poll_interval_seconds",
    "reduced_containment_acknowledged",
    "retry_backoff_seconds",
    "run_retention_days",
    "sandbox",
    "stale_run_hours",
    "state_dir",
    "ticket_label_code",
    "ticket_label_investigation",
    "transcript_max_bytes",
    "watched_repos",
    "workdir_base",
    "worker_command",
    "worker_env_allowlist",
    "worker_timeout_seconds",
];

/// Canonical worker environment variable names exported by the daemon.
/// This is the full **union** across both target paths (DAR §6.1):
/// the 10 historical issue-path names plus `CADUCEUS_WORK_TARGET` and
/// the four `CADUCEUS_PR_*` names. Each name is mirrored in the Python
/// bridge's `REQUIRED_ENV_VARS` tuple (set equality,
/// `docs_contract_test`). The exact per-path emission sets are pinned
/// by [`CANONICAL_WORKER_ENV_VARS_ISSUE_PATH`] and
/// [`CANONICAL_WORKER_ENV_VARS_PR_PATH`]; the union exists so the
/// bridge keeps a single required-name list while the per-path
/// fixtures assert byte-for-byte emission.
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
    "CADUCEUS_RESULT_PATH",
    "CADUCEUS_WORK_TARGET",
    "CADUCEUS_PR_BASE_SHA",
    "CADUCEUS_PR_HEAD_SHA",
    "CADUCEUS_PR_NUMBER",
    "CADUCEUS_PR_REPO",
];

/// Exact `CADUCEUS_*` set emitted for an **issue-target** run — the
/// byte-for-byte v0.1 contract (no `CADUCEUS_WORK_TARGET`, no
/// `CADUCEUS_PR_*`). `sanitized_env`'s issue arm and the OCI issue
/// arm both emit exactly this set.
pub const CANONICAL_WORKER_ENV_VARS_ISSUE_PATH: &[&str] = &[
    "CADUCEUS_BRANCH_NAME",
    "CADUCEUS_CONTEXT_JSON",
    "CADUCEUS_ISSUE_BODY",
    "CADUCEUS_ISSUE_LABELS_JSON",
    "CADUCEUS_ISSUE_NUMBER",
    "CADUCEUS_ISSUE_REPO",
    "CADUCEUS_ISSUE_TITLE",
    "CADUCEUS_RUN_ID",
    "CADUCEUS_WORKTREE_PATH",
    "CADUCEUS_RESULT_PATH",
];

/// Exact `CADUCEUS_*` set emitted for a **PR-target** run (DAR §6.1):
/// the `pr` mode marker plus the four `CADUCEUS_PR_*` values and the
/// shared run/context/worktree/result vars. No `CADUCEUS_ISSUE_*`, no
/// `CADUCEUS_ISSUE_ID` (an OCI-only issue-path compat var), no
/// `CADUCEUS_BRANCH_NAME`.
pub const CANONICAL_WORKER_ENV_VARS_PR_PATH: &[&str] = &[
    "CADUCEUS_CONTEXT_JSON",
    "CADUCEUS_PR_BASE_SHA",
    "CADUCEUS_PR_HEAD_SHA",
    "CADUCEUS_PR_NUMBER",
    "CADUCEUS_PR_REPO",
    "CADUCEUS_RESULT_PATH",
    "CADUCEUS_RUN_ID",
    "CADUCEUS_WORKTREE_PATH",
    "CADUCEUS_WORK_TARGET",
];

/// Default allowlist for the worker environment (the worker-result
/// contract in `src/worker/worker_contract.rs`). Operators may extend
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

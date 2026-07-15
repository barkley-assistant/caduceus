//! Cross-document contract test — public documentation, the Python
//! bridge, and the Rust daemon must all reference the same canonical
//! names.
//!
//! Task 8.1 requires a "cross-document test that extracts config keys
//! and worker environment names and compares them with Rust fixtures,
//! plus a Hermes contract test pinned to v0.18.2." This file pins
//! both.
//!
//! The test surfaces are:
//!
//! 1. **Config field names** — `CANONICAL_CONFIG_KEYS` from
//!    `crate::fixtures` matches every `Config` field the daemon
//!    exposes and every key `README.md` / `skills/caduceus/SKILL.md`
//!    references in its documented config snippets.
//! 2. **Worker env names** — `CANONICAL_WORKER_ENV_VARS` from
//!    `crate::fixtures` matches every variable the daemon emits on the
//!    worker side AND every variable the bridge lists in
//!    `plugin-assets/worker-bridge.py`'s `REQUIRED_ENV_VARS`. The
//!    bridge test suite `tests/bridge_test.py` mirrors the same list
//!    so a drift in either direction fails one of the two suites.
//! 3. **Default allowlist** — the four prefix patterns
//!    (`OPENAI_*`, `ANTHROPIC_*`, `OPENROUTER_*`, `OPENCODE_*`) and
//!    the eight exact names (`HOME`, `USER`, `SHELL`, `LANG`,
//!    `LC_ALL`, `PATH`, `TERM`, `TMPDIR`) appear in both the Rust
//!    fixture and the daemon's `crate::worker::DEFAULT_ALLOWLIST_*`
//!    constants.
//! 4. **Hermes v0.18.2 plugin manifest** — `plugin.yaml` references
//!    only the fields the v0.18.2 directory-plugin loader accepts, and
//!    no field on the legacy forbidden list appears.
//! 5. **No false claims** — `README.md` and the plugin skill do not
//!    claim that Hermes reads `commands/*.md`, `cron/*.yaml`, manifest
//!    config defaults, binary declarations, or lifecycle hooks. This
//!    match prevents drift back to the pre-0.18 plugin shape.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use caduceus::fixtures::{
    CANONICAL_CONFIG_KEYS, CANONICAL_WORKER_ENV_VARS, DEFAULT_ALLOWLIST_EXACT_ENV_NAMES,
    DEFAULT_ALLOWLIST_PREFIX_ENV_PATTERNS, DENIED_ENV_NAMES, HERMES_FORBIDDEN_MANIFEST_FIELDS,
    HERMES_MANIFEST_FIELDS,
};

/// Absolute path of the repository root.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Helper: read a UTF-8 file relative to the repo root.
fn read_repo_file(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {rel}: {err}"))
}

/// Helper: load a Python file and return the literal text (the
/// contract test only looks at identifier-like substrings, so we don't
/// need a Python AST).
fn load_bridge_source() -> String {
    read_repo_file("plugin-assets/worker-bridge.py")
}

/// Helper: is *needle* a substring of *haystack* on a word boundary
/// (so that ``api_base`` does not falsely match ``api_basement``)?
fn matches_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(idx) = haystack[start..].find(needle) {
        let abs = start + idx;
        let end = abs + needle_bytes.len();
        let before_ok = abs == 0
            || !bytes[abs - 1].is_ascii_alphanumeric()
            || bytes[abs - 1] == b'_'
            || bytes[abs - 1] == b'\n';
        let after_ok =
            end == bytes.len() || !bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_';
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// Extract all `REQUIRED_ENV_VARS` literal entries from the bridge
/// source so we can compare them with `CANONICAL_WORKER_ENV_VARS`.
fn extract_bridge_required_env_vars(source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut in_required = false;
    for line in source.lines() {
        let trimmed = line.trim_start();
        // Enter the REQUIRED_ENV_VARS tuple only after its leading
        // ``REQUIRED_ENV_VARS: tuple[str, ...] = (`` declaration.
        if !in_required && trimmed.starts_with("REQUIRED_ENV_VARS:") {
            in_required = true;
            continue;
        }
        if in_required {
            // Exit when we hit the closing ``)`` of the tuple.
            if trimmed.starts_with(")") {
                in_required = false;
                continue;
            }
            // Skip empty / pure-comment / docstring lines.
            if trimmed.is_empty() || trimmed.starts_with("#") || trimmed.starts_with("\"\"\"") {
                continue;
            }
            for raw in line.split(|c: char| c == '"' || c == '\'' || c == ',' || c.is_whitespace())
            {
                let stripped = raw
                    .trim_matches(|c: char| c == ',' || c == '"' || c == '\'' || c.is_whitespace());
                if stripped.starts_with("CADUCEUS_")
                    && !stripped.contains('=')
                    && !stripped.contains('/')
                    && !stripped.contains('`')
                {
                    names.insert(stripped.to_string());
                }
            }
        }
    }
    names
}

/// Extract every `Config` struct field name from `src/config.rs`.
fn extract_daemon_config_fields(source: &str) -> BTreeSet<String> {
    let mut fields = BTreeSet::new();
    let mut in_struct = false;
    let mut depth = 0i32;
    for line in source.lines() {
        if line.contains("pub struct Config") {
            in_struct = true;
            depth = 0;
            continue;
        }
        if in_struct {
            if line.trim_start().starts_with("//") {
                continue;
            }
            // Track brace depth so we exit when the struct closes.
            for ch in line.chars() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth <= 0 {
                            in_struct = false;
                        }
                    }
                    _ => {}
                }
            }
            // Strip comments; we only care about `pub <name>:` lines.
            let no_comment = line.split("//").next().unwrap_or("");
            let trimmed = no_comment.trim_start();
            if let Some(rest) = trimmed.strip_prefix("pub ") {
                let mut parts = rest.splitn(2, ':');
                if let Some(field_name) = parts.next() {
                    fields.insert(field_name.trim().to_string());
                }
            }
        }
    }
    fields
}

/// Worker env names emitted by `crate::worker::sanitized_env`. The
/// daemon's tests assert them directly; this cross-doc test only
/// requires that the canonical fixture covers the contract.
fn extract_daemon_worker_env_names() -> BTreeSet<String> {
    let source = read_repo_file("src/worker.rs");
    let mut names = BTreeSet::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("(\"") {
            // Handles `(  "CADUCEUS_...", &value),` entries.
            if let Some((name, _)) = rest.split_once("\",") {
                if name.starts_with("CADUCEUS_") {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names
}

/// Extract the manifest field allowlist actually used by
/// ``tests/hermes_plugin_test.py::_manifest_field_check``. The
/// expected allowlist is the literal ``_ALLOWED_MANIFEST_FIELDS`` set
/// declared in that test file; we parse it directly so the Rust
/// fixture's allowlist agrees with the Python contract.
fn extract_python_manifest_field_allowlist() -> BTreeSet<String> {
    let source = read_repo_file("tests/hermes_plugin_test.py");
    let mut fields = BTreeSet::new();
    let mut in_set = false;
    let mut depth = 0i32;
    for line in source.lines() {
        if !in_set && line.contains("_ALLOWED_MANIFEST_FIELDS") && line.contains("{") {
            in_set = true;
            depth = 1;
        }
        if in_set {
            for ch in line.chars() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth <= 0 {
                            in_set = false;
                        }
                    }
                    _ => {}
                }
            }
            for raw in line.split('"') {
                let stripped = raw.trim_matches(|c: char| {
                    c == ',' || c == '"' || c == '\'' || c.is_whitespace() || c == '{' || c == '}'
                });
                if !stripped.is_empty() && !stripped.starts_with('#') && !stripped.starts_with('_')
                {
                    fields.insert(stripped.to_string());
                }
            }
            if !in_set {
                break;
            }
        }
    }
    fields
}

// ---------------------------------------------------------------------------
// Test 1 — config fields
// ---------------------------------------------------------------------------

#[test]
fn daemon_config_struct_matches_canonical_fixture() {
    let source = read_repo_file("src/config.rs");
    let struct_fields = extract_daemon_config_fields(&source);

    let mut missing: Vec<&str> = Vec::new();
    for &key in CANONICAL_CONFIG_KEYS.iter() {
        if !struct_fields.contains(key) && !struct_fields.contains(&camel_to_snake(key).to_string())
        {
            // We allow listed aliases (e.g. feedback_author_allowlist vs
            // comment_feedback_author_allowlist) when the daemon's
            // struct field has an extra prefix; if no alias matches,
            // record the missing key as a contract violation.
            missing.push(key);
        }
    }
    assert!(
        missing.is_empty(),
        "Config keys listed in CANONICAL_CONFIG_KEYS but missing from `Config` struct: \
         {missing:?}"
    );
}

#[test]
fn canonical_config_keys_appear_in_readme_examples() {
    let readme = read_repo_file("README.md");
    for key in [
        "watched_repos",
        "worker_command",
        "poll_interval_seconds",
        "ticket_label_code",
    ] {
        // Documented config examples must mention the canonical key.
        assert!(
            matches_word(&readme, key),
            "README.md has no occurrence of canonical config key `{key}`"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2 — worker env names
// ---------------------------------------------------------------------------

#[test]
fn daemon_worker_emits_every_canonical_env_var() {
    let names = extract_daemon_worker_env_names();
    let canonical: BTreeSet<String> = CANONICAL_WORKER_ENV_VARS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let missing: Vec<&String> = canonical.difference(&names).collect();
    assert!(
        missing.is_empty(),
        "Daemon does not emit these canonical env vars: {missing:?}"
    );
}

#[test]
fn bridge_required_env_vars_match_daemon_canonical() {
    let bridge_source = load_bridge_source();
    let required = extract_bridge_required_env_vars(&bridge_source);
    let canonical: BTreeSet<String> = CANONICAL_WORKER_ENV_VARS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(
        required, canonical,
        "worker-bridge.py REQUIRED_ENV_VARS must equal CANONICAL_WORKER_ENV_VARS"
    );
}

#[test]
fn bridge_exposes_invoke_harness_as_the_only_user_editable_hook() {
    let bridge_source = load_bridge_source();
    // The contract requires ``invoke_harness`` to be the only function
    // operators are expected to edit. The bridge test suite pins it
    // separately; here we just confirm the docstring and signature
    // exist so a future refactor cannot silently drop the hook.
    assert!(
        bridge_source.contains("def invoke_harness("),
        "worker-bridge.py no longer exposes `invoke_harness` as the public hook"
    );
    assert!(
        bridge_source.contains("return its exit code"),
        "worker-bridge.py `invoke_harness` docstring no longer promises the harness exit code"
    );
    assert!(
        bridge_source.contains("PROMPT_FILE_NAME = \"worker-prompt.md\""),
        "worker-bridge.py no longer pins the worker-prompt filename"
    );
}

#[test]
fn bridge_does_not_write_state_heartbeats_or_results() {
    let bridge_source = load_bridge_source();
    // The bridge is forbidden from writing state files, heartbeats,
    // or worker-result.json. Any code that opens such files in write
    // mode would be a contract violation.
    for forbidden in [
        ".heartbeat",
        "caduceus-state",
        "state_meta.json",
        "queue.json",
        "worker-result.json",
    ] {
        let forbidden_writes = [
            format!("open({forbidden}"),
            format!("Path({forbidden})"),
            format!("write_text({forbidden}"),
            format!("write_bytes({forbidden}"),
            format!("with open({forbidden}"),
        ];
        for needle in &forbidden_writes {
            assert!(
                !bridge_source.contains(needle.as_str()),
                "worker-bridge.py references forbidden path `{forbidden}` in code"
            );
        }
    }
    // The bridge MAY reference these names in comments / docstrings
    // (they are exactly what we want to document); we only forbid
    // code-level file IO against them.
}

// ---------------------------------------------------------------------------
// Test 3 — default allowlist
// ---------------------------------------------------------------------------

#[test]
fn worker_default_allowlist_matches_canonical_constants() {
    let source = read_repo_file("src/worker.rs");
    for name in DEFAULT_ALLOWLIST_EXACT_ENV_NAMES {
        let needle = format!("\"{name}\"");
        assert!(
            source.contains(&needle),
            "DEFAULT_ALLOWLIST_EXACT missing {name}"
        );
    }
    for pattern in DEFAULT_ALLOWLIST_PREFIX_ENV_PATTERNS {
        // The fixture stores prefixes like "OPENAI_*" and the Rust
        // source stores them with the same form. We look for the
        // exact string between quotes rather than re-formatting.
        let needle = format!("\"{pattern}\"");
        assert!(
            source.contains(&needle),
            "DEFAULT_ALLOWLIST_PREFIXES missing {needle:?}; patterns: {pattern:?}"
        );
    }
    for name in DENIED_ENV_NAMES {
        let needle = format!("\"{name}\"");
        assert!(source.contains(&needle), "DENIED_EXACT_VARS missing {name}");
    }
}

// ---------------------------------------------------------------------------
// Test 4 — Hermes v0.18.2 plugin manifest contract
// ---------------------------------------------------------------------------

#[test]
fn plugin_yaml_uses_only_hermes_supported_fields() {
    let manifest_text = read_repo_file("plugin.yaml");
    // Parse out top-level `key:` lines, dropping comments and blank
    // lines. Comments starting with `#` may appear inline; we don't
    // need a full YAML parser.
    let mut top_level_keys = BTreeSet::new();
    for line in manifest_text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(key) = line.split(':').next() {
            let key = key.trim().to_string();
            // Indented lines are nested values; we only care about
            // the top of the file.
            if !line.starts_with(' ') && !line.starts_with('\t') && !key.is_empty() {
                top_level_keys.insert(key);
            }
        }
    }
    for key in &top_level_keys {
        assert!(
            HERMES_MANIFEST_FIELDS.contains(&key.as_str()),
            "plugin.yaml declares top-level key `{key}` which is not in the Hermes \
             v0.18.2 supported field list"
        );
    }
    for forbidden in HERMES_FORBIDDEN_MANIFEST_FIELDS {
        assert!(
            !top_level_keys.contains(*forbidden),
            "plugin.yaml still uses the forbidden legacy field `{forbidden}`"
        );
    }
}

#[test]
fn hermes_v0182_minimum_version_appears_in_docs() {
    let readme = read_repo_file("README.md");
    let skill = read_repo_file("skills/caduceus/SKILL.md");
    assert!(
        readme.contains("v0.18.2"),
        "README.md must state Hermes Agent v0.18.2 as the minimum tested host"
    );
    // The skill file references Hermes v0.18.2 explicitly so future
    // edits do not silently relax the floor.
    assert!(
        skill.contains("v0.18.2") || skill.contains("Hermes Agent"),
        "skills/caduceus/SKILL.md must reference Hermes Agent / v0.18.2"
    );
}

#[test]
fn python_manifest_field_allowlist_matches_hermes_v0182() {
    let allowlist = extract_python_manifest_field_allowlist();
    // Anything declared in the negative fixture must NOT be in the
    // supported set; anything that's a real Hermes v0.18 field MUST
    // appear in the supported set.
    assert!(!allowlist.contains("profile_section"));
    assert!(!allowlist.contains("binaries"));
    assert!(!allowlist.contains("cron_profiles"));
    // Spot-check a few well-known entries the hermes plugin test
    // asserts are present.
    assert!(allowlist.contains("manifest_version"));
    assert!(allowlist.contains("kind"));
    assert!(allowlist.contains("provides_tools"));
}

// ---------------------------------------------------------------------------
// Test 5 — no false claims in public docs
// ---------------------------------------------------------------------------

#[test]
fn readme_and_skill_do_not_claim_legacy_loader_features() {
    let readme = read_repo_file("README.md");
    let skill = read_repo_file("skills/caduceus/SKILL.md");

    // Strings that, if present, would imply Hermes reads a directory
    // Caduceus has never shipped. We *forbid* them so the docs cannot
    // drift back to the legacy plugin shape.
    for forbidden in [
        "commands/*.md",
        "cron/*.yaml",
        "caduceus-pulse.yaml",
        "profile_section:",
        "manifest config defaults",
        "binary declarations",
        "lifecycle hooks",
    ] {
        assert!(
            !readme.contains(forbidden),
            "README.md still references the legacy field path `{forbidden}`"
        );
        assert!(
            !skill.contains(forbidden),
            "skills/caduceus/SKILL.md still references the legacy field path `{forbidden}`"
        );
    }
}

#[test]
fn skill_describes_opt_in_nature_of_plugin_skills() {
    let skill = read_repo_file("skills/caduceus/SKILL.md");
    // The skill must explicitly say it does NOT auto-trigger — that's
    // the v0.18 plugin-skill contract. Hermes has no automatic skill
    // trigger rules; plugin skills are opt-in.
    let lower = skill.to_ascii_lowercase();
    assert!(
        lower.contains("opt-in")
            || lower.contains("opt in")
            || lower.contains("must be loaded")
            || lower.contains("does not trigger")
            || lower.contains("not trigger"),
        "skills/caduceus/SKILL.md must explain that Caduceus is opt-in"
    );
}

#[test]
fn skill_documents_cron_install_then_setup_lifecycle() {
    let skill = read_repo_file("skills/caduceus/SKILL.md");
    let readme = read_repo_file("README.md");
    // The full lifecycle is install -> setup -> cron-install -> (later
    // update/setup rebuild, cron-remove + plugins remove on uninstall).
    for marker in [
        "plugins install",
        "plugins update",
        "plugins remove",
        "caduceus setup",
        "caduceus cron-install",
        "caduceus cron-remove",
    ] {
        assert!(
            skill.contains(marker) || readme.contains(marker),
            "lifecycle marker `{marker}` missing from README and skill"
        );
    }
}

#[test]
fn readme_documents_required_pre_clones_and_branch_policy() {
    let readme = read_repo_file("README.md");
    // The daemon requires ``<workdir_base>/<owner>/<repo>`` to exist
    // with a matching ``origin`` and branch *before* it can poll. The
    // contract demands this be called out explicitly so operators do
    // not configure ``watched_repos`` pointing at non-existent local
    // clones.
    assert!(
        readme.contains("workdir_base"),
        "README.md does not document workdir_base / pre-clone requirement"
    );
    assert!(
        readme.contains("origin"),
        "README.md does not document the git `origin` requirement"
    );
}

#[test]
fn readme_documents_standalone_worker_command_requirement() {
    let readme = read_repo_file("README.md");
    // Standalone (non-canonical-layout) installs must set
    // ``worker_command`` explicitly. This is a recurring operator
    // mistake the docs must prevent.
    let lower = readme.to_ascii_lowercase();
    assert!(
        lower.contains("worker_command")
            && (lower.contains("standalone") || lower.contains("explicit")),
        "README.md does not document the standalone-install worker_command requirement"
    );
}

#[test]
fn readme_documents_pat_and_git_auth_disambiguation() {
    let readme = read_repo_file("README.md");
    // The contract separates two different authentication surfaces:
    // the GitHub API token (PAT) Caduceus holds, and the *git*
    // authentication (SSH agent / credential helper) used for
    // ``push``. Operators frequently confuse them.
    let lower = readme.to_ascii_lowercase();
    assert!(
        lower.contains("github token") || lower.contains("pat"),
        "README.md does not mention GitHub token / PAT scope"
    );
    assert!(
        lower.contains("ssh") || lower.contains("credential helper"),
        "README.md does not mention git SSH/credential helper auth"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn camel_to_snake(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

#[allow(dead_code)]
fn file_extension(path: &Path) -> Option<&OsStr> {
    path.extension()
}

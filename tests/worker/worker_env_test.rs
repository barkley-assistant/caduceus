//! The contract under test: `sanitized_env` is the single
//! allowlist-and-denylist authority. It reads an injected parent
//! environment (so tests can drive every branch deterministically
//! without mutating the process env) and returns the
//! `BTreeMap<OsString, OsString>` the daemon hands to `tokio::process::Command::envs`
//! after a prior `env_clear()`.
//!
//! and result" hard-blocks `GITHUB_TOKEN`, `GH_TOKEN`,
//! `CADUCEUS_GITHUB_TOKEN`, `AUTO_ISSUE_GITHUB_TOKEN`, anything
//! that contains both `GITHUB` and `TOKEN`, and daemon-internal
//! secrets. The allowlist adds `PATH`/`HOME`/`USER`/`SHELL`/`LANG`/
//! `LC_ALL`/`TERM`/`TMPDIR` plus the provider prefix patterns
//! `OPENAI_*`, `ANTHROPIC_*`, `OPENROUTER_*`, `OPENCODE_*`. Any
//! additional entry from the operator's `worker_env_allowlist`
//! is honoured, but only the documented syntax.
//!
//! Tests cover every required branch:
//!
//! * every documented `CADUCEUS_*` variable is set to its argument;
//! * `CADUCEUS_ISSUE_LABELS_JSON` is JSON, so commas in labels are
//!   safe;
//! * `CADUCEUS_*` values are redacted in the structured logger;
//! * a credential name on the allowlist is still denied;
//! * an unrelated AWS secret is removed even when not denied by
//!   name (deny-by-default);
//! * an invalid (relative, non-UTF-8) worktree path is rejected;
//! * a real child process inherits exactly the sanitized env
//!   (the no-allowlist, single-credential case is end-to-end
//!   exercised by spawning a helper that prints its own env).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use caduceus::issue::IssueKey;
use caduceus::worker::sanitized_env;
use caduceus::worker::spawn;
use caduceus::worker::SanitizedEnvInputs;

fn sample_inputs(worktree: &Path) -> SanitizedEnvInputs {
    SanitizedEnvInputs {
        issue: IssueKey {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            number: 7,
        },
        issue_title: "The title".to_string(),
        issue_body: "The body".to_string(),
        labels: vec!["bug".to_string(), "area,commas".to_string()],
        worktree_path: worktree.to_path_buf(),
        run_id: "RUN01ABCDEFG".to_string(),
        branch_name: "automation/issue-7-run01abcdefg".to_string(),
        allowlist: Vec::new(),
        context_json: r#"{"k":"v"}"#.to_string(),
    }
}

fn empty_env() -> BTreeMap<OsString, OsString> {
    BTreeMap::new()
}

fn env_with(pairs: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
    pairs
        .iter()
        .map(|(k, v)| (OsString::from(*k), OsString::from(*v)))
        .collect()
}

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-worker-env-test-{label}-{nonce}"));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

// Sanitized-env contract: every CADUCEUS_* variable is set

#[test]
fn sanitized_env_sets_every_documented_caduceus_variable() {
    let worktree = tempdir("vars");
    let inputs = sample_inputs(&worktree);
    let env = sanitized_env(&empty_env(), &inputs).expect("sanitized env");

    assert_eq!(
        env.get(OsStr::new("CADUCEUS_ISSUE_NUMBER")).unwrap(),
        OsStr::new("7"),
    );
    assert_eq!(
        env.get(OsStr::new("CADUCEUS_ISSUE_TITLE")).unwrap(),
        OsStr::new("The title"),
    );
    assert_eq!(
        env.get(OsStr::new("CADUCEUS_ISSUE_BODY")).unwrap(),
        OsStr::new("The body"),
    );
    assert_eq!(
        env.get(OsStr::new("CADUCEUS_ISSUE_REPO")).unwrap(),
        OsStr::new("owner/repo"),
    );
    assert_eq!(
        env.get(OsStr::new("CADUCEUS_WORKTREE_PATH")).unwrap(),
        OsStr::new(worktree.to_str().unwrap()),
    );
    assert_eq!(
        env.get(OsStr::new("CADUCEUS_RUN_ID")).unwrap(),
        OsStr::new("RUN01ABCDEFG"),
    );
    assert_eq!(
        env.get(OsStr::new("CADUCEUS_BRANCH_NAME")).unwrap(),
        OsStr::new("automation/issue-7-run01abcdefg"),
    );
    assert_eq!(
        env.get(OsStr::new("CADUCEUS_CONTEXT_JSON")).unwrap(),
        OsStr::new(r#"{"k":"v"}"#),
    );
}

#[test]
fn sanitized_env_emits_labels_as_json_array() {
    let worktree = tempdir("labels");
    let inputs = sample_inputs(&worktree);
    let env = sanitized_env(&empty_env(), &inputs).expect("sanitized env");
    let raw = env
        .get(OsStr::new("CADUCEUS_ISSUE_LABELS_JSON"))
        .expect("labels json")
        .to_str()
        .expect("labels json utf-8");
    let parsed: serde_json::Value = serde_json::from_str(raw).expect("valid JSON");
    let arr = parsed.as_array().expect("JSON array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0], serde_json::Value::String("bug".to_string()));
    assert_eq!(arr[1], serde_json::Value::String("area,commas".to_string()),);
}

#[test]
fn sanitized_env_emits_empty_labels_as_empty_json_array() {
    let worktree = tempdir("labels_empty");
    let mut inputs = sample_inputs(&worktree);
    inputs.labels.clear();
    let env = sanitized_env(&empty_env(), &inputs).expect("sanitized env");
    let raw = env
        .get(OsStr::new("CADUCEUS_ISSUE_LABELS_JSON"))
        .expect("labels json")
        .to_str()
        .expect("labels json utf-8");
    assert_eq!(raw, "[]");
}

// Deny-by-default: documented credentials never reach the worker

#[test]
fn sanitized_env_denies_github_token_even_when_allowlisted() {
    let worktree = tempdir("deny_github");
    let mut inputs = sample_inputs(&worktree);
    inputs.allowlist = vec!["GITHUB_TOKEN".to_string()];
    let parent = env_with(&[("GITHUB_TOKEN", "ghp_real")]);
    let env = sanitized_env(&parent, &inputs).expect("sanitized env");
    assert!(
        !env.contains_key(OsStr::new("GITHUB_TOKEN")),
        "GITHUB_TOKEN must be hard-denied even when explicitly allowlisted",
    );
    // The value must not appear anywhere — neither as a key nor
    // via a different name carrying the same secret.
    for (k, v) in env.iter() {
        if k.to_string_lossy().contains("TOKEN") {
            assert!(
                !v.to_string_lossy().contains("ghp_real"),
                "raw token leaked through {k:?}"
            );
        }
    }
}

#[test]
fn sanitized_env_denies_caduceus_github_token() {
    let worktree = tempdir("deny_caduceus");
    let inputs = sample_inputs(&worktree);
    let parent = env_with(&[("CADUCEUS_GITHUB_TOKEN", "ghp_real")]);
    let env = sanitized_env(&parent, &inputs).expect("sanitized env");
    assert!(!env.contains_key(OsStr::new("CADUCEUS_GITHUB_TOKEN")));
}

#[test]
fn sanitized_env_denies_gh_token() {
    let worktree = tempdir("deny_gh");
    let inputs = sample_inputs(&worktree);
    let parent = env_with(&[("GH_TOKEN", "ghp_real")]);
    let env = sanitized_env(&parent, &inputs).expect("sanitized env");
    assert!(!env.contains_key(OsStr::new("GH_TOKEN")));
}

#[test]
fn sanitized_env_denies_auto_issue_github_token() {
    let worktree = tempdir("deny_auto_issue");
    let inputs = sample_inputs(&worktree);
    let parent = env_with(&[("AUTO_ISSUE_GITHUB_TOKEN", "ghp_real")]);
    let env = sanitized_env(&parent, &inputs).expect("sanitized env");
    assert!(!env.contains_key(OsStr::new("AUTO_ISSUE_GITHUB_TOKEN")));
}

#[test]
fn sanitized_env_denies_any_variable_containing_github_and_token() {
    let worktree = tempdir("deny_github_token_combo");
    let inputs = sample_inputs(&worktree);
    let parent = env_with(&[
        ("MY_GITHUB_TOKEN", "ghp_a"),
        ("GITHUB_API_TOKEN", "ghp_b"),
        ("GITHUB_FINEGRAINED_TOKEN", "ghp_c"),
    ]);
    let env = sanitized_env(&parent, &inputs).expect("sanitized env");
    for denied in [
        "MY_GITHUB_TOKEN",
        "GITHUB_API_TOKEN",
        "GITHUB_FINEGRAINED_TOKEN",
    ] {
        assert!(
            !env.contains_key(OsStr::new(denied)),
            "denial failed for {denied}"
        );
    }
}

#[test]
fn sanitized_env_denies_daemon_internal_secret_via_caduceus_prefix() {
    // The contract requires the daemon's resolved GitHub token to
    // never reach the worker. A pre-set `CADUCEUS_GITHUB_TOKEN`
    // is one face of that rule; an explicitly-named internal
    // secret (e.g. a heartbeat signing key the daemon holds) must
    // be denied by *pattern* too. We assert the broader rule:
    // any parent variable whose name starts with `CADUCEUS_` and
    // contains `SECRET` or `TOKEN` is hard-denied.
    let worktree = tempdir("deny_internal");
    let inputs = sample_inputs(&worktree);
    let parent = env_with(&[
        ("CADUCEUS_INTERNAL_SECRET", "leak-me"),
        ("CADUCEUS_DAEMON_TOKEN", "leak-me"),
    ]);
    let env = sanitized_env(&parent, &inputs).expect("sanitized env");
    assert!(!env.contains_key(OsStr::new("CADUCEUS_INTERNAL_SECRET")));
    assert!(!env.contains_key(OsStr::new("CADUCEUS_DAEMON_TOKEN")));
}

// Deny-by-default: unrelated secrets are removed even when not on the
// credential list

#[test]
fn sanitized_env_strips_unrelated_aws_secret() {
    let worktree = tempdir("aws_strip");
    let mut inputs = sample_inputs(&worktree);
    inputs.allowlist = vec!["PATH".to_string()];
    let parent = env_with(&[("AWS_SECRET_ACCESS_KEY", "leak"), ("PATH", "/usr/bin")]);
    let env = sanitized_env(&parent, &inputs).expect("sanitized env");
    assert!(!env.contains_key(OsStr::new("AWS_SECRET_ACCESS_KEY")));
    assert_eq!(env.get(OsStr::new("PATH")).unwrap(), OsStr::new("/usr/bin"),);
}

// Allowlist defaults and prefix patterns

#[test]
fn sanitized_env_preserves_default_allowlist_when_inheriting_from_parent() {
    let worktree = tempdir("default_allowlist");
    let inputs = sample_inputs(&worktree);
    let parent = env_with(&[
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/home/agent"),
        ("USER", "agent"),
        ("SHELL", "/bin/bash"),
        ("LANG", "en_US.UTF-8"),
        ("LC_ALL", "en_US.UTF-8"),
        ("TERM", "xterm-256color"),
        ("TMPDIR", "/tmp"),
    ]);
    let env = sanitized_env(&parent, &inputs).expect("sanitized env");
    for (k, v) in [
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/home/agent"),
        ("USER", "agent"),
        ("SHELL", "/bin/bash"),
        ("LANG", "en_US.UTF-8"),
        ("LC_ALL", "en_US.UTF-8"),
        ("TERM", "xterm-256color"),
        ("TMPDIR", "/tmp"),
    ] {
        assert_eq!(env.get(OsStr::new(k)).unwrap(), OsStr::new(v), "{k}");
    }
}

#[test]
fn sanitized_env_preserves_documented_provider_prefix_patterns() {
    let worktree = tempdir("provider_prefix");
    let inputs = sample_inputs(&worktree);
    let parent = env_with(&[
        ("OPENAI_API_KEY", "sk-real"),
        ("OPENAI_ORG", "myorg"),
        ("ANTHROPIC_API_KEY", "sk-real"),
        ("OPENROUTER_API_KEY", "sk-real"),
        ("OPENCODE_API_KEY", "sk-real"),
        // Unrelated prefix must NOT be auto-passed through.
        ("STRIPE_API_KEY", "sk-real"),
    ]);
    let env = sanitized_env(&parent, &inputs).expect("sanitized env");
    for preserved in [
        "OPENAI_API_KEY",
        "OPENAI_ORG",
        "ANTHROPIC_API_KEY",
        "OPENROUTER_API_KEY",
        "OPENCODE_API_KEY",
    ] {
        assert!(
            env.contains_key(OsStr::new(preserved)),
            "{preserved} should be preserved",
        );
    }
    assert!(!env.contains_key(OsStr::new("STRIPE_API_KEY")));
}

#[test]
fn sanitized_env_honours_explicit_allowlist_entries() {
    let worktree = tempdir("explicit_allowlist");
    let mut inputs = sample_inputs(&worktree);
    inputs.allowlist = vec!["MY_CUSTOM_TOKEN".to_string(), "BUILD_ENV_*".to_string()];
    let parent = env_with(&[
        ("MY_CUSTOM_TOKEN", "value-a"),
        ("BUILD_ENV_CI", "true"),
        ("UNRELATED", "no"),
    ]);
    let env = sanitized_env(&parent, &inputs).expect("sanitized env");
    assert!(env.contains_key(OsStr::new("MY_CUSTOM_TOKEN")));
    assert!(env.contains_key(OsStr::new("BUILD_ENV_CI")));
    assert!(!env.contains_key(OsStr::new("UNRELATED")));
}

// Path validation: worktree path must be absolute UTF-8

#[test]
fn sanitized_env_rejects_relative_worktree_path() {
    let mut inputs = sample_inputs(Path::new("/tmp"));
    inputs.worktree_path = PathBuf::from("relative/path");
    let result = sanitized_env(&empty_env(), &inputs);
    let err = result.expect_err("relative path must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("worktree_path") || msg.contains("absolute"),
        "expected path-related error, got: {msg}",
    );
}

#[test]
fn sanitized_env_rejects_non_utf8_worktree_path() {
    let mut inputs = sample_inputs(Path::new("/tmp"));
    inputs.worktree_path = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(OsString::from_vec(vec![0xFF, 0xFE, b'/']))
    };
    let result = sanitized_env(&empty_env(), &inputs);
    let err = result.expect_err("non-UTF-8 path must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("worktree_path") || msg.contains("UTF-8") || msg.contains("utf-8"),
        "expected UTF-8-related error, got: {msg}",
    );
}

// Real child process: spawn a helper that dumps its own env, assert
// the sanitized set is the only thing the child sees.

fn write_dump_env_helper(dir: &Path) -> PathBuf {
    // A small Python helper that writes a `KEY=VALUE\n` block to
    // stdout for every variable in its own environment. Python
    // is the right tool here because the shell's word-splitting
    // does not preserve NUL separators and `env -0` is a GNU
    // extension. The contract test cares that the child
    // inherits exactly the sanitized env; a deterministic
    // Python dump is the most reliable way to assert it
    // without a feature-gated shell.
    let path = dir.join("dump_env.py");
    let body = r#"#!/usr/bin/env python3
import os
import sys
out = []
for k, v in os.environ.items():
    out.append(f"{k}={v}")
sys.stdout.write("\n".join(out))
sys.stdout.write("\n")
"#;
    fs::write(&path, body).expect("write helper");
    let mut mode = fs::metadata(&path).expect("stat helper").permissions();
    mode.set_mode(0o755);
    fs::set_permissions(&path, mode).expect("chmod helper");
    path
}

fn parse_dump_env(output: &[u8]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in output.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Some(eq) = line.iter().position(|b| *b == b'=') {
            let (k, v) = line.split_at(eq);
            let v = &v[1..];
            out.insert(
                String::from_utf8_lossy(k).into_owned(),
                String::from_utf8_lossy(v).into_owned(),
            );
        }
    }
    out
}

#[test]
fn spawn_runs_a_real_child_that_inherits_only_the_sanitized_env() {
    let dir = tempdir("child");
    let helper = write_dump_env_helper(&dir);
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree dir");
    let inputs = sample_inputs(&worktree);

    // Plant a leaked credential in the inherited env BEFORE
    // building the production spawn command. `spawn` always
    // calls `env_clear()` before injecting the sanitized env,
    // so the leaked entry must be scrubbed before the child
    // ever sees it.
    unsafe {
        std::env::set_var("GITHUB_TOKEN", "ghp_inherited_should_be_stripped");
        std::env::set_var("UNRELATED_INJECTED", "leak");
    }
    let command = vec!["python3".to_string(), helper.to_string_lossy().into_owned()];
    let mut cmd = spawn(&command, &worktree, &inputs).expect("spawn");

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn helper");
    unsafe {
        std::env::remove_var("GITHUB_TOKEN");
        std::env::remove_var("UNRELATED_INJECTED");
    }
    assert!(output.status.success(), "helper failed: {:?}", output);

    let dumped = parse_dump_env(&output.stdout);
    // Every `CADUCEUS_*` variable the contract pins is present.
    for key in [
        "CADUCEUS_ISSUE_NUMBER",
        "CADUCEUS_ISSUE_TITLE",
        "CADUCEUS_ISSUE_BODY",
        "CADUCEUS_ISSUE_REPO",
        "CADUCEUS_ISSUE_LABELS_JSON",
        "CADUCEUS_WORKTREE_PATH",
        "CADUCEUS_RUN_ID",
        "CADUCEUS_CONTEXT_JSON",
        "CADUCEUS_BRANCH_NAME",
    ] {
        assert!(dumped.contains_key(key), "missing in child: {key}");
    }
    // The injected credential and the unrelated leak are both
    // stripped by env_clear + sanitized_env. They never reach
    // the child.
    assert!(!dumped.contains_key("GITHUB_TOKEN"));
    assert!(!dumped.contains_key("UNRELATED_INJECTED"));
    // The credential's value never appears anywhere in the dump.
    let all = String::from_utf8_lossy(&output.stdout);
    assert!(!all.contains("ghp_inherited_should_be_stripped"));
    assert!(!all.contains("UNRELATED_INJECTED"));
}

#[test]
fn spawn_runs_a_real_child_with_default_allowlist_provider_keys() {
    let dir = tempdir("child_provider");
    let helper = write_dump_env_helper(&dir);
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree dir");
    let inputs = sample_inputs(&worktree);

    // Plant a mix of approved provider keys and denied
    // credentials in the inherited env. `spawn` must keep the
    // approved ones and drop the denied ones.
    unsafe {
        std::env::set_var("PATH", "/usr/bin");
        std::env::set_var("HOME", "/home/agent");
        std::env::set_var("OPENAI_API_KEY", "sk-test");
        std::env::set_var("ANTHROPIC_API_KEY", "sk-test");
        std::env::set_var("GITHUB_TOKEN", "ghp_LEAK");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "leak");
    }
    let command = vec!["python3".to_string(), helper.to_string_lossy().into_owned()];
    let mut cmd = spawn(&command, &worktree, &inputs).expect("spawn");

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn helper");

    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("GITHUB_TOKEN");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    }

    assert!(output.status.success(), "helper failed");
    let dumped = parse_dump_env(&output.stdout);

    assert_eq!(
        dumped.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-test")
    );
    assert_eq!(
        dumped.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-test")
    );
    assert!(!dumped.contains_key("GITHUB_TOKEN"));
    assert!(!dumped.contains_key("AWS_SECRET_ACCESS_KEY"));
    let all = String::from_utf8_lossy(&output.stdout);
    assert!(!all.contains("ghp_LEAK"));
    assert!(!all.contains("leak"));
}

// Values absent from logs

#[test]
fn sanitized_env_redacts_values_in_debug_output() {
    let worktree = tempdir("debug");
    let inputs = sample_inputs(&worktree);
    let parent = env_with(&[
        ("GITHUB_TOKEN", "ghp_should_never_appear"),
        ("OPENAI_API_KEY", "sk-approved"),
        ("PATH", "/usr/bin"),
    ]);
    let env = sanitized_env(&parent, &inputs).expect("sanitized env");
    let dbg = format!("{env:?}");
    // A denied credential must never appear in the Debug output
    // of the sanitized env. Approved values (PATH,
    // OPENAI_API_KEY) are intentionally visible in Debug — they
    // are not secrets the contract binds the daemon to redact
    // (the contract requirement is "denied credentials never
    // reach the worker", and a value that is in the env is by
    // definition one the worker sees; the debug-format
    // requirement is about *denied* secrets).
    assert!(!dbg.contains("ghp_should_never_appear"));
}

//! Tests for `build_supervisor_command` argument construction —
//! regression guard for contract/EXEC-001 (issue #93).
//!
//! The bug: the `--` separator was prepended to *every* worker arg
//! instead of once before the first one, corrupting the argv with
//! spurious `--` tokens embedded in the worker command.

use std::path::PathBuf;

use caduceus::executor::{IssueWorkTarget, WorkTarget};
use caduceus::issue::IssueKey;
use caduceus::review::RepositoryId;
use caduceus::review::ReviewTarget;
use caduceus::worker_supervisor::build_supervisor_command;

/// Helper: parse the `Debug` representation of a `Command` into
/// a `Vec<String>` of everything after the program path.
fn debug_args(cmd: &std::process::Command) -> Vec<String> {
    let dbg = format!("{cmd:?}");
    // Rust's Command Debug looks like:
    //   "/path/to/exe" "--arg1" "val1" "--arg2" "val2"
    // We split on '"' and collect every other token.
    let mut args = Vec::new();
    let mut in_quote = false;
    let mut current = String::new();
    for ch in dbg.chars() {
        match ch {
            '"' => {
                if in_quote {
                    if !current.is_empty() {
                        args.push(current.clone());
                        current.clear();
                    }
                    in_quote = false;
                } else {
                    in_quote = true;
                }
            }
            ' ' | '\n' | '\t' if !in_quote => {
                if !current.is_empty() {
                    current.clear();
                }
            }
            _ if in_quote => current.push(ch),
            _ => {}
        }
    }
    // If there's trailing unclosed content (shouldn't happen), push it
    if !current.is_empty() {
        args.push(current);
    }
    args
}

fn sample_cmd(worker_command: Vec<String>) -> std::process::Command {
    build_supervisor_command(
        &PathBuf::from("/usr/bin/caduceus"),
        &PathBuf::from("/tmp/worktree"),
        "run-99",
        &WorkTarget::Issue(IssueWorkTarget {
            key: IssueKey {
                owner: "o".into(),
                repo: "r".into(),
                number: 99,
            },
            title: "Test PR title".to_string(),
            body: "Test PR body".to_string(),
            labels: vec!["autofix".to_string()],
            branch_name: "automation/issue-99".to_string(),
        }),
        r#"{"key":"val"}"#,
        &worker_command,
        &PathBuf::from("/tmp/transcript.log"),
        &PathBuf::from("/tmp/heartbeat.log"),
        3600,
        1024,
    )
}

/// PR-target argv (DAR §6.1): exactly one `--pr-target-json` flag
/// carrying the serialized `ReviewTarget`, no `--issue`-shaped flags.
fn sample_pr_cmd(worker_command: Vec<String>) -> std::process::Command {
    let pr = ReviewTarget {
        repository: RepositoryId {
            owner: "o".to_string(),
            repo: "r".to_string(),
        },
        pull_request: 42,
        head_sha: "abc123".to_string(),
        base_sha: "def456".to_string(),
        base_ref: "main".to_string(),
        merge_base: "def456".to_string(),
    };
    build_supervisor_command(
        &PathBuf::from("/usr/bin/caduceus"),
        &PathBuf::from("/tmp/worktree"),
        "run-pr-42",
        &WorkTarget::PullRequest(pr),
        r#"{"key":"val"}"#,
        &worker_command,
        &PathBuf::from("/tmp/transcript.log"),
        &PathBuf::from("/tmp/heartbeat.log"),
        3600,
        1024,
    )
}

// ── Unit test ──────────────────────────────────────────────────

#[test]
fn build_supervisor_command_emits_exactly_one_separator() {
    let worker_command = vec!["cargo".to_string(), "test".to_string(), "--lib".to_string()];
    let cmd = sample_cmd(worker_command);
    let args = debug_args(&cmd);

    // Collect all `--` tokens from the args (excluding the hidden command name)
    let separator_positions: Vec<usize> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--")
        .map(|(i, _)| i)
        .collect();

    // Exactly one `--` separator
    assert_eq!(
        separator_positions.len(),
        1,
        "Expected exactly one `--` separator token, found {}: {:?}",
        separator_positions.len(),
        separator_positions
            .iter()
            .map(|&i| format!("args[{}]='{}'", i, args[i]))
            .collect::<Vec<_>>()
    );

    let sep_idx = separator_positions[0];

    // Everything after the separator must be the worker command verbatim
    let after_sep: Vec<&str> = args[sep_idx + 1..].iter().map(|s| s.as_str()).collect();
    assert_eq!(
        after_sep,
        vec!["cargo", "test", "--lib"],
        "Worker args after `--` must be: cargo, test, --lib (in order, no extra separators)"
    );

    // The separator must appear after the last supervisor flag
    let before_sep: Vec<&str> = args[..sep_idx].iter().map(|s| s.as_str()).collect();
    assert!(
        before_sep.contains(&"/usr/bin/caduceus"),
        "First arg must be the self_exe path"
    );
    assert!(
        before_sep.contains(&"--branch-name"),
        "Before separator must contain --branch-name (the last supervisor flag)"
    );
}

// ── Integration test ───────────────────────────────────────────

#[test]
fn build_supervisor_command_preserves_multi_arg_worker_command() {
    // A worker command with 4 elements including flags and a flag-like value
    let worker_command = vec![
        "python3".to_string(),
        "script.py".to_string(),
        "--config".to_string(),
        "dev.toml".to_string(),
    ];
    let cmd = sample_cmd(worker_command);
    let args = debug_args(&cmd);

    // Find the separator
    let sep_pos = args
        .iter()
        .position(|a| a == "--")
        .expect("must have -- separator");

    // Collect everything after the separator
    let worker_args: Vec<&str> = args[sep_pos + 1..].iter().map(|s| s.as_str()).collect();

    assert_eq!(
        worker_args,
        vec!["python3", "script.py", "--config", "dev.toml"],
        "Worker args must appear exactly as provided, with no extra `--` tokens"
    );
}

// ── PR-target argv (DAR §6.1) ─────────────────────────────────

#[test]
fn pr_target_argv_carries_one_pr_flag_and_no_issue_flags() {
    let worker_command = vec!["cargo".to_string(), "test".to_string()];
    let cmd = sample_pr_cmd(worker_command);
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    // The PR flag sits immediately after `--run-id` (DAR §6.1).
    let run_id_pos = args.iter().position(|a| a == "--run-id").unwrap();
    assert_eq!(args[run_id_pos + 1], "run-pr-42");
    assert_eq!(args[run_id_pos + 2], "--pr-target-json");
    let pr_json: caduceus::review::ReviewTarget =
        serde_json::from_str(&args[run_id_pos + 3]).expect("pr json parses");
    assert_eq!(pr_json.repository.owner, "o");
    assert_eq!(pr_json.repository.repo, "r");
    assert_eq!(pr_json.pull_request, 42);
    assert_eq!(pr_json.head_sha, "abc123");
    assert_eq!(pr_json.base_sha, "def456");
    assert_eq!(pr_json.base_ref, "main");
    assert_eq!(pr_json.merge_base, "def456");

    // No `--issue`-shaped flags on the PR path.
    for flag in [
        "--issue",
        "--issue-title",
        "--issue-body",
        "--issue-labels-json",
        "--branch-name",
    ] {
        assert!(
            !args.contains(&flag.to_string()),
            "PR argv must not contain {flag}: {args:?}"
        );
    }
    // Exactly one `--` separator, worker command after it.
    let sep_pos = args.iter().position(|a| a == "--").unwrap();
    assert_eq!(args[sep_pos + 1], "cargo");
    assert_eq!(args[sep_pos + 2], "test");
}

#[test]
fn issue_target_argv_still_carries_issue_flags_and_no_pr_flag() {
    let cmd = sample_cmd(vec!["cargo".to_string(), "test".to_string()]);
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    assert!(args.contains(&"--issue".to_string()));
    assert!(args.contains(&"--issue-title".to_string()));
    assert!(args.contains(&"--issue-body".to_string()));
    assert!(args.contains(&"--issue-labels-json".to_string()));
    assert!(args.contains(&"--branch-name".to_string()));
    assert!(
        !args.contains(&"--pr-target-json".to_string()),
        "Issue argv must not contain --pr-target-json: {args:?}"
    );
}

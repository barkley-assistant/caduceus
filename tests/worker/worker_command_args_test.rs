//! Tests for `build_supervisor_command` argument construction —
//! regression guard for contract/EXEC-001 (issue #93).
//!
//! The bug: the `--` separator was prepended to *every* worker arg
//! instead of once before the first one, corrupting the argv with
//! spurious `--` tokens embedded in the worker command.

use std::path::PathBuf;

use caduceus::issue::IssueKey;
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
        &IssueKey {
            owner: "o".into(),
            repo: "r".into(),
            number: 99,
        },
        r#"{"key":"val"}"#,
        &worker_command,
        &PathBuf::from("/tmp/transcript.log"),
        &PathBuf::from("/tmp/heartbeat.log"),
        3600,
        1024,
        "Test PR title",
        "Test PR body",
        &["autofix".to_string()],
        "automation/issue-99",
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

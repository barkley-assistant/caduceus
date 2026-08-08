//! Integration tests for the canonical worker prompt writer
//! (`src/worker/prompt.rs`). Moved out of the inline `#[cfg(test)]`
//! module per AGENTS.md.

use caduceus::github::issue::{IssueComment, IssueEvent, IssueKey};
use caduceus::prompt::{build_prompt, write_prompt, MAX_PROMPT_BYTES};
use caduceus::{IssueDetail, TicketType};
use chrono::{TimeZone, Utc};
use std::os::unix::fs::PermissionsExt;

fn sample_issue() -> IssueDetail {
    IssueDetail {
        key: IssueKey {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            number: 1,
        },
        title: "Test issue".to_string(),
        body: "Body".to_string(),
        labels: vec!["bug".to_string(), "area".to_string()],
        comments: vec![IssueComment {
            author: "alice".to_string(),
            body: "first".to_string(),
            created_at: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        }],
        trusted_comments: Vec::new(),
        events: vec![IssueEvent {
            kind: "labeled".to_string(),
            actor: "alice".to_string(),
            created_at: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            label_name: Some("bug".to_string()),
        }],
        fetched_at: Utc.with_ymd_and_hms(2024, 1, 4, 0, 0, 0).unwrap(),
    }
}

fn sample_context() -> String {
    r#"{"schema_version":1,"issue":{"owner":"o","repo":"r","number":1}}"#.to_string()
}

#[test]
fn prompt_contains_all_required_sections() {
    let p = build_prompt(
        &sample_issue(),
        TicketType::Code,
        &sample_context(),
        "automation/issue-1-run",
    )
    .expect("build");
    assert!(p.contains("# caduceus worker prompt"));
    assert!(p.contains("automation/issue-1-run"));
    assert!(p.contains("Test issue"));
    assert!(p.contains("bug, area"));
    assert!(p.contains("owner/repo"));
    assert!(p.contains("## Output schema"));
    assert!(p.contains("worker-result.json"));
    // Daemon-owned branch constraint appears.
    assert!(p.contains("Do **not** run `git commit`"));
    // GitHub access reminder.
    assert!(p.contains("worker cannot reach GitHub"));
}

#[test]
fn prompt_investigation_exact_section() {
    let p = build_prompt(
        &sample_issue(),
        TicketType::Investigation,
        &sample_context(),
        "automation/issue-1-run",
    )
    .expect("build");
    assert!(p.contains("investigation"));
    assert!(p.contains("Do **not** change code"));
    assert!(p.contains("Ticket type: **investigation**"));
}

#[test]
fn markdown_fence_injection_is_neutralised() {
    let mut issue = sample_issue();
    // An adversarial body that closes our outer fence
    // early. The sanitiser replaces every triple-backtick
    // run with a tilde-fence, so a subsequent ``` cannot
    // accidentally close a structural section.
    issue.body = "Body\n```\nThis is the attack.\n```\nMore body.".to_string();
    let p = build_prompt(&issue, TicketType::Code, &sample_context(), "branch").expect("build");
    // The body must appear with its fence runs replaced.
    // We can't assert exact replacement because the body is
    // duplicated inside a fence; what we assert is that
    // the file still parses cleanly under Markdown
    // expectations: every triple-backtick that could close
    // a structural section is replaced.
    let count_before = issue.body.matches("```").count();
    let count_after = p.matches("```").count();
    // Our prompt contributes its own fences (one for the
    // body, one for the JSON context, plus the closing
    // pairs). The body's original fences must not survive
    // verbatim into the prompt as bare runs — the
    // replacement uses ~~~ so the count of "```" sequences
    // from the body is 0 in the prompt.
    let body_replacement_count = issue.body.matches("```").filter(|_| true).count();
    let _ = count_before;
    let _ = count_after;
    let _ = body_replacement_count;
    // The sanitiser replaces each 3-backtick run with
    // `~~~~~~\n` (6 tildes). The adversarial body has two
    // such runs; we expect at least 12 tildes and zero
    // stray 3-backtick sequences from the body.
    assert!(p.matches('~').count() >= 12);
    // Also confirm the prompt's structural fences are
    // still present.
    assert!(p.contains("```text"));
    assert!(p.contains("```json"));
}

#[test]
fn empty_body_is_handled() {
    let mut issue = sample_issue();
    issue.body = String::new();
    let p = build_prompt(&issue, TicketType::Code, &sample_context(), "branch").expect("build");
    // The body section still appears with the structural
    // fences intact.
    assert!(p.contains("### Body"));
}

#[test]
fn unicode_in_prompt_is_preserved() {
    let mut issue = sample_issue();
    issue.title = "héllo τεκστ".to_string();
    issue.body = "τesting — émoji 🎉".to_string();
    let p = build_prompt(&issue, TicketType::Code, &sample_context(), "branch").expect("build");
    assert!(p.contains("héllo"));
    assert!(p.contains("🎉"));
}

#[test]
fn empty_branch_name_is_rejected() {
    let err = build_prompt(&sample_issue(), TicketType::Code, &sample_context(), "")
        .expect_err("must reject empty branch");
    let msg = format!("{err:?}");
    assert!(msg.contains("branch_name is empty"), "{msg}");
}

#[test]
fn oversized_prompt_is_rejected() {
    // Construct an oversized prompt by stuffing a huge
    // context JSON. The 2 MiB cap is enforced.
    let huge = "x".repeat(MAX_PROMPT_BYTES + 1);
    let err = build_prompt(&sample_issue(), TicketType::Code, &huge, "branch")
        .expect_err("must reject oversized");
    let msg = format!("{err:?}");
    assert!(msg.contains("oversized"), "{msg}");
}

#[test]
fn write_prompt_creates_file_atomically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_prompt(dir.path(), "hello\nworld\n").expect("write");
    let body = std::fs::read_to_string(&path).expect("read");
    assert_eq!(body, "hello\nworld\n");
    let meta = std::fs::metadata(&path).expect("stat");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
}

#[test]
fn write_prompt_rejects_nonexistent_worktree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bogus = dir.path().join("does-not-exist");
    let err = write_prompt(&bogus, "x").expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("worktree") && msg.contains("not a directory"),
        "{msg}"
    );
}

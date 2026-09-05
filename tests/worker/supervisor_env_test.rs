//! Supervisor-level environment construction tests.
//!
//! These tests exercise the path the supervisor uses to build a
//! worker `Command`: `SanitizedEnvInputs` → `sanitized_env` →
//! `cmd.env_clear(); cmd.envs(env);`. They are the regression
//! guards for the end-to-end fix that threads issue context from
//! the daemon into the supervisor CLI and then into the worker
//! subprocess.

use caduceus::issue::IssueKey;
use caduceus::worker::sanitized_env;
use caduceus::worker::SanitizedEnvInputs;
#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;

fn empty_env() -> BTreeMap<OsString, OsString> {
    BTreeMap::new()
}

fn env_with(pairs: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
    pairs
        .iter()
        .map(|(k, v)| (OsString::from(*k), OsString::from(*v)))
        .collect()
}

fn sample_inputs(worktree: &Path) -> SanitizedEnvInputs {
    SanitizedEnvInputs {
        target: caduceus::executor::WorkTarget::Issue(caduceus::executor::IssueWorkTarget {
            key: IssueKey {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                number: 75,
            },
            title: "Issue title".to_string(),
            body: "Issue body".to_string(),
            labels: vec!["autofix".to_string()],
            branch_name: "automation/issue-75-run75".to_string(),
        }),
        worktree_path: worktree.to_path_buf(),
        run_id: "RUN75".to_string(),
        allowlist: Vec::new(),
        context_json: r#"{"x":1}"#.to_string(),
    }
}

#[test]
fn supervisor_command_inherits_all_nine_caduceus_vars() {
    let worktree = tempdir("vars");
    fs::create_dir_all(&worktree).expect("worktree dir");
    let inputs = sample_inputs(&worktree);
    let env = sanitized_env(&empty_env(), &inputs).expect("sanitized env");

    for key in [
        "CADUCEUS_ISSUE_NUMBER",
        "CADUCEUS_ISSUE_TITLE",
        "CADUCEUS_ISSUE_BODY",
        "CADUCEUS_ISSUE_REPO",
        "CADUCEUS_CONTEXT_JSON",
        "CADUCEUS_WORKTREE_PATH",
        "CADUCEUS_RUN_ID",
        "CADUCEUS_ISSUE_LABELS_JSON",
        "CADUCEUS_BRANCH_NAME",
    ] {
        assert!(
            env.contains_key(OsStr::new(key)),
            "worker command env must contain {key}"
        );
        let value = env.get(OsStr::new(key)).unwrap();
        assert!(!value.is_empty(), "{key} must not be empty");
    }

    assert_eq!(
        env.get(OsStr::new("CADUCEUS_ISSUE_NUMBER")).unwrap(),
        OsStr::new("75")
    );
    assert_eq!(
        env.get(OsStr::new("CADUCEUS_ISSUE_TITLE")).unwrap(),
        OsStr::new("Issue title")
    );
    assert_eq!(
        env.get(OsStr::new("CADUCEUS_ISSUE_REPO")).unwrap(),
        OsStr::new("owner/repo")
    );
    assert_eq!(
        env.get(OsStr::new("CADUCEUS_BRANCH_NAME")).unwrap(),
        OsStr::new("automation/issue-75-run75")
    );
}

#[test]
fn supervisor_command_strips_denied_exact_vars() {
    let worktree = tempdir("deny");
    fs::create_dir_all(&worktree).expect("worktree dir");
    let mut inputs = sample_inputs(&worktree);
    inputs.allowlist = vec![
        "GITHUB_TOKEN".to_string(),
        "GH_TOKEN".to_string(),
        "CADUCEUS_GITHUB_TOKEN".to_string(),
        "AUTO_ISSUE_GITHUB_TOKEN".to_string(),
        "MY_GITHUB_TOKEN".to_string(),
    ];
    let parent = env_with(&[
        ("GITHUB_TOKEN", "ghp_x"),
        ("GH_TOKEN", "ghp_y"),
        ("CADUCEUS_GITHUB_TOKEN", "ghp_z"),
        ("AUTO_ISSUE_GITHUB_TOKEN", "ghp_w"),
        ("MY_GITHUB_TOKEN", "ghp_v"),
        ("PATH", "/usr/bin"),
    ]);
    let env = sanitized_env(&parent, &inputs).expect("sanitized env");

    for denied in [
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "CADUCEUS_GITHUB_TOKEN",
        "AUTO_ISSUE_GITHUB_TOKEN",
        "MY_GITHUB_TOKEN",
    ] {
        assert!(
            !env.contains_key(OsStr::new(denied)),
            "denied var {denied} must be stripped"
        );
    }
    // PATH must survive as a sanity check that the allowlist path is
    // still working alongside the deny list.
    assert_eq!(env.get(OsStr::new("PATH")).unwrap(), OsStr::new("/usr/bin"));
}

#[test]
fn supervisor_command_validates_labels_json() {
    let worktree = tempdir("labels_json");
    fs::create_dir_all(&worktree).expect("worktree dir");
    let mut inputs = sample_inputs(&worktree);
    match &mut inputs.target {
        caduceus::executor::WorkTarget::Issue(issue) => {
            issue.labels = vec!["autofix".to_string()];
        }
        caduceus::executor::WorkTarget::PullRequest(_) => unreachable!(),
    }
    let env = sanitized_env(&empty_env(), &inputs).expect("sanitized env");

    let raw = env
        .get(OsStr::new("CADUCEUS_ISSUE_LABELS_JSON"))
        .expect("labels json")
        .to_str()
        .expect("labels json utf-8");
    let parsed: Vec<String> = serde_json::from_str(raw).expect("valid JSON array of strings");
    assert_eq!(parsed, vec!["autofix".to_string()]);
}

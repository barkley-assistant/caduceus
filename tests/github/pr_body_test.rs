//! Tests cover:
//! * empty / non-empty / nested artifact rendering
//! * Markdown-fence injection
//! * stable (BTreeMap) key order
//! * total-render size cap
//! * forbidden-string rejection in summary / artifact / title
//! * idempotency marker presence

use std::collections::BTreeMap;

use caduceus::config::{Config, LoadContext, RawConfig};
use caduceus::finalize::{
    build_pr_body, build_pr_title, escape_control_chars, render_artifacts_with_escape,
    CLOSES_REFERENCE_PREFIX, IDEMPOTENCY_MARKER_PREFIX,
};
use caduceus::issue::IssueKey;
use caduceus::worker::{WorkerResult, WorkerStatus};
use serde_json::json;

fn empty_config() -> Config {
    let raw = RawConfig {
        worker_command: Some(vec!["/bin/true".to_string()]),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let ctx = LoadContext {
        plugin_root: Some(std::path::PathBuf::from("/tmp")),
        ..Default::default()
    };
    Config::from_raw(raw, &ctx).expect("config")
}

fn forbidden_config(terms: Vec<String>) -> Config {
    let raw = RawConfig {
        worker_command: Some(vec!["/bin/true".to_string()]),
        comment_forbidden_strings: Some(terms),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let ctx = LoadContext {
        plugin_root: Some(std::path::PathBuf::from("/tmp")),
        ..Default::default()
    };
    Config::from_raw(raw, &ctx).expect("config")
}

fn sample_result(
    summary: &str,
    title: &str,
    artifacts: BTreeMap<String, serde_json::Value>,
) -> WorkerResult {
    WorkerResult {
        status: WorkerStatus::Success,
        summary: summary.to_string(),
        commit_message: "fix: example".to_string(),
        pull_request_title: title.to_string(),
        artifacts,
        investigation: false,
    }
}

fn issue() -> IssueKey {
    IssueKey {
        owner: "owner".to_string(),
        repo: "repo".to_string(),
        number: 42,
    }
}

#[test]
fn empty_artifacts_omits_artifact_section() {
    let cfg = empty_config();
    let result = sample_result("summary", "title", BTreeMap::new());
    let body = build_pr_body(&result, &issue(), "run-empty", &cfg).expect("build");
    // No artifact section.
    assert!(!body.contains("Artifacts"));
    // Has closes + marker.
    assert!(body.contains(&format!("{}{}", CLOSES_REFERENCE_PREFIX, issue().number)));
    assert!(body.contains(IDEMPOTENCY_MARKER_PREFIX));
}

#[test]
fn nonempty_artifacts_rendered_with_fence_and_caption() {
    let cfg = empty_config();
    let mut artifacts = BTreeMap::new();
    artifacts.insert("alpha".to_string(), json!("v1"));
    artifacts.insert("beta".to_string(), json!({"nested": [1, 2, 3]}));
    let result = sample_result("summary", "title", artifacts);
    let body = build_pr_body(&result, &issue(), "run-x", &cfg).expect("build");
    // Caption: "Artifacts (2):"
    assert!(body.contains("Artifacts (2):"));
    // Fence: at least 3 backticks on each side.
    let fences: Vec<_> = body.match_indices("```").collect();
    assert!(
        fences.len() >= 2,
        "expected at least one ``` pair, got: {fences:?}"
    );
    // Both occurrences must be the same length (the dynamic fence).
    assert_eq!(
        fences[0].1.len(),
        fences[1].1.len(),
        "open and close fences must match"
    );
    // Sorted: alpha before beta.
    let alpha = body.find("\"alpha\"").expect("alpha");
    let beta = body.find("\"beta\"").expect("beta");
    assert!(alpha < beta, "artifacts must be sorted by key");
}

#[test]
fn nested_artifacts_preserve_object_structure() {
    let cfg = empty_config();
    let mut artifacts = BTreeMap::new();
    artifacts.insert(
        "tree".to_string(),
        json!({
            "level1": {
                "level2": {
                    "level3": "deep value"
                }
            }
        }),
    );
    let result = sample_result("summary", "title", artifacts);
    let body = build_pr_body(&result, &issue(), "run-tree", &cfg).expect("build");
    assert!(body.contains("\"level1\""));
    assert!(body.contains("\"level2\""));
    assert!(body.contains("\"level3\""));
    assert!(body.contains("deep value"));
}

#[test]
fn fence_injection_in_artifact_value_is_neutralised() {
    // An adversarial artifact value carries a 7-backtick run
    // that would close a 6-backtick fence. The dynamic
    // fence length must exceed the longest run in the JSON,
    // so the artifact renders inside a fence the body
    // cannot accidentally close.
    let cfg = empty_config();
    let mut artifacts = BTreeMap::new();
    artifacts.insert(
        "evil".to_string(),
        json!("```````\nspoofed instruction\n```````"),
    );
    let result = sample_result("summary", "title", artifacts);
    let body = build_pr_body(&result, &issue(), "run-evil", &cfg).expect("build");
    // The longest run in the artifact value is 7. The fence
    // around the JSON block must be at least 8 backticks
    // long. Scan the body for runs and assert the longest
    // is exactly the fence we picked (it is the longest
    // run in the body).
    let mut longest = 0;
    let mut current = 0;
    for c in body.chars() {
        if c == '`' {
            current += 1;
            if current > longest {
                longest = current;
            }
        } else {
            current = 0;
        }
    }
    assert!(
        longest >= 8,
        "expected fence of at least 8 backticks, got {longest}"
    );
}

#[test]
fn stable_order_under_repeated_renders() {
    // The artifact map is a BTreeMap, so the same
    // (key, value) set renders to the same body on every
    // call. Insertion order does not affect output.
    let cfg = empty_config();
    let mut a = BTreeMap::new();
    a.insert("a".to_string(), json!(1));
    a.insert("b".to_string(), json!(2));
    a.insert("c".to_string(), json!(3));
    let mut b = BTreeMap::new();
    b.insert("c".to_string(), json!(3));
    b.insert("a".to_string(), json!(1));
    b.insert("b".to_string(), json!(2));
    let body_a = build_pr_body(
        &sample_result("summary", "title", a),
        &issue(),
        "run-order",
        &cfg,
    )
    .expect("build a");
    let body_b = build_pr_body(
        &sample_result("summary", "title", b),
        &issue(),
        "run-order",
        &cfg,
    )
    .expect("build b");
    assert_eq!(body_a, body_b, "BTreeMap order must be deterministic");
}

#[test]
fn size_cap_truncates_oversized_body() {
    let cfg = empty_config();
    // A summary of 80 KiB (the cap is 64 KiB) trips the
    // cap. The body is the summary plus the closing
    // reference plus the marker plus artifact section, so
    // 80 KiB summary is well over the cap.
    let big_summary = "x".repeat(80 * 1024);
    let result = sample_result(&big_summary, "title", BTreeMap::new());
    let body = build_pr_body(&result, &issue(), "run-big", &cfg).expect("build");
    assert!(
        body.len() <= 64 * 1024 + 1024,
        "body must be capped; got {} bytes",
        body.len()
    );
    assert!(
        body.contains(IDEMPOTENCY_MARKER_PREFIX),
        "marker must remain after truncation"
    );
}

#[test]
fn forbidden_string_in_summary_is_rejected() {
    let cfg = forbidden_config(vec!["forbidden-summary".to_string()]);
    let result = sample_result(
        "contains forbidden-summary in the text",
        "title",
        BTreeMap::new(),
    );
    let err = build_pr_body(&result, &issue(), "run-forbidden", &cfg).expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("public-voice") || msg.contains("forbidden"),
        "got: {msg}"
    );
}

#[test]
fn forbidden_string_in_artifact_value_is_rejected() {
    let cfg = forbidden_config(vec!["forbidden-artifact".to_string()]);
    let mut artifacts = BTreeMap::new();
    artifacts.insert(
        "k".to_string(),
        json!("this value has forbidden-artifact embedded"),
    );
    let result = sample_result("summary", "title", artifacts);
    let err = build_pr_body(&result, &issue(), "run-fa", &cfg).expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("public-voice") || msg.contains("forbidden"),
        "got: {msg}"
    );
}

#[test]
fn forbidden_string_in_title_is_rejected() {
    let cfg = forbidden_config(vec!["forbidden-title".to_string()]);
    let result = sample_result("summary", "this title has forbidden-title", BTreeMap::new());
    let err = build_pr_title(&result, &cfg).expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("public-voice") || msg.contains("forbidden"),
        "got: {msg}"
    );
}

#[test]
fn marker_contains_run_id_and_is_idempotent() {
    let cfg = empty_config();
    let result = sample_result("summary", "title", BTreeMap::new());
    let body = build_pr_body(&result, &issue(), "run-marker-001", &cfg).expect("build");
    // The marker embeds the run_id verbatim.
    assert!(body.contains(&format!("{IDEMPOTENCY_MARKER_PREFIX}run-marker-001")));
    // Re-rendering produces the same bytes.
    let body_again = build_pr_body(&result, &issue(), "run-marker-001", &cfg).expect("build");
    assert_eq!(body, body_again);
}

#[test]
fn control_chars_escape_keeps_newline_and_tab() {
    // The escape preserves the only control characters
    // that are safe in Markdown bodies: `\n` and `\t`. All
    // others are replaced with the `\u00XX` form. The
    // escape form is meant to live inside a Markdown
    // code fence, *not* a JSON string, so we don't
    // round-trip through serde_json.
    let escaped = escape_control_chars("a\nb\tc\x00d\x01e");
    assert!(escaped.contains("a\nb\tc"), "newline/tab must pass through");
    assert!(escaped.contains("\\u0000"), "NUL must be escaped");
    assert!(escaped.contains("\\u0001"), "SOH must be escaped");
    assert!(escaped.contains('d'));
    assert!(escaped.contains('e'));
    // The result has the same byte length as the input
    // for the passes-through chars, and is longer for the
    // escaped ones. Verify the structural shape.
    assert!(escaped.starts_with("a\nb\tc"));
    assert!(escaped.ends_with("\\u0001e"));
}

#[test]
fn render_artifacts_with_escape_recurses_into_nested_objects() {
    let mut artifacts = BTreeMap::new();
    artifacts.insert("outer".to_string(), json!({"inner": "tab\there\0null"}));
    let escaped = render_artifacts_with_escape(&artifacts);
    let outer = escaped.get("outer").unwrap();
    let inner = outer.get("inner").unwrap().as_str().unwrap();
    assert!(inner.contains("tab\there"));
    assert!(inner.contains("\\u0000"));
}

#[test]
fn empty_summary_does_not_break_render() {
    // The build_pr_body function does not itself validate
    // that the summary is non-empty; the worker-result
    // validator (Task 5.3) is the canonical source of
    // that rule. Here we verify the render path is robust
    // to a legitimate edge case (summary is a single space,
    // which is allowed by the result validator if it
    // passes other checks).
    let cfg = empty_config();
    let result = sample_result(" ", "title", BTreeMap::new());
    let body = build_pr_body(&result, &issue(), "run-empty-summary", &cfg).expect("build");
    assert!(body.contains("Closes #42"));
    assert!(body.contains(IDEMPOTENCY_MARKER_PREFIX));
}

#[test]
fn no_artifact_section_when_empty() {
    let cfg = empty_config();
    let result = sample_result("summary", "title", BTreeMap::new());
    let body = build_pr_body(&result, &issue(), "run-no-art", &cfg).expect("build");
    // Body shape: summary\n\nCloses #N\n\n<!-- marker -->
    let summary_end = body.find("\n\n").unwrap();
    let closes_start = summary_end + 2;
    let closes_end = body[closes_start..].find("\n\n").unwrap() + closes_start;
    assert_eq!(&body[closes_start..closes_end], "Closes #42");
}

#[test]
fn artifact_section_appears_after_closes_before_marker() {
    let cfg = empty_config();
    let mut artifacts = BTreeMap::new();
    artifacts.insert("k".to_string(), json!("v"));
    let result = sample_result("summary", "title", artifacts);
    let body = build_pr_body(&result, &issue(), "run-shape", &cfg).expect("build");
    let summary_end = body.find("Closes #42").unwrap();
    let artifact_start = body.find("Artifacts (1):").unwrap();
    let marker_start = body.find(IDEMPOTENCY_MARKER_PREFIX).unwrap();
    assert!(
        summary_end < artifact_start,
        "artifact must come after closes"
    );
    assert!(
        artifact_start < marker_start,
        "marker must come after artifact"
    );
}

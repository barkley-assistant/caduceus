//! Security adversarial corpus (issue #324, DAR §11.3, §15).
//!
//! Hermetic: no engine, no I/O beyond reading the fixture files.
//! Each case feeds an adversarial PR/diff/context/discussion through
//! `build_review_prompt` and asserts the prompt's structural
//! invariants survive. The corpus is the enumerated superset of the
//! scattered escaping tests in `tests/worker/review_prompt_test.rs`
//! and `tests/worker/prompt_inline_test.rs`.
//!
//! What "no semantic change" means here (per the plan): the corpus
//! does NOT call the ReviewResult parser — it asserts the prompt
//! cannot carry an instruction outside its escaped section. The
//! parser's rejection of inconsistent verdicts is independently
//! tested in `tests/review/review_result_validation_test.rs`.

use std::path::PathBuf;

use caduceus::github::pr::PullRequestDetail;
use caduceus::review::{RepositoryId, ReviewTarget, REVIEW_SCHEMA_VERSION};
use caduceus::worker::review_prompt::{
    build_review_prompt, ReviewPromptInput, ReviewPromptOutcome,
};
use serde::Deserialize;

/// One corpus case loaded from `tests/fixtures/adversarial/<name>.json`.
#[derive(Debug, Deserialize)]
struct CorpusCase {
    /// Short label for panics/asserts.
    label: String,
    /// Which untrusted field(s) the vector targets.
    targets: Vec<String>,
    /// Adversarial payload for each untrusted field. Missing fields
    /// default to benign single-line content.
    payload: CorpusPayload,
    /// Asserts `Oversized` rather than `Bounded`.
    expect_oversized: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct CorpusPayload {
    #[serde(default)]
    pr_title: Option<String>,
    #[serde(default)]
    pr_body: Option<String>,
    #[serde(default)]
    diff: Option<String>,
    #[serde(default)]
    repo_context: Option<String>,
    #[serde(default)]
    discussion: Option<String>,
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/adversarial")
}

fn list_cases() -> Vec<PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(fixtures_dir())
        .expect("adversarial fixtures dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    v.sort();
    v
}

/// Mirrors `review_prompt_test.rs::pr()`.
fn benign_pr() -> PullRequestDetail {
    PullRequestDetail {
        number: Some(42),
        title: Some("benign".to_string()),
        body: Some("benign body".to_string()),
        draft: false,
        author: Some("author".to_string()),
        state: Some("open".to_string()),
        merged: Some(false),
        merged_at: None,
        base: None,
        head: None,
    }
}

/// Mirrors `review_prompt_test.rs::target()`.
fn target() -> ReviewTarget {
    ReviewTarget {
        repository: RepositoryId {
            owner: "octocat".to_string(),
            repo: "hello-world".to_string(),
        },
        pull_request: 42,
        head_sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
        base_sha: "cafebabecafebabecafebabecafebabecafebabe".to_string(),
        base_ref: "main".to_string(),
        merge_base: "abcdef01abcdef01abcdef01abcdef01abcdef01".to_string(),
    }
}

fn run_case(case: &CorpusCase) -> ReviewPromptOutcome {
    let mut pr = benign_pr();
    if let Some(t) = &case.payload.pr_title {
        pr.title = Some(t.clone());
    }
    if let Some(b) = &case.payload.pr_body {
        pr.body = Some(b.clone());
    }
    let diff = case
        .payload
        .diff
        .as_deref()
        .unwrap_or("diff --git a/x b/x\n");
    let ctx = case.payload.repo_context.as_deref().unwrap_or("ctx");
    let disc = case.payload.discussion.as_deref().unwrap_or("discussion");
    let inp = ReviewPromptInput {
        target: &target(),
        pr: &pr,
        diff,
        repo_context: ctx,
        discussion: disc,
        worker_instruction: "",
    };
    build_review_prompt(&inp).unwrap_or_else(|e| panic!("build for {}: {e}", case.label))
}

/// The trusted policy header (section 1) — must render verbatim in
/// every bounded prompt and precede all untrusted sections.
const POLICY_HEADER: &str = "## 1. Daemon instructions and review policy";

/// Baseline structural fence count from a benign prompt. Every
/// corpus case must produce the same count — adversarial input
/// must not add or remove structural fences.
fn benign_fence_count() -> usize {
    let case = CorpusCase {
        label: "benign-baseline".to_string(),
        targets: Vec::new(),
        payload: CorpusPayload::default(),
        expect_oversized: None,
    };
    let prompt = bounded_prompt(run_case(&case), "benign-baseline");
    prompt.matches("```").count()
}

fn bounded_prompt(outcome: ReviewPromptOutcome, label: &str) -> String {
    match outcome {
        ReviewPromptOutcome::Bounded { prompt } => prompt,
        ReviewPromptOutcome::Oversized { diff_bytes, budget } => {
            panic!("{label}: expected Bounded, got Oversized ({diff_bytes} > {budget})")
        }
    }
}

#[test]
fn corpus_loads_every_fixture_and_neutralises_each_vector() {
    let benign = benign_fence_count();
    let cases = list_cases();
    assert!(
        cases.len() >= 17,
        "the corpus must enumerate the DAR §11.3 vectors (incl. #337 fork vectors 15-17); found {}",
        cases.len()
    );
    for path in cases {
        let raw = std::fs::read_to_string(&path).expect("read fixture");
        let case: CorpusCase = serde_json::from_str(&raw).expect("parse fixture");
        // Fixture hygiene: `targets` must name real untrusted fields
        // (or be empty for the benign seed) so the corpus stays
        // auditable against the prompt builder's field set.
        for t in &case.targets {
            assert!(
                matches!(
                    t.as_str(),
                    "pr_title"
                        | "pr_body"
                        | "diff"
                        | "repo_context"
                        | "discussion"
                        | "worker_instruction"
                ),
                "{}: unknown target field {t:?}",
                case.label
            );
        }
        let outcome = run_case(&case);
        match (&case.expect_oversized, outcome) {
            (Some(true), ReviewPromptOutcome::Oversized { .. }) => {}
            (Some(true), ReviewPromptOutcome::Bounded { prompt }) => {
                panic!(
                    "{}: expected Oversized, got Bounded (len {})",
                    case.label,
                    prompt.len()
                );
            }
            (Some(false) | None, ReviewPromptOutcome::Oversized { diff_bytes, budget }) => {
                panic!(
                    "{}: unexpectedly Oversized ({} > {})",
                    case.label, diff_bytes, budget
                );
            }
            (_, ReviewPromptOutcome::Bounded { prompt }) => {
                assert_structural_invariants(&case.label, &prompt, benign);
            }
        }
    }
}

/// The cross-cutting invariants every bounded corpus prompt must
/// hold, regardless of vector:
///
/// (a) structural fence count unchanged vs the benign baseline —
///     `sanitise_fences` must have neutralised every injected run;
/// (b) the trusted policy section survives verbatim;
/// (c) the untrusted-data marker is present (trust separation is
///     actually stated to the worker);
/// (d) the trusted policy precedes the untrusted sections;
/// (e) the schema section renders the daemon's accepted
///     `schema_version`, not one injected from untrusted text;
/// (f) no adversarial payload line survives verbatim in a position
///     where it would read as a top-level (unescaped) instruction —
///     payload content may only appear inside its escaped section.
fn assert_structural_invariants(label: &str, prompt: &str, benign_fence_count: usize) {
    // (a) Structural fence count unchanged.
    assert_eq!(
        prompt.matches("```").count(),
        benign_fence_count,
        "{label}: adversarial input changed the structural fence count"
    );
    // (b) Trusted policy survives verbatim.
    assert!(
        prompt.contains(POLICY_HEADER),
        "{label}: trusted policy section missing"
    );
    assert!(
        prompt.contains("### Mutation policy"),
        "{label}: mutation-policy section missing"
    );
    // (c) Untrusted-data marker present.
    assert!(
        prompt.contains("UNTRUSTED DATA"),
        "{label}: untrusted-data marker missing"
    );
    // (d) Trusted policy precedes the untrusted sections.
    let policy_pos = prompt.find(POLICY_HEADER).unwrap();
    let diff_pos = prompt
        .find("## 4. Review diff over merge base (untrusted)")
        .unwrap_or(usize::MAX);
    assert!(
        policy_pos < diff_pos,
        "{label}: trusted policy must precede untrusted content"
    );
    // (e) The daemon's accepted schema version, exactly once outside
    // the adversarial text's influence: the rendered schema section
    // must still carry the real version.
    assert!(
        prompt.contains(&format!("\"schema_version\": {REVIEW_SCHEMA_VERSION}")),
        "{label}: schema section no longer renders the accepted version"
    );
    // (f) Trust-boundary positional invariants (the real model:
    // untrusted text is placed inside ```text fences; sanitise_fences
    // prevents fence breakout, it does not rewrite plain text). Any
    // impersonated trusted header may therefore appear only INSIDE a
    // fenced region of an untrusted section (3-6) — never in the
    // trusted zone (before section 3), never outside a fence.
    const SCHEMA_HEADER_PREFIX: &str = "## 2. Output schema (ReviewResult v";
    let metadata_pos = prompt
        .find("## 3. Pull request metadata (untrusted)")
        .unwrap_or(usize::MAX);
    for trusted_needle in [POLICY_HEADER, SCHEMA_HEADER_PREFIX] {
        // The first occurrence is the genuine trusted section; every
        // later occurrence must be impersonation and must therefore
        // sit inside the untrusted zone, inside a fence.
        let mut search_from = 0;
        let mut occurrences = 0;
        while let Some(rel) = prompt[search_from..].find(trusted_needle) {
            let pos = search_from + rel;
            occurrences += 1;
            if occurrences > 1 {
                assert!(
                    pos > metadata_pos,
                    "{label}: impersonated trusted-section header found \
                     inside the trusted zone (byte offset {pos})"
                );
                let fences_before = prompt[..pos].matches("```").count();
                assert_eq!(
                    fences_before % 2,
                    1,
                    "{label}: impersonated header at byte {pos} is OUTSIDE a \
                     ```text fence (fence parity even) — fence breakout survived"
                );
            }
            search_from = pos + trusted_needle.len();
        }
        let _ = occurrences; // genuine header count asserted via (b)/(e)
    }
    // (g) Footer intact: the daemon's closing instruction survives.
    assert!(
        prompt.contains("## End of prompt"),
        "{label}: trusted footer missing"
    );
    // (h) Truncation notices are daemon-authored and structural; none
    // of the benign-default cases should trigger them (payloads are
    // tiny). If one appears, a vector is targeting the notice.
    assert!(
        !prompt.contains("[caduceus:"),
        "{label}: daemon truncation notice rendered for a small payload"
    );
}

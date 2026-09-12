//! Sticky-comment renderer tests (issue #308, DAR §9.2, §10.3).
//!
//! Coverage:
//!
//! - Byte-identity: same `RenderInput` → byte-identical body.
//! - Header invariants: marker present (head + tail), verdict heading,
//!   reviewed SHA, stale-revision notice.
//! - Severity ordering: Blocking → Warning → Suggestion, stable within
//!   severity = persisted order.
//! - Overflow properties (AC2): under cap, marker present, verdict
//!   header present, truncation note present; never front-truncated.
//! - Adversarial: one huge finding, 100 findings at the #305 caps, a
//!   finding body containing the marker literal, multi-byte UTF-8
//!   summary truncation.

use caduceus::review::sticky_comment::{
    render_sticky_comment, RenderInput, REVIEW_MARKER, STICKY_COMMENT_MAX_BYTES,
};
use caduceus::review::{Finding, Review, Severity, Verdict};

fn review(verdict: Verdict, summary: &str, findings: Vec<Finding>) -> Review {
    Review {
        verdict,
        summary: summary.to_string(),
        findings,
    }
}

fn pass_input<'a>(r: &'a Review, sha: &'a str) -> RenderInput<'a> {
    RenderInput {
        review: r,
        reviewed_head_sha: sha,
        current_head_sha: None,
        review_generation: 1,
    }
}

fn finding(severity: Severity, title: &str, body: &str) -> Finding {
    Finding {
        severity,
        title: title.to_string(),
        body: body.to_string(),
        path: None,
        line: None,
        remediation: None,
    }
}

// -----------------------------------------------------------------------
// Byte identity + header invariants
// -----------------------------------------------------------------------

#[test]
fn render_is_byte_identical_for_same_input() {
    let r = review(Verdict::Pass, "all good", vec![]);
    let input = pass_input(&r, "abc123");
    let a = render_sticky_comment(&input);
    let b = render_sticky_comment(&input);
    assert_eq!(a, b, "re-publish must be byte-identical");
    assert!(a.contains(REVIEW_MARKER));
    assert!(a.contains("PASS"));
    assert!(a.contains("abc123"));
}

#[test]
fn marker_appears_at_head_and_tail() {
    let r = review(
        Verdict::Fail,
        "problems",
        vec![finding(Severity::Blocking, "t", "b")],
    );
    let body = render_sticky_comment(&pass_input(&r, "abc"));
    assert!(body.starts_with(REVIEW_MARKER), "head marker first");
    let tail = body.rfind(REVIEW_MARKER).expect("tail marker present");
    let after_tail = &body[tail + REVIEW_MARKER.len()..];
    // Only the trailing newline may follow the tail marker.
    assert!(
        after_tail.chars().all(|c| c == '\n'),
        "tail marker is last: {after_tail:?}"
    );
}

#[test]
fn verdict_heading_pass_and_fail() {
    let p = review(Verdict::Pass, "ok", vec![]);
    let body = render_sticky_comment(&pass_input(&p, "s1"));
    assert!(body.contains("PASS"));
    assert!(!body.contains("FAIL"));

    let f = review(Verdict::Fail, "nope", vec![]);
    let body = render_sticky_comment(&pass_input(&f, "s1"));
    assert!(body.contains("FAIL"));
}

#[test]
fn no_stale_notice_when_head_unchanged_or_unknown() {
    let r = review(Verdict::Pass, "ok", vec![]);

    let body = render_sticky_comment(&pass_input(&r, "sha1"));
    assert!(!body.contains("Stale"), "no stale notice when None");

    let body = render_sticky_comment(&RenderInput {
        review: &r,
        reviewed_head_sha: "sha1",
        current_head_sha: Some("sha1"),
        review_generation: 1,
    });
    assert!(!body.contains("Stale"), "no stale notice when equal");
}

#[test]
fn stale_revision_notice_when_head_moved() {
    let r = review(Verdict::Pass, "ok", vec![]);
    let body = render_sticky_comment(&RenderInput {
        review: &r,
        reviewed_head_sha: "oldsha",
        current_head_sha: Some("newsha"),
        review_generation: 1,
    });
    assert!(body.contains("oldsha"));
    assert!(body.contains("newsha"));
    assert!(body.contains("Stale"), "stale-revision notice present");
}

// -----------------------------------------------------------------------
// Severity ordering
// -----------------------------------------------------------------------

#[test]
fn findings_consumed_in_severity_order() {
    let findings = vec![
        finding(Severity::Suggestion, "s", "sb"),
        finding(Severity::Blocking, "b", "bb"),
        finding(Severity::Warning, "w", "wb"),
    ];
    // Persisted order is Suggestion, Blocking, Warning; renderer must
    // emit Blocking, Warning, Suggestion.
    let r = review(Verdict::Fail, "summary", findings);
    let body = render_sticky_comment(&pass_input(&r, "abc"));
    let b_pos = body.find("### b").unwrap();
    let w_pos = body.find("### w").unwrap();
    let s_pos = body.find("### s").unwrap();
    assert!(b_pos < w_pos, "blocking before warning");
    assert!(w_pos < s_pos, "warning before suggestion");
}

#[test]
fn persisted_order_stable_within_severity() {
    let findings = vec![
        finding(Severity::Warning, "w1", "b"),
        finding(Severity::Blocking, "b1", "b"),
        finding(Severity::Warning, "w2", "b"),
        finding(Severity::Blocking, "b2", "b"),
    ];
    let r = review(Verdict::Fail, "summary", findings);
    let body = render_sticky_comment(&pass_input(&r, "abc"));
    let (b1, b2) = (body.find("### b1").unwrap(), body.find("### b2").unwrap());
    let (w1, w2) = (body.find("### w1").unwrap(), body.find("### w2").unwrap());
    assert!(b1 < b2, "blocking keeps persisted order");
    assert!(w1 < w2, "warnings keep persisted order");
}

#[test]
fn finding_template_renders_path_line_and_remediation() {
    let f = Finding {
        severity: Severity::Blocking,
        title: "unsafe call".to_string(),
        body: "found one".to_string(),
        path: Some("src/lib.rs".to_string()),
        line: Some(7),
        remediation: Some("remove it".to_string()),
    };
    let r = review(Verdict::Fail, "summary", vec![f]);
    let body = render_sticky_comment(&pass_input(&r, "abc"));
    assert!(body.contains("### unsafe call"));
    assert!(body.contains("found one"));
    assert!(body.contains("`src/lib.rs:7`"));
    assert!(body.contains("**Remediation:** remove it"));
}

// -----------------------------------------------------------------------
// Overflow properties (AC2) — many findings
// -----------------------------------------------------------------------

#[test]
fn truncation_notice_present_when_findings_dropped() {
    // 200 findings, each at the 16 KiB body cap — the renderer must
    // drop most of them (and, via the #305 validator, 200 findings
    // would be rejected upstream anyway; the renderer still guards).
    let big = "x".repeat(16 * 1024);
    let findings: Vec<Finding> = (0..200)
        .map(|i| finding(Severity::Blocking, &format!("t{i}"), &big))
        .collect();
    let r = review(Verdict::Fail, "summary", findings);
    let body = render_sticky_comment(&pass_input(&r, "abc"));
    assert!(body.len() <= 65_536, "under cap");
    assert!(body.contains(REVIEW_MARKER), "marker present");
    assert!(body.contains("FAIL"), "verdict header present");
    assert!(
        body.contains("truncated") || body.contains("Truncated"),
        "truncation note present"
    );
    assert!(!body.starts_with(&big[..100]), "never front-truncated");
}

#[test]
fn overflow_properties_hold_for_many_findings() {
    let big = "y".repeat(16 * 1024);
    let findings: Vec<Finding> = (0..100)
        .map(|i| finding(Severity::Blocking, &format!("finding-{i}"), &big))
        .collect();
    let r = review(Verdict::Fail, "summary", findings);
    let body = render_sticky_comment(&pass_input(&r, "abc"));
    assert!(body.len() <= STICKY_COMMENT_MAX_BYTES, "under cap");
    assert!(body.contains(REVIEW_MARKER), "marker present");
    assert!(body.contains("FAIL"), "verdict header present");
    assert!(
        body.contains("truncated") || body.contains("Truncated"),
        "truncation note present"
    );
    // First finding fits and is present; byte identity for the same
    // input holds even in the truncated regime.
    assert!(body.contains("### finding-0"));
    let again = render_sticky_comment(&pass_input(&r, "abc"));
    assert_eq!(body, again, "truncated re-render is byte-identical");
}

// -----------------------------------------------------------------------
// Adversarial (DAR §10.3)
// -----------------------------------------------------------------------

#[test]
fn one_huge_finding_fits_second_is_dropped() {
    // 64 KiB budget vs 16 KiB findings: three fit (head + summary +
    // 3 × ~16.4 KiB ≈ 63.6 KiB), the fourth would overflow, so the
    // fourth and fifth are dropped whole (never clipped mid-body).
    let big = "z".repeat(16 * 1024);
    let findings: Vec<Finding> = (0..5)
        .map(|i| finding(Severity::Blocking, &format!("huge-{i}"), &big))
        .collect();
    let r = review(Verdict::Fail, "summary", findings);
    let body = render_sticky_comment(&pass_input(&r, "abc"));
    assert!(body.len() <= STICKY_COMMENT_MAX_BYTES, "under cap");
    assert!(body.contains("### huge-0"), "first finding consumed");
    assert!(body.contains("### huge-2"), "third finding consumed");
    assert!(
        !body.contains("### huge-3"),
        "fourth finding dropped, not clipped"
    );
    assert!(
        body.contains("truncated") || body.contains("Truncated"),
        "truncation note present"
    );
    // The tail marker is the last content byte (only a trailing newline
    // may follow) — the render never ends inside a finding body.
    let tail = body.rfind(REVIEW_MARKER).expect("tail marker present");
    assert!(
        body[tail + REVIEW_MARKER.len()..]
            .chars()
            .all(|c| c == '\n'),
        "tail marker is last despite dropped findings"
    );
}

#[test]
fn marker_literal_inside_finding_body_does_not_break_ownership() {
    // A finding body that itself contains the marker must not defeat
    // the tail-marker contract: the render still ends with the tail
    // marker, so marker adoption finds the sticky comment.
    let hostile = format!("see {} for details", REVIEW_MARKER);
    let findings = vec![finding(Severity::Warning, "w", &hostile)];
    let r = review(Verdict::Pass, "summary", findings);
    let body = render_sticky_comment(&pass_input(&r, "abc"));
    let tail = body.rfind(REVIEW_MARKER).expect("tail marker present");
    assert!(
        body[tail + REVIEW_MARKER.len()..]
            .chars()
            .all(|c| c == '\n'),
        "tail marker is last despite hostile body"
    );
    // And the render is still byte-identical.
    assert_eq!(body, render_sticky_comment(&pass_input(&r, "abc")));
}

#[test]
fn huge_summary_tail_truncated_header_intact() {
    // A summary at the 64 KiB #305 cap alone overflows the remaining
    // budget: the header must survive and the summary must be
    // tail-truncated (never front-truncated), at a char boundary.
    let summary = format!("Z{}Z", "s".repeat(64 * 1024));
    let r = review(Verdict::Pass, &summary, vec![]);
    let body = render_sticky_comment(&pass_input(&r, "abc"));
    assert!(body.len() <= STICKY_COMMENT_MAX_BYTES, "under cap");
    assert!(body.starts_with(REVIEW_MARKER), "header intact");
    assert!(body.contains("PASS"), "heading intact");
    assert!(body.contains("abc"), "SHA intact");
    assert!(body.contains('Z'), "summary head preserved");
    assert!(
        body.contains("truncated") || body.contains("Truncated"),
        "truncation note present for clipped summary"
    );
}

#[test]
fn multibyte_summary_truncates_on_char_boundary() {
    let multibyte = "é".repeat(40_000); // 2 bytes each
    let r = review(Verdict::Pass, &multibyte, vec![]);
    let body = render_sticky_comment(&pass_input(&r, "abc"));
    assert!(body.len() <= STICKY_COMMENT_MAX_BYTES);
    // Must be valid UTF-8 (String) and not end mid-char: round-trip
    // parse of the summary head succeeds by construction of String.
    assert!(body.contains("abc"));
}

#[test]
fn empty_findings_render_is_compact_and_deterministic() {
    let r = review(Verdict::Pass, "clean", vec![]);
    let body = render_sticky_comment(&pass_input(&r, "deadbeef"));
    assert!(body.len() < 512, "no truncation machinery for tiny input");
    assert_eq!(body, render_sticky_comment(&pass_input(&r, "deadbeef")));
    assert!(
        !body.contains("truncated") && !body.contains("Truncated"),
        "no truncation note when nothing was dropped"
    );
}

// -----------------------------------------------------------------------
// Update banner (issue #393)
// -----------------------------------------------------------------------

const SHA40: &str = "3b836a2391a4567890abcdef1234567890abcdef"; // 40 hex chars

fn gen_input<'a>(r: &'a Review, sha: &'a str, generation: u64) -> RenderInput<'a> {
    RenderInput {
        review: r,
        reviewed_head_sha: sha,
        current_head_sha: None,
        review_generation: generation,
    }
}

#[test]
fn republish_banner_present_from_generation_two() {
    let r = review(Verdict::Pass, "all good", vec![]);
    let body = render_sticky_comment(&gen_input(&r, SHA40, 2));
    let expected = "> [!IMPORTANT] Updated for commit `3b836a2391a4` \
(review generation 2)";
    assert!(body.contains(expected), "exact banner line: {body}");
    // Banner reads as the top of the comment: after the invisible
    // head marker, before the verdict heading.
    assert!(body.starts_with(REVIEW_MARKER), "marker still first");
    let banner_pos = body.find("[!IMPORTANT]").expect("banner present");
    let heading_pos = body.find("## Auto review").expect("heading present");
    assert!(banner_pos < heading_pos, "banner above the heading");
    assert_eq!(
        body.find("[!IMPORTANT]"),
        body.rfind("[!IMPORTANT]"),
        "exactly one banner"
    );
}

#[test]
fn banner_carries_the_generation_number() {
    let r = review(Verdict::Pass, "ok", vec![]);
    let body = render_sticky_comment(&gen_input(&r, SHA40, 3));
    assert!(
        body.contains("(review generation 3)"),
        "generation is part of the banner: {body}"
    );
}

#[test]
fn first_publication_generation_one_has_no_banner() {
    let r = review(Verdict::Pass, "ok", vec![]);
    let body = render_sticky_comment(&gen_input(&r, SHA40, 1));
    assert!(!body.contains("[!IMPORTANT]"), "no banner on gen 1");
    assert!(
        !body.contains("Updated for commit"),
        "no banner text on gen 1"
    );
}

#[test]
fn republish_banner_is_byte_identical_across_renders() {
    let r = review(
        Verdict::Fail,
        "problems",
        vec![finding(Severity::Blocking, "t", "b")],
    );
    let input = gen_input(&r, SHA40, 2);
    assert_eq!(render_sticky_comment(&input), render_sticky_comment(&input));
}

#[test]
fn banner_survives_truncation_under_overflow() {
    // 64 KiB summary alone overflows the budget: the banner is part
    // of the reserved header and must never be front-truncated.
    let summary = format!("Z{}", "s".repeat(64 * 1024));
    let r = review(Verdict::Pass, &summary, vec![]);
    let body = render_sticky_comment(&gen_input(&r, SHA40, 2));
    assert!(body.len() <= STICKY_COMMENT_MAX_BYTES, "under cap");
    assert!(body.starts_with(REVIEW_MARKER), "marker intact");
    assert!(
        body.contains("> [!IMPORTANT] Updated for commit `3b836a2391a4` (review generation 2)"),
        "banner intact despite overflow"
    );
    assert!(
        body.contains("truncated") || body.contains("Truncated"),
        "truncation note present"
    );
}

#[test]
fn banner_short_sha_is_twelve_chars_and_boundary_safe() {
    let r = review(Verdict::Pass, "ok", vec![]);
    // 40-char SHA → first 12 chars in the banner.
    let body = render_sticky_comment(&gen_input(&r, SHA40, 2));
    assert!(body.contains("`3b836a2391a4`"), "12-char short sha: {body}");
    // Short SHA stays whole.
    let body = render_sticky_comment(&gen_input(&r, "abc123", 2));
    assert!(
        body.contains("Updated for commit `abc123`"),
        "short sha unchanged when already short: {body}"
    );
}

//! Interactive-vs-plain rendering for the CLI's human surfaces (issue
//! #413), driven through the library's `display` module.
//!
//! The renderers live in two crates: `render_human` (status) and the two
//! queue renderers are library code (`src/daemon/status.rs`,
//! `src/daemon/display.rs`) and are called directly here; the review
//! renderers are binary-local (`src/cli/review.rs`) because their row view
//! types are, so their table shares this module's `render_table` — the
//! same function `src/cli/review.rs` calls — and is exercised with
//! review-shaped rows below. The binary's piped (plain-mode) bytes for all
//! five surfaces are pinned by the pre-existing subprocess suites
//! (`status_test`, `queue_show_test`, `review_cli_test`) plus
//! `plain_mode_zero_esc_bytes_every_surface` here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::Utc;

use caduceus::daemon::display::{self, DisplayStyle, TableCell};
use caduceus::daemon::status::{
    render_human, render_human_styled, BlockedEntry, DoctorStatus, LiveWorker, StatusReport,
};
use caduceus::meta::TickOutcome;
use caduceus::queue::{
    FinalizationCheckpoint, FinalizationStage, Phase, QueueEntry, QueueState, TicketType,
    QUEUE_FILE_VERSION,
};
use caduceus::IssueKey;

const TTY: DisplayStyle = DisplayStyle::INTERACTIVE;
const PLAIN: DisplayStyle = DisplayStyle::PLAIN;

fn key(owner: &str, repo: &str, number: u64) -> IssueKey {
    IssueKey {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    }
}

fn entry(
    k: &IssueKey,
    phase: Phase,
    last_error: Option<&str>,
    checkpoint: Option<FinalizationCheckpoint>,
) -> QueueEntry {
    QueueEntry {
        key: k.clone(),
        phase,
        ticket_type: TicketType::Code,
        attempts: 2,
        last_error: last_error.map(|text| text.to_string()),
        last_run_id: Some("RUN-1".to_string()),
        next_attempt_at: None,
        finalization: checkpoint,
        queued_at: Utc::now(),
        updated_at: Utc::now(),
        blocked_source: None,
        blocked_recovery_hint: None,
        generation: 1,
    }
}

/// Three entries spanning a failed, a healthy, and an attention phase.
fn seeded_queue() -> QueueState {
    let mut state = QueueState {
        version: QUEUE_FILE_VERSION,
        entries: BTreeMap::new(),
    };
    let alpha = key("Alpha", "Project", 1);
    let owner = key("owner", "repo", 42);
    let zed = key("zed", "repo", 2);
    state.entries.insert(
        alpha.display_key(),
        entry(&alpha, Phase::Failed, Some("boom"), None),
    );
    state.entries.insert(
        owner.display_key(),
        entry(
            &owner,
            Phase::Done,
            None,
            Some(FinalizationCheckpoint {
                run_id: "RUN-42".to_string(),
                branch_name: "automation/issue-42-run-1".to_string(),
                result_path: PathBuf::from("/state/runs/RUN-42.result.json"),
                stage: FinalizationStage::PrCreated,
                commit_oid: Some("abc123".to_string()),
                pr_number: Some(7),
                pr_url: Some("https://github.com/owner/repo/pull/7".to_string()),
            }),
        ),
    );
    state.entries.insert(
        zed.display_key(),
        entry(&zed, Phase::NeedsAttention, Some("dirty main"), None),
    );
    state
}

/// A report carrying a healthy phase, a failed phase, an attention phase,
/// a blocked issue with a long reason, and a long recent error.
fn sample_report() -> StatusReport {
    let long_reason = "main checkout is dirty at /home/agent/projects/zed/repo; refusing to operate until the operator commits or stashes the pending edits";
    let long_error = "worker supervision failure during result: commit_message exceeds limit of 256 bytes (got 425) — the run was discarded and the entry requeued";
    StatusReport {
        version: "7.7.0".to_string(),
        state_dir: PathBuf::from("/state"),
        last_tick_started: Some(Utc::now()),
        last_tick_finished: Some(Utc::now()),
        last_outcome: Some(TickOutcome::Processed),
        last_http_status: Some(200),
        next_allowed_poll_at: Some(Utc::now()),
        phases: BTreeMap::from([
            ("awaiting_review".to_string(), 0),
            ("done".to_string(), 1),
            ("failed".to_string(), 2),
            ("needs_attention".to_string(), 1),
            ("queued".to_string(), 0),
        ]),
        next_head: Some("alpha/project#1".to_string()),
        next_head_earliest_eligibility: None,
        recent_errors: vec![long_error.to_string()],
        blocked_issues: vec![BlockedEntry {
            issue_key: "zed/repo#2".to_string(),
            blocked_source: "worktree/dirty_main".to_string(),
            blocked_recovery_hint: "caduceus queue reset zed/repo#2 --force-finalization-reset"
                .to_string(),
            last_error: long_reason.to_string(),
        }],
        rate_limit: None,
        live_workers: vec![LiveWorker {
            run_id: "01J8RUN".to_string(),
            issue: "owner/repo#42".to_string(),
            pid: 4242,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            transcript_path: PathBuf::from("/state/runs/01J8RUN.log"),
            freshness: "stale".to_string(),
        }],
        diagnostics: Vec::new(),
        state_corrupt: false,
        readiness: None,
        pool_state: Some("active(1)".to_string()),
        doctor: Some(DoctorStatus {
            verdict: "READY".to_string(),
            generated_at: Utc::now(),
            informational: true,
            failed_checks: Vec::new(),
        }),
    }
}

/// The review-list table the binary builds, in plain mode.
fn review_rows() -> Vec<Vec<TableCell>> {
    vec![
        vec![
            TableCell::plain("owner/repo#42@aaaaaaaaaaaa"),
            TableCell::status(display::phase_color("done"), "done"),
            TableCell::plain("0"),
            TableCell::plain("1"),
            TableCell::status(display::verdict_color("fail"), "fail"),
            TableCell::plain("published"),
            TableCell::plain("RUN-B"),
        ],
        vec![
            TableCell::plain("owner/repo#43@bbbbbbbbbbbb"),
            TableCell::status(display::phase_color("failed"), "failed"),
            TableCell::plain("3"),
            TableCell::plain("2"),
            TableCell::status(display::verdict_color("pass"), "pass"),
            TableCell::plain("pending"),
            TableCell::plain("-"),
        ],
    ]
}

const REVIEW_HEADERS: [&str; 7] = [
    "key",
    "phase",
    "attempts",
    "generation",
    "verdict",
    "publication",
    "run_id",
];

// ---------------------------------------------------------------------------
// queue show: list table
// ---------------------------------------------------------------------------

#[test]
fn queue_table_plain_bytes_verbatim() {
    let out = display::render_queue_table(&seeded_queue(), PLAIN, 80);
    let mut lines = out.lines();
    assert_eq!(
        lines.next(),
        Some("key\tphase\tticket\tattempts\tgeneration\tage")
    );
    // Lexical BTreeMap order, lowercase display keys, tab separators.
    let rows: Vec<&str> = lines.collect();
    assert_eq!(rows.len(), 3);
    assert!(rows[0].starts_with("alpha/project#1\tfailed\tcode\t2\t1\t"));
    assert!(rows[0].ends_with('s'));
    assert!(rows[1].starts_with("owner/repo#42\tdone\tcode\t2\t1\t"));
    assert!(rows[2].starts_with("zed/repo#2\tneeds_attention\tcode\t2\t1\t"));
    assert!(!out.contains('\x1b'), "plain table emitted SGR: {out}");
}

#[test]
fn queue_table_interactive_no_tabs_and_words_preserved() {
    let out = display::render_queue_table(&seeded_queue(), TTY, 120);
    assert!(!out.contains('\t'), "interactive table kept tabs: {out}");
    // The header is bold and every plain-mode word survives.
    let header = out.lines().next().expect("header line");
    assert!(
        header.starts_with(display::BOLD),
        "header not bold: {header:?}"
    );
    assert!(header.ends_with(display::RESET));
    for word in [
        "key",
        "phase",
        "ticket",
        "attempts",
        "generation",
        "age",
        "alpha/project#1",
        "needs_attention",
        "owner/repo#42",
    ] {
        assert!(out.contains(word), "missing {word:?} in {out}");
    }
    // Columns are aligned: the phase cell starts at the same offset on
    // every row.
    let offsets: Vec<usize> = out
        .lines()
        .skip(1)
        .map(|line| {
            line.find("failed")
                .or_else(|| line.find("done"))
                .or_else(|| line.find("needs_attention"))
                .expect("phase cell")
        })
        .collect();
    assert_eq!(offsets.len(), 3);
    assert!(
        offsets.iter().all(|offset| *offset == offsets[0]),
        "phase column not aligned: {offsets:?}\n{out}"
    );
}

#[test]
fn queue_table_interactive_colors_failed_red() {
    let out = display::render_queue_table(&seeded_queue(), TTY, 120);
    assert!(
        out.contains("\x1b[31mfailed"),
        "failed phase cell not red: {out}"
    );
    assert!(out.contains("\x1b[32mdone"), "done phase cell not green");
    assert!(
        out.contains("\x1b[33mneeds_attention"),
        "needs_attention phase cell not yellow"
    );
}

// ---------------------------------------------------------------------------
// queue show: detail
// ---------------------------------------------------------------------------

#[test]
fn queue_detail_plain_bytes_verbatim() {
    let state = seeded_queue();
    let entry = state
        .entries
        .get("owner/repo#42")
        .expect("seeded owner/repo#42");
    let out = display::render_queue_entry_detail(entry, PLAIN, 80);
    assert!(out.starts_with("entry owner/repo#42\n"));
    assert!(out.contains("  phase: done\n"));
    assert!(out.contains("  ticket_type: code\n"));
    assert!(out.contains("  attempts: 2\n"));
    assert!(out.contains("  last_error: None\n"));
    assert!(out.contains("  finalization:\n"));
    assert!(out.contains("    stage: pr_created\n"));
    assert!(out.contains("    pr_number: Some(7)\n"));
    assert!(!out.contains('\x1b'), "plain detail emitted SGR: {out}");
}

#[test]
fn queue_detail_interactive_colors_last_error() {
    let state = seeded_queue();
    let entry = state
        .entries
        .get("alpha/project#1")
        .expect("seeded alpha/project#1");
    let out = display::render_queue_entry_detail(entry, TTY, 200);
    assert!(
        out.contains("  last_error: \x1b[31mSome(\"boom\")\x1b[0m"),
        "last_error not red: {out}"
    );
    assert!(
        out.contains("  phase: \x1b[31mfailed\x1b[0m"),
        "phase not red: {out}"
    );
    // Labels are never coloured.
    assert!(out.contains("  phase: \x1b[31m"));
    assert!(!out.contains("\x1b[31mphase"));
}

// ---------------------------------------------------------------------------
// review list (the binary's table, driven through the shared renderer)
// ---------------------------------------------------------------------------

#[test]
fn review_list_plain_bytes_verbatim() {
    let out = display::render_table(PLAIN, 80, &REVIEW_HEADERS, &review_rows());
    assert_eq!(
        out,
        "key\tphase\tattempts\tgeneration\tverdict\tpublication\trun_id\n\
         owner/repo#42@aaaaaaaaaaaa\tdone\t0\t1\tfail\tpublished\tRUN-B\n\
         owner/repo#43@bbbbbbbbbbbb\tfailed\t3\t2\tpass\tpending\t-\n"
    );
}

#[test]
fn review_list_interactive_colors_verdicts() {
    let out = display::render_table(TTY, 200, &REVIEW_HEADERS, &review_rows());
    assert!(!out.contains('\t'), "interactive table kept tabs: {out}");
    assert!(out.contains("\x1b[31mfail"), "fail verdict not red: {out}");
    assert!(
        out.contains("\x1b[32mpass"),
        "pass verdict not green: {out}"
    );
    // The verdict cell keeps its text; only SGR wraps it.
    assert!(out.contains("fail"));
    assert!(out.contains("pass"));
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[test]
fn status_render_interactive_colors_phase_counts() {
    let report = sample_report();
    let out = render_human_styled(&report, None, TTY, 200);
    assert!(out.contains("  phases:\n"), "phase header changed: {out}");
    // Headers stay uncoloured; only the counts are painted.
    assert!(!out.contains("\x1b[32mphases"));
    assert!(
        out.contains("\x1b[32m1\x1b[0m"),
        "done count not green: {out}"
    );
    assert!(
        out.contains("\x1b[31m2\x1b[0m"),
        "failed count not red: {out}"
    );
    assert!(
        out.contains("\x1b[33m1\x1b[0m"),
        "needs_attention count not yellow: {out}"
    );
    // Every plain label survives verbatim (glyph prefixes aside).
    let stripped = strip_sgr(&out);
    for label in [
        "caduceus status",
        "  state dir: /state",
        "  phases:",
        "    queued: 0",
        "  next head: alpha/project#1",
        "  live workers: 1",
        "  blocked issues:",
        "      source: worktree/dirty_main",
        "      recovery: caduceus queue reset zed/repo#2 --force-finalization-reset",
        "  recent errors:",
    ] {
        assert!(stripped.contains(label), "missing {label:?} in\n{stripped}");
    }
    // Status-bearing phase rows carry a glyph prefix in interactive mode.
    assert!(
        stripped.contains("✓ done: 1"),
        "missing glyph row:\n{stripped}"
    );
    assert!(
        stripped.contains("✗ failed: 2"),
        "missing glyph row:\n{stripped}"
    );
    assert!(
        stripped.contains("! needs_attention: 1"),
        "missing glyph row:\n{stripped}"
    );
}

#[test]
fn status_render_interactive_wraps_long_reason() {
    let report = sample_report();
    let out = render_human_styled(&report, None, TTY, 60);
    let mut wrapped_lines = 0;
    for line in out.lines() {
        assert!(
            display::visible_len(line) <= 60,
            "line over the 60-column budget ({}): {line:?}",
            display::visible_len(line)
        );
        if line.trim_start().starts_with('/') || line.contains("requeued") {
            wrapped_lines += 1;
        }
    }
    assert!(wrapped_lines > 0, "nothing wrapped at width 60:\n{out}");
    // Continuation lines hang under their value column (14 columns for
    // `      reason: `).
    assert!(
        out.lines()
            .any(|line| line.starts_with("              ") && !line.trim().is_empty()),
        "no hanging continuation line:\n{out}"
    );
    // Nothing was truncated: the reason text survives in full.
    let stripped = strip_sgr(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(stripped.contains("refusing to operate until the operator commits"));
}

#[test]
fn plain_mode_zero_esc_bytes_every_surface() {
    let report = sample_report();
    let state = seeded_queue();
    let detail = state.entries.get("alpha/project#1").expect("seeded");
    let surfaces = [
        render_human_styled(&report, None, PLAIN, 80),
        render_human(&report, None),
        display::render_queue_table(&state, PLAIN, 80),
        display::render_queue_entry_detail(detail, PLAIN, 80),
        display::render_table(PLAIN, 80, &REVIEW_HEADERS, &review_rows()),
    ];
    for surface in surfaces {
        assert!(
            !surface.contains('\x1b'),
            "plain surface emitted SGR: {surface}"
        );
        for glyph in ["✓", "✗"] {
            assert!(
                !surface.contains(glyph),
                "plain surface emitted the {glyph} glyph: {surface}"
            );
        }
        // ... and no plain surface wraps: every line is the logical line.
        assert!(surface.contains('\n'));
    }
}

#[test]
fn status_plain_styled_matches_render_human() {
    // `render_human` resolves the style from stdout (a pipe under the
    // harness), so it must equal the explicit plain rendering byte for
    // byte — the plain-mode contract for the status surface.
    let report = sample_report();
    assert_eq!(
        render_human(&report, None),
        render_human_styled(&report, None, PLAIN, 80)
    );
}

#[test]
fn status_interactive_keeps_ops_skill_labels() {
    // The caduceus-daemon-ops skill greps these labels from the human
    // output; interactive mode must not rename or re-shape any of them.
    let report = sample_report();
    let stripped = strip_sgr(&render_human_styled(&report, None, TTY, 120));
    for label in [
        "phases:",
        "queued:",
        "live workers:",
        "blocked issues:",
        "next head:",
    ] {
        assert!(stripped.contains(label), "missing {label:?} in\n{stripped}");
    }
    // And the plain rendering (what the ops skill actually greps) is
    // unaffected by the new code path.
    let plain = render_human_styled(&report, None, PLAIN, 80);
    for label in [
        "phases:",
        "queued:",
        "live workers:",
        "blocked issues:",
        "next head:",
    ] {
        assert!(plain.contains(label), "missing {label:?} in\n{plain}");
    }
}

/// Drop SGR sequences so a coloured line can be compared with its plain
/// text.
fn strip_sgr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut iter = text.chars().peekable();
    while let Some(ch) = iter.next() {
        if ch == '\x1b' && iter.peek() == Some(&'[') {
            for next in iter.by_ref() {
                if next == 'm' {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

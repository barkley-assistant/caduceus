//! Shared display policy for the CLI's human-facing renderers.
//!
//! Two rendering modes, chosen by one gate ([`detect_style`]):
//!
//! * **plain** — the legacy byte-for-byte text. No SGR sequences, no
//!   Unicode glyphs, no wrapping, and table rows stay tab-separated.
//!   This is what piped, CI, and `NO_COLOR` runs see, so the
//!   `caduceus-daemon-ops` greps (`phases:`, `queued:`, `live workers:`,
//!   `blocked issues:`, `next head:`) and every `--json` envelope keep
//!   working unchanged.
//! * **interactive** — 16-colour ANSI on the status-bearing words,
//!   Unicode verdict glyphs, aligned two-space-gutter tables, and
//!   width-aware wrapping that never truncates.
//!
//! Labels, keys, values, and line shapes are identical in BOTH modes:
//! interactive mode only adds SGR wrappers, a leading glyph, alignment
//! padding, and line breaks. The policy mirrors the wrapper's
//! `_display.py` (issue #412) so the binary and the Hermes plugin doctor
//! behave the same way.
//!
//! The two queue-surface renderers live here (not in the binary crate's
//! `src/cli/`) so their interactive branches are reachable from the
//! integration tests under `tests/`; the review renderers stay in
//! `src/cli/review.rs` because their row view types are binary-local.

use std::io::IsTerminal;

use chrono::Utc;
use terminal_size::{terminal_size, Width};

use crate::state::queue::{QueueEntry, QueueState, TicketType};

/// Terminal width used when the probe fails (no TTY, degenerate 0).
pub const FALLBACK_WIDTH: usize = 80;

/// Below this much room for text after the hanging indent, wrapping
/// cannot improve anything, so the line is emitted unwrapped. Mirrors
/// `_display.py::_MIN_WRAP_TEXT_WIDTH`.
const MIN_WRAP_TEXT_WIDTH: usize = 12;

/// Two-space gutter between aligned table columns.
const COLUMN_GUTTER: &str = "  ";

/// Bold SGR — table headers.
pub const BOLD: &str = "\x1b[1m";
/// Green SGR — healthy / passing status words.
pub const GREEN: &str = "\x1b[32m";
/// Red SGR — failed status words.
pub const RED: &str = "\x1b[31m";
/// Yellow SGR — attention status words.
pub const YELLOW: &str = "\x1b[33m";
/// Bold red SGR — the corrupt-state banner.
pub const BOLD_RED: &str = "\x1b[1m\x1b[31m";
/// Reset SGR.
pub const RESET: &str = "\x1b[0m";

/// One status vocabulary row: the interactive glyph, the ASCII fallback
/// used where a marker is required in both modes, and the SGR colour.
///
/// Mirrors `_display.py::_STATUS_VOCAB` (issue #412): the wrapper doctor
/// and this CLI share one presentation policy. The five renderers that
/// fall under the plain-mode byte contract emit no marker in plain mode —
/// the status word itself (`pass`, `fail`, `failed`, `needs_attention`)
/// is their ASCII fallback, so they use [`ok_glyph`]/[`fail_glyph`]/
/// [`warn_glyph`], while a surface that needs a marker in both modes uses
/// [`Marker::render`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Marker {
    /// Interactive glyph.
    pub glyph: &'static str,
    /// ASCII fallback marker.
    pub ascii: &'static str,
    /// SGR colour for the marker's status word.
    pub color: &'static str,
}

impl Marker {
    /// The glyph on an interactive stream, the ASCII fallback otherwise.
    pub fn render(self, style: DisplayStyle) -> &'static str {
        if style.interactive {
            self.glyph
        } else {
            self.ascii
        }
    }
}

/// Healthy marker (`✓` / `[OK]`).
pub const OK_MARKER: Marker = Marker {
    glyph: "✓",
    ascii: "[OK]",
    color: GREEN,
};
/// Failed marker (`✗` / `[FAIL]`).
pub const FAIL_MARKER: Marker = Marker {
    glyph: "✗",
    ascii: "[FAIL]",
    color: RED,
};
/// Attention marker (`!` / `[WARN]`).
pub const WARN_MARKER: Marker = Marker {
    glyph: "!",
    ascii: "[WARN]",
    color: YELLOW,
};

/// Whether the human renderers may use ANSI colour, Unicode glyphs,
/// alignment padding, and width-aware wrapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayStyle {
    /// `true` when stdout is an interactive terminal and neither
    /// `TERM=dumb` nor `NO_COLOR` opts out.
    pub interactive: bool,
}

impl DisplayStyle {
    /// Plain mode: the legacy bytes, no SGR, no glyphs.
    pub const PLAIN: DisplayStyle = DisplayStyle { interactive: false };
    /// Interactive mode: colour, glyphs, alignment, wrapping.
    pub const INTERACTIVE: DisplayStyle = DisplayStyle { interactive: true };
}

/// The gate itself, with the environment passed in.
///
/// Interactive only when every one of these holds: stdout is a TTY,
/// `TERM` is not the literal `dumb` (an unset `TERM` on a real TTY is
/// treated as capable), and `NO_COLOR` is absent — `NO_COLOR=""` counts as
/// set, so "unset" is literal. Split out from [`detect_style`] so the
/// TTY-true branches are unit-testable without a pseudo-terminal.
pub fn style_from(tty: bool, term: Option<&str>, no_color: bool) -> DisplayStyle {
    let dumb = term.is_some_and(|value| value.eq_ignore_ascii_case("dumb"));
    DisplayStyle {
        interactive: tty && !dumb && !no_color,
    }
}

/// Resolve the display style for stdout.
pub fn detect_style() -> DisplayStyle {
    let term = std::env::var("TERM").ok();
    style_from(
        std::io::stdout().is_terminal(),
        term.as_deref(),
        std::env::var_os("NO_COLOR").is_some(),
    )
}

/// Terminal width in columns, falling back to [`FALLBACK_WIDTH`] when the
/// probe fails (`terminal_size` returns `None` off a TTY) or reports a
/// degenerate zero.
///
/// `libc`'s `ioctl` would need `unsafe`, which the crate forbids; the
/// `terminal_size` probe is the safe equivalent and is already in the clap
/// dependency tree, so naming it as a direct dependency adds no crate to
/// the build.
pub fn terminal_width() -> usize {
    match terminal_size() {
        Some((Width(columns), _)) if columns > 0 => usize::from(columns),
        _ => FALLBACK_WIDTH,
    }
}

/// Visible width of `text`: its character count minus SGR escape
/// sequences, so a coloured fragment measures the same as its plain text.
pub fn visible_len(text: &str) -> usize {
    let mut visible = 0;
    let mut iter = text.chars().peekable();
    while let Some(ch) = iter.next() {
        if ch == '\x1b' && iter.peek() == Some(&'[') {
            // Consume the CSI introducer and the sequence body; our own
            // sequences always end in `m` (SGR).
            for next in iter.by_ref() {
                if next == 'm' {
                    break;
                }
            }
            continue;
        }
        visible += 1;
    }
    visible
}

/// Wrap `text` in `color` iff interactive; plain mode returns `text`
/// unchanged, so a plain render never emits an ESC byte.
pub fn paint(style: DisplayStyle, color: &str, text: &str) -> String {
    if style.interactive {
        format!("{color}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// [`paint`] for an optional colour (uncoloured phases, unknown verdicts).
pub fn paint_opt(style: DisplayStyle, color: Option<&str>, text: &str) -> String {
    match color {
        Some(color) => paint(style, color, text),
        None => text.to_string(),
    }
}

/// Interactive glyph for a healthy row, empty in plain mode.
pub fn ok_glyph(style: DisplayStyle) -> &'static str {
    if style.interactive {
        OK_MARKER.glyph
    } else {
        ""
    }
}

/// Interactive glyph for a failed row, empty in plain mode.
pub fn fail_glyph(style: DisplayStyle) -> &'static str {
    if style.interactive {
        FAIL_MARKER.glyph
    } else {
        ""
    }
}

/// Interactive glyph for an attention row, empty in plain mode.
pub fn warn_glyph(style: DisplayStyle) -> &'static str {
    if style.interactive {
        WARN_MARKER.glyph
    } else {
        ""
    }
}

/// Prefix a status word with its glyph, or leave it bare when there is no
/// glyph (always the case in plain mode).
pub fn with_glyph(glyph: &str, word: &str) -> String {
    if glyph.is_empty() {
        word.to_string()
    } else {
        format!("{glyph} {word}")
    }
}

/// SGR colour for a stable phase label: `done` green, `failed` red,
/// `needs_attention` yellow, everything else uncoloured.
pub fn phase_color(label: &str) -> Option<&'static str> {
    match label {
        "done" => Some(GREEN),
        "failed" => Some(RED),
        "needs_attention" => Some(YELLOW),
        _ => None,
    }
}

/// Interactive glyph for a stable phase label.
pub fn phase_glyph(style: DisplayStyle, label: &str) -> &'static str {
    match label {
        "done" => ok_glyph(style),
        "failed" => fail_glyph(style),
        "needs_attention" => warn_glyph(style),
        _ => "",
    }
}

/// SGR colour for a review verdict word: `pass`/`passed` green,
/// `fail`/`failed` red, anything else (including `-`) uncoloured.
pub fn verdict_color(label: &str) -> Option<&'static str> {
    match label.to_ascii_lowercase().as_str() {
        "pass" | "passed" => Some(GREEN),
        "fail" | "failed" => Some(RED),
        _ => None,
    }
}

/// Interactive glyph for a review verdict word.
pub fn verdict_glyph(style: DisplayStyle, label: &str) -> &'static str {
    match label.to_ascii_lowercase().as_str() {
        "pass" | "passed" => ok_glyph(style),
        "fail" | "failed" => fail_glyph(style),
        _ => "",
    }
}

/// Stable snake_case ticket-type label (independent of the serde rename
/// attribute on [`TicketType`]).
pub fn ticket_type_label(ticket_type: TicketType) -> &'static str {
    match ticket_type {
        TicketType::Code => "code",
        TicketType::Investigation => "investigation",
    }
}

/// Pad `text` to `width` columns with spaces. Never truncates: a cell
/// already at or over `width` is returned unchanged.
pub fn pad_cell(text: &str, width: usize) -> String {
    let visible = visible_len(text);
    if visible >= width {
        return text.to_string();
    }
    let mut padded = String::with_capacity(text.len() + (width - visible));
    padded.push_str(text);
    padded.push_str(&" ".repeat(width - visible));
    padded
}

/// Wrap `text` to `width` columns with continuation lines indented `hang`
/// columns. Returns `text` unchanged when it fits on one line.
///
/// Never truncates and never splits a token: a token longer than the
/// remaining budget starts its own line and overflows rather than losing
/// bytes, and hyphenated tokens (`worker-bridge.py`) stay intact. A
/// degenerate `width < hang + 12` (a terminal narrower than its own
/// indent) also returns the input unchanged. SGR sequences do not count
/// towards the budget.
pub fn wrap_hanging(text: &str, width: usize, hang: usize) -> String {
    if width < hang + MIN_WRAP_TEXT_WIDTH {
        return text.to_string();
    }
    // Preserve the caller's own indentation on the first line: wrapping
    // only collapses inter-word runs, never the leading indent.
    let trimmed = text.trim_start_matches(' ');
    let lead = &text[..text.len() - trimmed.len()];
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.is_empty() {
        return text.to_string();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::from(lead);
    let mut has_word = false;
    let mut budget = width;
    for word in words {
        let candidate = if has_word {
            visible_len(&current) + 1 + visible_len(word)
        } else {
            visible_len(&current) + visible_len(word)
        };
        if has_word && candidate > budget {
            lines.push(current);
            current = String::new();
            budget = width.saturating_sub(hang);
            has_word = false;
        }
        if has_word {
            current.push(' ');
        }
        current.push_str(word);
        has_word = true;
    }
    if has_word {
        lines.push(current);
    }
    if lines.len() <= 1 {
        return text.to_string();
    }
    let indent = " ".repeat(hang);
    lines.join(&format!("\n{indent}"))
}

/// Emit one logical line.
///
/// Plain mode writes `line` verbatim (it carries no SGR bytes there) and
/// never wraps. Interactive mode writes it as-is when it fits `width`,
/// and otherwise wraps it with a hanging indent of `hang`; a value that
/// already contains newlines is never re-flowed.
pub fn push_row_line(out: &mut String, style: DisplayStyle, width: usize, hang: usize, line: &str) {
    if !style.interactive || line.contains('\n') || visible_len(line) <= width {
        out.push_str(line);
        out.push('\n');
        return;
    }
    out.push_str(&wrap_hanging(line, width, hang));
    out.push('\n');
}

/// Emit one `"<indent><label>: <value>"` line, wrapping the whole line
/// with a hanging indent under the value column when it overflows.
pub fn push_labelled(
    out: &mut String,
    style: DisplayStyle,
    width: usize,
    indent: usize,
    label: &str,
    value: &str,
    color: Option<&str>,
) {
    let prefix = format!("{}{}: ", " ".repeat(indent), label);
    let line = format!("{prefix}{}", paint_opt(style, color, value));
    push_row_line(out, style, width, visible_len(&prefix), &line);
}

/// One table cell: the text plus the optional SGR colour for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableCell {
    /// Cell text — always the plain-mode bytes.
    pub text: String,
    /// Colour applied in interactive mode only.
    pub color: Option<&'static str>,
}

impl TableCell {
    /// An uncoloured cell.
    pub fn plain(text: impl Into<String>) -> TableCell {
        TableCell {
            text: text.into(),
            color: None,
        }
    }

    /// A cell whose text is coloured in interactive mode.
    pub fn colored(color: &'static str, text: impl Into<String>) -> TableCell {
        TableCell {
            text: text.into(),
            color: Some(color),
        }
    }

    /// A cell coloured by a status label (uncoloured when the label has
    /// no colour of its own).
    pub fn status(color: Option<&'static str>, text: impl Into<String>) -> TableCell {
        TableCell {
            text: text.into(),
            color,
        }
    }
}

/// Render a table.
///
/// Plain mode is the tab-separated bytes the CLI has always printed
/// (header row then one row per entry, `\t` joined). Interactive mode pads
/// every cell to its column width, separates columns with a two-space
/// gutter, bolds the header, applies each cell's colour, and — when the
/// aligned row would overflow `width` — wraps the FIRST column (the issue
/// key in both tables) across lines, leaving the other columns on the
/// first line. Columns are never shrunk and cells are never truncated.
pub fn render_table(
    style: DisplayStyle,
    width: usize,
    headers: &[&str],
    rows: &[Vec<TableCell>],
) -> String {
    let mut out = String::new();
    if !style.interactive {
        out.push_str(&headers.join("\t"));
        out.push('\n');
        for row in rows {
            let cells: Vec<&str> = row.iter().map(|cell| cell.text.as_str()).collect();
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
        return out;
    }

    let columns = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|header| visible_len(header)).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(slot) = widths.get_mut(index) {
                *slot = (*slot).max(visible_len(&cell.text));
            }
        }
    }

    let gutter_total = COLUMN_GUTTER.len() * columns.saturating_sub(1);
    let total: usize = widths.iter().sum::<usize>() + gutter_total;
    let key_budget = key_budget(width, total, &widths, gutter_total, columns);

    let header_cells: Vec<String> = headers.iter().map(|header| (*header).to_string()).collect();
    let no_colors: Vec<Option<&str>> = Vec::new();
    out.push_str(&paint(
        style,
        BOLD,
        &table_line(style, &header_cells, &widths, &no_colors),
    ));
    out.push('\n');

    for row in rows {
        let texts: Vec<String> = row.iter().map(|cell| cell.text.clone()).collect();
        let colors: Vec<Option<&str>> = row.iter().map(|cell| cell.color).collect();
        let first = texts.first().map(String::as_str).unwrap_or("");
        let segments = match key_budget {
            Some(budget) => wrap_hanging(first, budget, 0),
            None => first.to_string(),
        };
        for (index, segment) in segments.split('\n').enumerate() {
            let (cells, line_colors) = if index == 0 {
                (texts.clone(), colors.clone())
            } else {
                (
                    vec![segment.to_string()],
                    vec![colors.first().copied().flatten()],
                )
            };
            out.push_str(&table_line(style, &cells, &widths, &line_colors));
            out.push('\n');
        }
    }
    out
}

/// Budget for wrapping the first column, or `None` when the aligned table
/// already fits `width`.
fn key_budget(
    width: usize,
    total: usize,
    widths: &[usize],
    gutter_total: usize,
    columns: usize,
) -> Option<usize> {
    if total <= width || columns < 2 {
        return None;
    }
    let others: usize = widths[1..].iter().sum::<usize>() + gutter_total;
    Some(width.saturating_sub(others).max(MIN_WRAP_TEXT_WIDTH))
}

/// Join `cells` into one aligned row (last column unpadded), applying each
/// cell's colour.
fn table_line(
    style: DisplayStyle,
    cells: &[String],
    widths: &[usize],
    colors: &[Option<&str>],
) -> String {
    let last = cells.len().saturating_sub(1);
    let mut rendered: Vec<String> = Vec::with_capacity(cells.len());
    for (index, cell) in cells.iter().enumerate() {
        let padded = if index == last {
            cell.clone()
        } else {
            pad_cell(cell, widths.get(index).copied().unwrap_or(0))
        };
        rendered.push(paint_opt(
            style,
            colors.get(index).copied().flatten(),
            &padded,
        ));
    }
    rendered.join(COLUMN_GUTTER)
}

/// Render the human list table for `queue show`: columns key, phase,
/// ticket type, attempts, generation, and age (seconds since
/// `updated_at`), in `BTreeMap` lexical order.
pub fn render_queue_table(state: &QueueState, style: DisplayStyle, width: usize) -> String {
    if state.entries.is_empty() {
        return "queue: no entries".to_string();
    }
    let now = Utc::now();
    let headers = ["key", "phase", "ticket", "attempts", "generation", "age"];
    let mut rows: Vec<Vec<TableCell>> = Vec::with_capacity(state.entries.len());
    for entry in state.entries.values() {
        let age = (now - entry.updated_at).num_seconds().max(0);
        let phase = entry.phase.as_str();
        rows.push(vec![
            TableCell::plain(entry.key.display_key()),
            TableCell::status(phase_color(phase), phase),
            TableCell::plain(ticket_type_label(entry.ticket_type)),
            TableCell::plain(entry.attempts.to_string()),
            TableCell::plain(entry.generation.to_string()),
            TableCell::plain(format!("{age}s")),
        ]);
    }
    render_table(style, width, &headers, &rows)
}

/// Render the human detail view for `queue show <key>`, including the
/// finalization checkpoint (branch, run id, stage, PR).
///
/// Plain mode reproduces the legacy bytes verbatim: [`push_labelled`]
/// adds nothing when the style is not interactive.
pub fn render_queue_entry_detail(entry: &QueueEntry, style: DisplayStyle, width: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!("entry {}\n", entry.key.display_key()));
    push_labelled(
        &mut out,
        style,
        width,
        2,
        "phase",
        entry.phase.as_str(),
        phase_color(entry.phase.as_str()),
    );
    push_labelled(
        &mut out,
        style,
        width,
        2,
        "ticket_type",
        ticket_type_label(entry.ticket_type),
        None,
    );
    push_labelled(
        &mut out,
        style,
        width,
        2,
        "attempts",
        &entry.attempts.to_string(),
        None,
    );
    let last_error = format!("{:?}", entry.last_error);
    push_labelled(
        &mut out,
        style,
        width,
        2,
        "last_error",
        &last_error,
        entry.last_error.as_ref().map(|_| RED),
    );
    push_labelled(
        &mut out,
        style,
        width,
        2,
        "last_run_id",
        &format!("{:?}", entry.last_run_id),
        None,
    );
    push_labelled(
        &mut out,
        style,
        width,
        2,
        "next_attempt_at",
        &format!("{:?}", entry.next_attempt_at),
        None,
    );
    push_labelled(
        &mut out,
        style,
        width,
        2,
        "queued_at",
        &entry.queued_at.to_rfc3339(),
        None,
    );
    push_labelled(
        &mut out,
        style,
        width,
        2,
        "updated_at",
        &entry.updated_at.to_rfc3339(),
        None,
    );
    push_labelled(
        &mut out,
        style,
        width,
        2,
        "generation",
        &entry.generation.to_string(),
        None,
    );
    push_labelled(
        &mut out,
        style,
        width,
        2,
        "blocked_source",
        &format!("{:?}", entry.blocked_source),
        None,
    );
    push_labelled(
        &mut out,
        style,
        width,
        2,
        "blocked_recovery_hint",
        &format!("{:?}", entry.blocked_recovery_hint),
        None,
    );
    match entry.finalization.as_ref() {
        Some(check) => {
            out.push_str("  finalization:\n");
            push_labelled(&mut out, style, width, 4, "run_id", &check.run_id, None);
            push_labelled(
                &mut out,
                style,
                width,
                4,
                "branch_name",
                &check.branch_name,
                None,
            );
            push_labelled(
                &mut out,
                style,
                width,
                4,
                "result_path",
                &check.result_path.display().to_string(),
                None,
            );
            push_labelled(
                &mut out,
                style,
                width,
                4,
                "stage",
                check.stage.as_str(),
                None,
            );
            push_labelled(
                &mut out,
                style,
                width,
                4,
                "commit_oid",
                &format!("{:?}", check.commit_oid),
                None,
            );
            push_labelled(
                &mut out,
                style,
                width,
                4,
                "pr_number",
                &format!("{:?}", check.pr_number),
                None,
            );
            push_labelled(
                &mut out,
                style,
                width,
                4,
                "pr_url",
                &format!("{:?}", check.pr_url),
                None,
            );
        }
        None => out.push_str("  finalization: none\n"),
    }
    out
}

//! `src/daemon/display.rs` — the shared display policy (issue #413).
//!
//! Hermetic: no pseudo-terminal. The TTY-true half of the gate is pinned
//! through [`style_from`] (the pure gate `detect_style` delegates to) and
//! through the interactive renderers in `display_render_test.rs`, which
//! take an explicit `DisplayStyle`. The TTY-false half is the real
//! environment: under the test harness stdout is a pipe.

use caduceus::daemon::display::{
    self, style_from, terminal_width, DisplayStyle, FAIL_MARKER, FALLBACK_WIDTH, GREEN, OK_MARKER,
    RED, WARN_MARKER, YELLOW,
};

/// A TTY with a capable `TERM` and no `NO_COLOR`.
const TTY: DisplayStyle = DisplayStyle::INTERACTIVE;
/// Piped / `NO_COLOR` / `TERM=dumb`.
const PLAIN: DisplayStyle = DisplayStyle::PLAIN;

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

#[test]
fn detect_style_plain_when_not_tty() {
    // The harness pipes stdout, so the TTY condition fails regardless of
    // the host environment (AC-2 baseline).
    assert!(
        !display::detect_style().interactive,
        "piped stdout must resolve to plain mode"
    );
}

#[test]
#[serial_test::serial]
fn detect_style_requires_tty_env_term_and_no_color() {
    // Capable TERM, no NO_COLOR — but stdout is still a pipe, so the TTY
    // check is real and not an environment-only test.
    temp_env::with_vars(
        [("TERM", Some("xterm-256color")), ("NO_COLOR", None::<&str>)],
        || {
            assert!(!display::detect_style().interactive);
        },
    );
}

#[test]
fn detect_style_term_dumb_forces_plain() {
    assert!(!style_from(true, Some("dumb"), false).interactive);
    // Case-insensitive, so a `TERM=DUMB` spelling opts out too.
    assert!(!style_from(true, Some("DUMB"), false).interactive);
}

#[test]
fn detect_style_no_color_variants_force_plain() {
    // `NO_COLOR=""` counts as set: presence, not value.
    assert!(!style_from(true, Some("xterm-256color"), true).interactive);
}

#[test]
fn detect_style_interactive_when_every_condition_holds() {
    assert!(style_from(true, Some("xterm-256color"), false).interactive);
    // An unset TERM on a real TTY is treated as capable.
    assert!(style_from(true, None, false).interactive);
}

// ---------------------------------------------------------------------------
// Colour, glyphs, markers
// ---------------------------------------------------------------------------

#[test]
fn paint_plain_mode_returns_text_unchanged() {
    let painted = display::paint(PLAIN, RED, "boom");
    assert_eq!(painted, "boom");
    assert!(!painted.contains('\x1b'));
}

#[test]
fn paint_interactive_wraps_in_sgr() {
    assert_eq!(display::paint(TTY, RED, "boom"), "\x1b[31mboom\x1b[0m");
    assert_eq!(display::paint(TTY, GREEN, "ok"), "\x1b[32mok\x1b[0m");
    assert_eq!(display::paint(TTY, YELLOW, "warn"), "\x1b[33mwarn\x1b[0m");
    // 16-colour SGR only: no truecolor / 256-colour sequences.
    for painted in [
        display::paint(TTY, RED, "x"),
        display::paint(TTY, GREEN, "x"),
    ] {
        assert!(
            !painted.contains("38;5"),
            "256-colour SGR leaked: {painted}"
        );
        assert!(!painted.contains("38;2"), "truecolor SGR leaked: {painted}");
    }
}

#[test]
fn paint_opt_skips_uncoloured_values() {
    assert_eq!(display::paint_opt(TTY, None, "queued"), "queued");
    assert_eq!(display::paint_opt(PLAIN, Some(RED), "failed"), "failed");
}

#[test]
fn glyphs_empty_in_plain_mode() {
    assert_eq!(display::ok_glyph(PLAIN), "");
    assert_eq!(display::fail_glyph(PLAIN), "");
    assert_eq!(display::warn_glyph(PLAIN), "");
    // The ASCII fallback vocabulary is what a plain surface that needs a
    // marker in both modes renders (mirrors `_display.py`).
    assert_eq!(OK_MARKER.render(PLAIN), "[OK]");
    assert_eq!(FAIL_MARKER.render(PLAIN), "[FAIL]");
    assert_eq!(WARN_MARKER.render(PLAIN), "[WARN]");
}

#[test]
fn glyphs_unicode_in_interactive_mode() {
    assert_eq!(display::ok_glyph(TTY), "✓");
    assert_eq!(display::fail_glyph(TTY), "✗");
    assert_eq!(display::warn_glyph(TTY), "!");
    assert_eq!(OK_MARKER.render(TTY), "✓");
    assert_eq!(FAIL_MARKER.render(TTY), "✗");
    assert_eq!(WARN_MARKER.render(TTY), "!");
}

#[test]
fn status_label_maps_cover_phases_and_verdicts() {
    assert_eq!(display::phase_color("done"), Some(GREEN));
    assert_eq!(display::phase_color("failed"), Some(RED));
    assert_eq!(display::phase_color("needs_attention"), Some(YELLOW));
    assert_eq!(display::phase_color("queued"), None);
    assert_eq!(display::phase_glyph(TTY, "done"), "✓");
    assert_eq!(display::phase_glyph(PLAIN, "failed"), "");
    assert_eq!(display::verdict_color("pass"), Some(GREEN));
    assert_eq!(display::verdict_color("fail"), Some(RED));
    assert_eq!(display::verdict_color("-"), None);
    assert_eq!(display::verdict_glyph(TTY, "fail"), "✗");
    assert_eq!(display::verdict_glyph(PLAIN, "pass"), "");
}

// ---------------------------------------------------------------------------
// Width, padding, wrapping
// ---------------------------------------------------------------------------

#[test]
fn visible_len_ignores_sgr() {
    assert_eq!(display::visible_len("abc"), 3);
    assert_eq!(display::visible_len("\x1b[31mabc\x1b[0m"), 3);
    // A glyph counts as one column.
    assert_eq!(display::visible_len("✓ ok"), 4);
}

#[test]
fn pad_cell_never_truncates() {
    assert_eq!(display::pad_cell("ab", 4), "ab  ");
    assert_eq!(display::pad_cell("abcd", 4), "abcd");
    assert_eq!(display::pad_cell("abcde", 4), "abcde");
}

#[test]
fn wrap_hanging_never_truncates() {
    let token = "/home/agent/".to_string() + &"a".repeat(280) + ".json";
    let wrapped = display::wrap_hanging(&token, 40, 0);
    // A single unbreakable token overflows rather than losing bytes.
    assert_eq!(wrapped, token);
}

#[test]
fn wrap_hanging_breaks_only_on_spaces() {
    let text = "the worker-bridge.py process failed to start";
    let wrapped = display::wrap_hanging(text, 24, 0);
    assert!(
        wrapped.contains("worker-bridge.py"),
        "hyphenated token was split: {wrapped}"
    );
    // Every source character survives (whitespace runs may collapse).
    let recovered: String = wrapped.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(recovered, text);
    for line in wrapped.split('\n') {
        assert!(line.chars().count() <= 24, "line over budget: {line:?}");
    }
}

#[test]
fn wrap_hanging_degenerate_width_returns_text() {
    let text = "one two three four five six";
    // width < hang + 12: a terminal narrower than its own indent cannot
    // align anything, so the line is emitted unwrapped.
    assert_eq!(display::wrap_hanging(text, 11, 4), text);
}

#[test]
fn wrap_hanging_continuation_indent() {
    let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
    let wrapped = display::wrap_hanging(text, 24, 6);
    let lines: Vec<&str> = wrapped.split('\n').collect();
    assert!(lines.len() > 1, "expected a wrap: {wrapped}");
    for line in &lines[1..] {
        assert!(
            line.starts_with("      "),
            "continuation line lost its hanging indent: {line:?}"
        );
        assert!(line.chars().count() <= 24, "line over budget: {line:?}");
    }
}

#[test]
fn wrap_hanging_preserves_leading_indent() {
    // The caller's own indentation is part of the line, not inter-word
    // whitespace: a wrapped `    - <item>` keeps its four leading spaces.
    let text = format!("    - {}", "word ".repeat(40).trim_end());
    let wrapped = display::wrap_hanging(&text, 40, 6);
    let first = wrapped.split('\n').next().expect("first line");
    assert!(first.starts_with("    - "), "indent lost: {first:?}");
}

#[test]
fn wrap_hanging_leaves_fitting_text_untouched() {
    let text = "  status: success  verdict: pass";
    assert_eq!(display::wrap_hanging(text, 80, 6), text);
}

#[test]
fn terminal_width_falls_back_to_80() {
    // Under the harness stdout is a pipe: `terminal_size` reports nothing,
    // so the documented 80-column fallback applies.
    assert_eq!(terminal_width(), FALLBACK_WIDTH);
}

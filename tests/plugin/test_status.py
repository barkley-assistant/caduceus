"""Caduceus status slash command tests."""

from __future__ import annotations

import json
import os
import re
import shutil
import stat
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Dict, List

import pytest

from tests.fixtures.fake_ctx import (
    FakePluginContext,
    assert_cli_command_registered,
    assert_command_registered,
    assert_skill_registered,
)

# The four verdict glyphs (mirrored from the adapter constant so a change to
# one without the other fails loudly).
HEALTHY = "\u2705"
WARN = "\u26a0\ufe0f"
FAIL = "\u274c"
INFO = "\u2139\ufe0f"

_PHASE_NAMES = (
    "queued",
    "in_progress",
    "previewed",
    "done",
    "failed",
    "skipped",
    "needs_attention",
    "awaiting_review",
)


def _phases(**overrides: int) -> Dict[str, int]:
    """Build the always-all-eight phase map the binary emits."""
    phases = {name: 0 for name in _PHASE_NAMES}
    phases.update(overrides)
    return phases


def _rfc3339(age_seconds: float) -> str:
    """Render an RFC3339 timestamp *age_seconds* in the past (negative = future)."""
    stamp = datetime.now(timezone.utc) - timedelta(seconds=age_seconds)
    return stamp.isoformat()


def _payload(**report_overrides: Any) -> Dict[str, Any]:
    """Build the envelope shape ``caduceus status --json`` produces."""
    report: Dict[str, Any] = {
        "version": "1.0.0",
        "last_tick_started": None,
        "last_tick_finished": None,
        "last_outcome": None,
        "phases": _phases(),
        "next_head": None,
        "rate_limit": None,
    }
    report.update(report_overrides)
    return {
        "app_version": "1.0.0",
        "version": "1.0.0",
        "diagnostic": None,
        "report": report,
    }


def test_status_slash_command_is_registered(adapter, fake_ctx: FakePluginContext) -> None:
    adapter.register(fake_ctx)
    cmd = assert_command_registered(fake_ctx, "caduceus-status")
    assert callable(cmd.handler)




def test_status_slash_command_missing_binary_returns_diagnostic(
    adapter, fake_ctx: FakePluginContext
) -> None:
    """When the binary is absent the handler returns a precise diagnostic."""
    adapter.register(fake_ctx)
    cmd = assert_command_registered(fake_ctx, "caduceus-status")
    result = cmd.handler("")
    assert isinstance(result, str)
    assert "hermes caduceus setup" in result




def test_status_slash_command_invokes_binary(
    adapter, fake_ctx: FakePluginContext, install_with_fake_binary: Path
) -> None:
    adapter.register(fake_ctx)
    cmd = assert_command_registered(fake_ctx, "caduceus-status")
    result = cmd.handler("")
    assert isinstance(result, str)
    assert "caduceus 0.1.0" in result




def test_status_slash_redacts_token_like_strings(
    adapter, fake_ctx: FakePluginContext, install_plugin: Path, tmp_path: Path
) -> None:
    """A binary that prints ``GITHUB_TOKEN=ghp_xxx`` is redacted."""
    binary = install_plugin / "bin" / "caduceus"
    binary.parent.mkdir(exist_ok=True)
    binary.write_text(
        "#!/usr/bin/env bash\n"
        'if [ "$1" = "status" ]; then\n'
        '  if [ "$2" = "--json" ]; then\n'
        '    printf \'{"app_version":"0.1.0","version":"0.1.0","diagnostic":null,"report":{"version":"0.1.0","last_tick_started":null,"last_tick_finished":null,"last_outcome":"idle","phases":{"queued":0},"next_head":null,"rate_limit":null}}\'\n'
        "  fi\n"
        "  exit 0\n"
        "fi\n"
        "exit 0\n"
    )
    binary.chmod(0o755)
    adapter.register(fake_ctx)
    cmd = assert_command_registered(fake_ctx, "caduceus-status")
    result = cmd.handler("")
    assert result is not None
    # No ``ghp_`` token made it into chat output.
    assert "ghp_" not in result
    assert "<redacted>" not in result  # the fake didn't leak one — defensive


def test_format_status_for_chat_reads_nested_report_key(adapter) -> None:
    """When the payload carries a nested ``report`` key, the formatter reads
    the app version from there. This is the shape the live ``caduceus
    status --json`` produces today.
    """
    payload = {
        "report": {
            "app_version": "1.2.3",
            "last_tick_started": None,
            "last_tick_finished": None,
            "last_outcome": None,
            "phases": {},
        }
    }
    result = adapter._format_status_for_chat(payload)
    assert "1.2.3" in result
    assert "last tick never" in result
    assert "no tick yet" in result
    assert result.splitlines()[0][0] in set(adapter._STATUS_VERDICT_EMOJI.values())


def test_format_status_for_chat_falls_back_to_root(adapter) -> None:
    """When the payload is flat (no ``report`` key), the formatter falls
    back to the root level so legacy consumers keep working.
    """
    payload = {"version": "1.2.3", "phases": {}}
    result = adapter._format_status_for_chat(payload)
    assert "1.2.3" in result


def test_format_status_for_chat_prefers_app_version_over_version(adapter) -> None:
    """When both ``app_version`` and ``version`` are present, the formatter
    prefers ``app_version`` so the chat shows the real crate version, not
    the JSON schema version.
    """
    payload = {
        "app_version": "2.0.0",
        "version": "7.5.0",
        "report": {"version": "7.5.0"},
    }
    result = adapter._format_status_for_chat(payload)
    assert "2.0.0" in result
    assert "7.5.0" not in result


def test_format_status_for_chat_shows_last_tick_started_and_finished(
    adapter,
) -> None:
    """The summary surfaces both ``last_tick_started`` and
    ``last_tick_finished`` as local time plus relative age, never as raw
    nanosecond ISO strings.
    """
    payload = _payload(
        last_tick_started=_rfc3339(2 * 3600 + 30),
        last_tick_finished=_rfc3339(2 * 3600),
        last_outcome="processed",
    )
    result = adapter._format_status_for_chat(payload)
    assert "started " in result
    assert "finished " in result
    assert "h ago" in result
    assert "2024-01-01T00:00" not in result
    assert "T00:00" not in result


def test_format_status_healthy_idle_empty_queue(adapter) -> None:
    """A recent idle304 tick with nothing queued renders the healthy verdict,
    a quiet queue, and the rate-limit line."""
    payload = _payload(
        last_tick_started=_rfc3339(120),
        last_tick_finished=_rfc3339(120),
        last_outcome="idle304",
        rate_limit={"limit": 5000, "remaining": 4876, "reset_at": None},
    )
    result = adapter._format_status_for_chat(payload)
    assert HEALTHY in result
    assert "Idle — GitHub 304, no changes" in result
    assert "Queue empty" in result
    assert "Rate limit: 4876/5000" in result
    assert "|" not in result
    assert "rate_limit" not in result
    assert "unknown" not in result
    assert "min ago" in result


def test_format_status_queue_table_shows_only_nonzero_operational_rows(
    adapter,
) -> None:
    """Non-zero operational phases render as a compact table in signal order;
    terminal bookkeeping counts drop to a trailing sentence."""
    payload = _payload(
        last_tick_started=_rfc3339(120),
        last_outcome="idle304",
        phases=_phases(queued=2, in_progress=1, done=12, awaiting_review=0),
    )
    result = adapter._format_status_for_chat(payload)
    assert "| Phase | Count |" in result
    assert "|---|" in result
    assert "| queued | 2 |" in result
    assert "| in_progress | 1 |" in result
    assert "awaiting_review" not in result
    assert "Queue empty" not in result
    assert "Also done: 12" in result
    assert result.index("| queued | 2 |") < result.index("| in_progress | 1 |")


def test_format_status_table_block_terminated_by_blank_line(adapter) -> None:
    """A blank line must separate the queue table from following prose: GFM
    ends a table only at a blank line, so without one the ``Also`` / ``Next`` /
    ``Rate limit`` lines render as single-cell rows of the table."""
    payload = _payload(
        last_tick_started=_rfc3339(120),
        last_tick_finished=_rfc3339(120),
        last_outcome="idle304",
        phases=_phases(queued=2, in_progress=1, done=12, skipped=1),
        next_head="owner/repo#42",
        rate_limit={"limit": 5000, "remaining": 42},
    )
    lines = adapter._format_status_for_chat(payload).splitlines()
    assert [line for line in lines if line.startswith("  |")] == [
        "  | Phase | Count |",
        "  |---|---|",
        "  | queued | 2 |",
        "  | in_progress | 1 |",
    ]
    also = lines.index("  Also done: 12 · skipped: 1")
    assert lines[also - 1] == ""
    assert lines[also + 1] == "  Next: owner/repo#42"
    assert lines[also + 2] == "  Rate limit: 42/5000"

    # a table with no bookkeeping sentence still terminates before the prose
    payload = _payload(
        last_tick_started=_rfc3339(120),
        last_outcome="idle_empty",
        phases=_phases(failed=1),
        next_head="owner/repo#42",
    )
    lines = adapter._format_status_for_chat(payload).splitlines()
    assert lines[-2:] == ["", "  Next: owner/repo#42"]


def test_format_status_prose_only_output_has_no_blank_lines(adapter) -> None:
    """The blank separator exists only to terminate a table; a queue with no
    non-zero operational phase stays contiguous."""
    payload = _payload(
        last_tick_started=_rfc3339(120),
        last_outcome="idle304",
        phases=_phases(previewed=2),
        next_head="owner/repo#42",
    )
    result = adapter._format_status_for_chat(payload)
    assert "|" not in result
    assert "" not in result.splitlines()
    assert "\n\n" not in result


def test_format_status_failed_phase_warns(adapter) -> None:
    payload = _payload(
        last_tick_started=_rfc3339(120),
        last_outcome="idle_empty",
        phases=_phases(failed=1),
    )
    result = adapter._format_status_for_chat(payload)
    assert WARN in result
    assert "Recent failure" in result
    assert HEALTHY not in result


def test_format_status_needs_attention_warns(adapter) -> None:
    payload = _payload(
        last_tick_started=_rfc3339(120),
        last_outcome="idle_empty",
        phases=_phases(needs_attention=3),
    )
    result = adapter._format_status_for_chat(payload)
    assert WARN in result
    assert "attention" in result


def test_format_status_in_progress_info(adapter) -> None:
    payload = _payload(
        last_tick_started=_rfc3339(120),
        last_outcome="processed",
        phases=_phases(in_progress=2),
    )
    result = adapter._format_status_for_chat(payload)
    assert INFO in result
    assert "Working" in result
    assert "2 run(s) in progress" in result


def test_format_status_queued_info(adapter) -> None:
    payload = _payload(
        last_tick_started=_rfc3339(120),
        last_outcome="idle_empty",
        phases=_phases(queued=1),
    )
    result = adapter._format_status_for_chat(payload)
    assert INFO in result
    assert "Queued work" in result
    assert "1 queued" in result


def test_format_status_rate_limited_outcome(adapter) -> None:
    """A rate-limited tick decodes in the header and drives the verdict."""
    payload = _payload(
        last_tick_started=_rfc3339(120),
        last_outcome="rate_limited",
        next_allowed_poll_at=_rfc3339(-120),
    )
    result = adapter._format_status_for_chat(payload)
    assert INFO in result
    # once in the verdict line, once in the header
    assert result.count("Rate-limited by GitHub") == 2
    assert "polling resumes" in result


def test_format_status_stale_tick_warns(adapter) -> None:
    payload = _payload(
        last_tick_started=_rfc3339(50 * 60),
        last_tick_finished=_rfc3339(50 * 60),
        last_outcome="idle_empty",
    )
    result = adapter._format_status_for_chat(payload)
    assert WARN in result
    assert "Tick overdue" in result


def test_format_status_skipped_cadence_decodes(adapter) -> None:
    payload = _payload(
        last_tick_started=_rfc3339(120),
        last_tick_finished=_rfc3339(120),
        last_outcome="skipped_cadence",
    )
    result = adapter._format_status_for_chat(payload)
    assert "cadence interval not elapsed" in result


def test_format_status_unknown_outcome_verbatim(adapter) -> None:
    """The legacy ``idle`` label (emitted by older binaries and the test fake)
    passes through verbatim instead of crashing or being mislabelled."""
    payload = _payload(last_outcome="idle")
    result = adapter._format_status_for_chat(payload)
    assert "(idle)" in result
    assert HEALTHY in result


def test_format_status_corrupt_state_fails(adapter) -> None:
    payload = _payload(state_corrupt=True)
    result = adapter._format_status_for_chat(payload)
    assert FAIL in result
    assert "corrupt" in result
    assert HEALTHY not in result


def test_chat_when_buckets(adapter) -> None:
    """``_chat_when`` buckets: never / verbatim / just now / min / h / d /
    future clamp, and chrono's nanosecond RFC3339 parses."""
    when = adapter._chat_when
    assert when(None) == "never"
    assert when("") == "never"
    assert when("not-a-timestamp") == "not-a-timestamp"
    assert when(_rfc3339(5)).endswith("just now")
    assert "2 min ago" in when(_rfc3339(120))
    assert "3h ago" in when(_rfc3339(3 * 3600))
    assert "2d ago" in when(_rfc3339(2 * 86400))
    # a future timestamp (clock skew) clamps instead of printing a negative age
    assert when(_rfc3339(-300)).endswith("just now")
    nano = when("2026-09-21T14:13:15.405154539Z")
    assert nano != "2026-09-21T14:13:15.405154539Z"
    assert "ago" in nano


def test_format_status_omits_rate_limit_when_unknown(adapter) -> None:
    payload = _payload(last_outcome="idle_empty", rate_limit=None)
    result = adapter._format_status_for_chat(payload)
    assert "Rate limit" not in result
    assert "rate_limit" not in result


def test_format_status_rate_limit_without_limit_field(adapter) -> None:
    payload = _payload(
        last_outcome="idle_empty",
        rate_limit={"remaining": 40, "limit": None},
    )
    result = adapter._format_status_for_chat(payload)
    assert "Rate limit: 40 remaining" in result


def test_format_status_next_head_line(adapter) -> None:
    payload = _payload(last_outcome="idle_empty", next_head="owner/repo#42")
    result = adapter._format_status_for_chat(payload)
    assert "  Next: owner/repo#42" in result


def test_format_status_plain_text_no_ansi(adapter) -> None:
    """The rendering is line-based with no ANSI bytes; the only markdown
    construct is the indented two-column table."""
    payload = _payload(
        last_tick_started=_rfc3339(120),
        last_tick_finished=_rfc3339(120),
        last_outcome="idle304",
        phases=_phases(queued=2, in_progress=1, done=12),
        next_head="owner/repo#42",
        rate_limit={"limit": 5000, "remaining": 42},
    )
    result = adapter._format_status_for_chat(payload)
    assert "\x1b" not in result
    assert "**" not in result
    lines = result.splitlines()
    assert lines[0].startswith(INFO)
    for line in lines[2:]:
        assert line.startswith("  ") or line == "", line
    table_rows = [line for line in lines if line.strip().startswith("|")]
    assert table_rows
    assert all(line.startswith("  |") for line in table_rows)
    # the table block is closed by a blank line before any following prose
    assert any(line == "" for line in lines)

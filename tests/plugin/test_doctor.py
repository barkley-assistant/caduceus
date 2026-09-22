"""Doctor display-layer tests — TTY policy, plain byte-identity, wrapping.

The rendering policy lives in ``_display``; ``_cli_doctor`` only wires it
up. Tests drive the policy with fake TTY streams (``isatty()`` True plus
an explicit encoding) rather than a real pty, so they are deterministic
under parallel test threads.
"""

from __future__ import annotations

import io
import json
import re
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Dict, Iterator, List, Tuple

import pytest

from caduceus import _display

from tests.plugin._helpers import _stub_cron_runtime


_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")

GLYPH_OK = "\u2713"
GLYPH_FAIL = "\u2717"
EM_DASH = "\u2014"


class _FakeTTY(io.StringIO):
    """A capture stream that claims to be an interactive terminal."""

    encoding = "utf-8"

    def isatty(self) -> bool:
        return True


def _encoded_tty(encoding: str) -> _FakeTTY:
    class _EncodedTTY(_FakeTTY):
        pass

    _EncodedTTY.encoding = encoding
    return _EncodedTTY()


def _visible(text: str) -> str:
    return _ANSI_RE.sub("", text)


def _control_env(monkeypatch: pytest.MonkeyPatch, *, term: str | None, no_color: str | None) -> None:
    if term is None:
        monkeypatch.delenv("TERM", raising=False)
    else:
        monkeypatch.setenv("TERM", term)
    if no_color is None:
        monkeypatch.delenv("NO_COLOR", raising=False)
    else:
        monkeypatch.setenv("NO_COLOR", no_color)


def _finding(**overrides: Any):
    from caduceus import _DoctorFinding

    fields: Dict[str, Any] = {
        "category": "config-incomplete",
        "status": "fail",
        "detail": "provider secret not configured",
        "next_action": "set CADUCEUS_GITHUB_TOKEN",
        "internal_detail": "checked CADUCEUS_GITHUB_TOKEN, GITHUB_TOKEN, GH_TOKEN",
    }
    fields.update(overrides)
    return _DoctorFinding(**fields)


def _interactive_renderer(width: int = 120, name_width: int = 15) -> "_display.DoctorRenderer":
    return _display.DoctorRenderer(_display.Style(interactive=True), width, name_width)


def _plain_renderer(width: int = 80, name_width: int = 15) -> "_display.DoctorRenderer":
    return _display.DoctorRenderer(_display.Style(interactive=False), width, name_width)


# ---------------------------------------------------------------------------
# Policy gate
# ---------------------------------------------------------------------------


def test_detect_style_interactive_tty(monkeypatch: pytest.MonkeyPatch) -> None:
    _control_env(monkeypatch, term="xterm-256color", no_color=None)
    assert _display.detect_style(_FakeTTY()) == _display.Style(interactive=True)


def test_detect_style_piped_is_plain() -> None:
    assert _display.detect_style(io.StringIO()) == _display.Style(interactive=False)


def test_detect_style_term_dumb_is_plain(monkeypatch: pytest.MonkeyPatch) -> None:
    _control_env(monkeypatch, term="dumb", no_color=None)
    assert _display.detect_style(_FakeTTY()).interactive is False


@pytest.mark.parametrize("value", ["1", ""])
def test_detect_style_no_color_env_is_plain(
    monkeypatch: pytest.MonkeyPatch, value: str
) -> None:
    _control_env(monkeypatch, term="xterm-256color", no_color=value)
    assert _display.detect_style(_FakeTTY()).interactive is False


@pytest.mark.parametrize("encoding", ["ascii", "ANSI_X3.4-1968", "cp437"])
def test_detect_style_ascii_encoding_is_plain(
    monkeypatch: pytest.MonkeyPatch, encoding: str
) -> None:
    """A glyph-incapable stdout encoding falls back to plain mode.

    ``ANSI_X3.4-1968`` is how a glibc C locale reports itself and
    ``cp437`` stands in for any codec without the glyphs; rendering
    those would raise ``UnicodeEncodeError`` mid-report.
    """
    _control_env(monkeypatch, term="xterm-256color", no_color=None)
    assert _display.detect_style(_encoded_tty(encoding)).interactive is False


# ---------------------------------------------------------------------------
# Status vocabulary
# ---------------------------------------------------------------------------


def test_status_vocabulary_table() -> None:
    assert _display._STATUS_VOCAB == {
        "ok": (GLYPH_OK, "OK", "[OK]", "\x1b[32m"),
        "fail": (GLYPH_FAIL, "FAIL", "[FAIL]", "\x1b[31m"),
        "warn": ("!", "WARN", "[WARN]", "\x1b[33m"),
    }


# ---------------------------------------------------------------------------
# Plain mode — the byte contract
# ---------------------------------------------------------------------------


def test_plain_render_is_byte_identical_to_legacy() -> None:
    renderer = _plain_renderer()
    finding = _finding()
    assert renderer.finding("Provider Secret", finding, verbose=False) == [
        "[FAIL] Provider Secret \u2014 provider secret not configured",
        "       next action: set CADUCEUS_GITHUB_TOKEN",
    ]
    assert renderer.finding("Provider Secret", finding, verbose=True) == [
        "[FAIL] Provider Secret \u2014 provider secret not configured",
        "       next action: set CADUCEUS_GITHUB_TOKEN",
        "       detail:      checked CADUCEUS_GITHUB_TOKEN, GITHUB_TOKEN, GH_TOKEN",
        "       category:    config-incomplete",
    ]
    healthy = _finding(
        status="ok",
        detail="all good",
        next_action="",
        internal_detail="",
    )
    assert renderer.finding("Binary", healthy, verbose=True) == [
        "[OK] Binary \u2014 all good",
        "       detail:      all good",
    ]


def test_plain_render_has_no_ansi_or_glyphs() -> None:
    renderer = _plain_renderer()
    joined = "\n".join(renderer.finding("Provider Secret", _finding(), verbose=True))
    assert "\x1b" not in joined
    assert GLYPH_OK not in joined
    assert GLYPH_FAIL not in joined


# ---------------------------------------------------------------------------
# Interactive mode
# ---------------------------------------------------------------------------


def test_interactive_render_glyph_color_and_columns() -> None:
    renderer = _interactive_renderer()
    ok_finding = _finding(status="ok", detail="binary present", next_action="", internal_detail="")

    ok_lines = renderer.finding("Binary", ok_finding, verbose=False)
    fail_lines = renderer.finding("Provider Secret", _finding(), verbose=False)

    assert ok_lines[0].startswith("\x1b[32m" + GLYPH_OK + " OK  ")
    assert _display.RESET in ok_lines[0]
    assert fail_lines[0].startswith("\x1b[31m" + GLYPH_FAIL + " FAIL")
    assert _display.RESET in fail_lines[0]

    ok_detail_at = _visible(ok_lines[0]).index("binary present")
    fail_detail_at = _visible(fail_lines[0]).index("provider secret not configured")
    assert ok_detail_at == fail_detail_at == renderer.hang


def test_interactive_render_hanging_indent_sublines() -> None:
    renderer = _interactive_renderer()
    lines = renderer.finding("Provider Secret", _finding(), verbose=True)
    indent = " " * renderer.hang

    assert lines[1] == indent + "next action: set CADUCEUS_GITHUB_TOKEN"
    assert lines[2] == (
        indent + "detail:      checked CADUCEUS_GITHUB_TOKEN, GITHUB_TOKEN, GH_TOKEN"
    )
    assert lines[3] == indent + "category:    config-incomplete"


# ---------------------------------------------------------------------------
# Wrapping
# ---------------------------------------------------------------------------


def test_wrap_never_truncates_and_hangs() -> None:
    words = ["item%02d" % index for index in range(45)]
    text = " ".join(words)
    assert len(text) > 300

    wrapped = _display.wrap_hanging(text, width=40, hang=25)
    lines = wrapped.split("\n")

    assert len(lines) > 1
    assert all(len(line) <= 40 for line in lines)
    for line in lines[1:]:
        assert line.startswith(" " * 25)
    assert " ".join(wrapped.split()) == text  # no word lost, none duplicated


def test_wrap_does_not_break_hyphens() -> None:
    text = "bridge at /home/agent/.hermes/caduceus/worker-bridge.py is executable"
    wrapped = _display.wrap_hanging(text, width=30, hang=25)
    assert all("worker-bridge" not in line or "worker-bridge.py" in line for line in wrapped.split("\n"))
    assert "worker-bridge.py" in wrapped


def test_narrow_terminal_guard_skips_wrap() -> None:
    text = "a very long detail " * 10
    renderer = _interactive_renderer(width=30, name_width=15)
    assert renderer.width < renderer.hang + 12
    lines = renderer.finding(
        "Binary",
        _finding(detail=text, next_action="", internal_detail=""),
        verbose=False,
    )
    assert len(lines) == 1
    assert _visible(lines[0]).endswith(text)
    assert len(_visible(lines[0])) > renderer.width  # overflow, never truncation


# ---------------------------------------------------------------------------
# WARN status — the advisory row (issue #414)
# ---------------------------------------------------------------------------


def test_plain_render_warn() -> None:
    """A ``warn`` finding renders as ``[WARN]`` with sub-lines and no ANSI."""
    renderer = _plain_renderer()
    finding = _finding(
        category="daemon-defect",
        status="warn",
        detail="last tick 7 minutes ago (cron fires every 2 min)",
        next_action="check the cron ticker (`hermes cron list`) if this persists",
        internal_detail="last_tick_started=2026-09-12T15:40:24Z; age_seconds=420",
    )

    assert renderer.finding("Tick Freshness", finding, verbose=False) == [
        "[WARN] Tick Freshness \u2014 last tick 7 minutes ago (cron fires every 2 min)",
        "       next action: check the cron ticker (`hermes cron list`) if this persists",
    ]
    assert renderer.finding("Tick Freshness", finding, verbose=True) == [
        "[WARN] Tick Freshness \u2014 last tick 7 minutes ago (cron fires every 2 min)",
        "       next action: check the cron ticker (`hermes cron list`) if this persists",
        "       detail:      last_tick_started=2026-09-12T15:40:24Z; age_seconds=420",
        "       category:    daemon-defect",
    ]
    joined = "\n".join(renderer.finding("Tick Freshness", finding, verbose=True))
    assert "\x1b" not in joined
    assert GLYPH_OK not in joined
    assert GLYPH_FAIL not in joined


def test_interactive_render_warn() -> None:
    """A ``warn`` finding uses the ``!`` glyph, yellow SGR, and hang column."""
    renderer = _interactive_renderer()
    finding = _finding(
        category="daemon-defect",
        status="warn",
        detail="last tick 7 minutes ago",
        next_action="",
        internal_detail="",
    )

    lines = renderer.finding("Tick Freshness", finding, verbose=False)

    assert lines[0].startswith("\x1b[33m" + "! WARN" + _display.RESET)
    assert _visible(lines[0]).startswith("! WARN " + "Tick Freshness".ljust(15) + " \u2014 ")
    assert _visible(lines[0]).index("last tick 7 minutes ago") == renderer.hang


def test_finding_passes_warn_status_through() -> None:
    """The renderer honours ``warn`` and still falls back to ``fail`` for junk."""
    warn_finding = _finding(status="warn", detail="advisory", next_action="", internal_detail="")
    unknown_finding = _finding(status="bogus", detail="junk", next_action="", internal_detail="")

    assert _plain_renderer().finding("X", warn_finding, verbose=False) == [
        "[WARN] X \u2014 advisory"
    ]
    assert _plain_renderer().finding("X", unknown_finding, verbose=False) == [
        "[FAIL] X \u2014 junk"
    ]


# ---------------------------------------------------------------------------
# End to end through _cli_doctor
# ---------------------------------------------------------------------------


@pytest.fixture
def healthy_home(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path, monkeypatch: pytest.MonkeyPatch
) -> Iterator[None]:
    """Healthy environment: binary, executable bridge, configured secret."""
    monkeypatch.setenv("CADUCEUS_GITHUB_TOKEN", "ghp_test-secret-configured")
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)
    _stub_cron_runtime(adapter, {})
    yield


def test_doctor_plain_end_to_end_matches_legacy_prefixes(
    adapter,
    healthy_home: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from caduceus import _runtime

    _control_env(monkeypatch, term="xterm-256color", no_color=None)
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()

    out = capsys.readouterr().out
    assert rc == 0
    assert "[OK] Binary \u2014" in out
    assert "[OK] Bridge Harness \u2014" in out
    assert "[OK] Provider Secret \u2014" in out
    assert "[OK] Cron Capability \u2014" in out
    assert "[OK] Hermes Home \u2014" in out
    assert "       detail:      " not in out
    assert "       category:    " not in out
    assert "\x1b" not in out


def test_doctor_tty_end_to_end_glyphs_and_color(
    adapter,
    healthy_home: None,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from caduceus import _runtime

    _control_env(monkeypatch, term="xterm-256color", no_color=None)
    monkeypatch.setenv("COLUMNS", "120")
    stream = _FakeTTY()
    monkeypatch.setattr(sys, "stdout", stream)
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()

    out = stream.getvalue()
    assert rc == 0
    assert GLYPH_OK in out
    assert "\x1b[32m" in out
    assert "[OK]" not in out
    assert "[FAIL]" not in out


@pytest.mark.parametrize("env", [{"NO_COLOR": "1"}, {"TERM": "dumb"}])
def test_doctor_tty_plain_fallbacks_end_to_end(
    adapter,
    healthy_home: None,
    monkeypatch: pytest.MonkeyPatch,
    env: Dict[str, str],
) -> None:
    from caduceus import _runtime

    _control_env(monkeypatch, term="xterm-256color", no_color=None)
    for key, value in env.items():
        monkeypatch.setenv(key, value)
    stream = _FakeTTY()
    monkeypatch.setattr(sys, "stdout", stream)
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()

    out = stream.getvalue()
    assert rc == 0
    assert "[OK] Binary \u2014" in out
    assert "\x1b" not in out
    assert GLYPH_OK not in out


def test_doctor_exit_codes_preserved_in_tty_mode(
    adapter,
    install_with_fake_binary: Path,
    healthy_home: None,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from caduceus import _DoctorFinding, _runtime

    _control_env(monkeypatch, term="xterm-256color", no_color=None)
    monkeypatch.setenv("COLUMNS", "120")

    original_secret = adapter._doctor_check_provider_secret

    def _run() -> Tuple[int, str]:
        stream = _FakeTTY()
        monkeypatch.setattr(sys, "stdout", stream)
        return adapter._cli_doctor(), stream.getvalue()

    try:
        rc_healthy, tty_out = _run()
        assert rc_healthy == 0
        assert GLYPH_OK in tty_out

        def _failing_secret():
            return _DoctorFinding(
                category="config-incomplete",
                status="fail",
                detail="provider secret not configured",
                next_action="set CADUCEUS_GITHUB_TOKEN",
            )

        adapter._doctor_check_provider_secret = _failing_secret  # type: ignore[assignment]
        rc_config, config_out = _run()
        assert rc_config == 1
        assert GLYPH_FAIL in config_out

        install_with_fake_binary.unlink()
        rc_prereq, prereq_out = _run()
        assert rc_prereq == 2
        assert GLYPH_FAIL in prereq_out
    finally:
        adapter._doctor_check_provider_secret = original_secret
        _runtime.reset_dispatcher()


def test_doctor_verbose_in_tty_mode_prints_detail_and_category(
    adapter,
    healthy_home: None,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from caduceus import _DoctorFinding, _runtime

    _control_env(monkeypatch, term="xterm-256color", no_color=None)
    monkeypatch.setenv("COLUMNS", "200")
    original_secret = adapter._doctor_check_provider_secret

    def _failing_secret():
        return _DoctorFinding(
            category="config-incomplete",
            status="fail",
            detail="provider secret not configured",
            next_action="set CADUCEUS_GITHUB_TOKEN",
            internal_detail="checked CADUCEUS_GITHUB_TOKEN, GITHUB_TOKEN, GH_TOKEN",
        )

    stream = _FakeTTY()
    monkeypatch.setattr(sys, "stdout", stream)
    try:
        adapter._doctor_check_provider_secret = _failing_secret  # type: ignore[assignment]
        rc = adapter._cli_doctor(verbose=True)
    finally:
        adapter._doctor_check_provider_secret = original_secret
        _runtime.reset_dispatcher()

    out = _visible(stream.getvalue())
    assert rc == 1
    assert "detail:      checked CADUCEUS_GITHUB_TOKEN, GITHUB_TOKEN, GH_TOKEN" in out
    assert "category:    config-incomplete" in out


# ---------------------------------------------------------------------------
# Chained checks — one unified report (issue #414)
# ---------------------------------------------------------------------------


def _minutes_ago(minutes: float) -> str:
    """An RFC3339 timestamp *minutes* in the past (chrono emits ``Z``)."""
    stamp = datetime.now(timezone.utc) - timedelta(minutes=minutes)
    return stamp.isoformat().replace("+00:00", "Z")


def _doctor_report(verdict: str, checks: List[Dict[str, Any]]) -> str:
    """A ``caduceus doctor --json`` ReadinessReport payload."""
    return json.dumps(
        {
            "schema_version": "1.0.0",
            "generated_at": "2026-09-12T15:40:24.394878683Z",
            "verdict": verdict,
            "checks": checks,
        }
    )


def _status_envelope(last_tick_started: Any, *, diagnostic: Any = None) -> str:
    """A ``caduceus status --json`` envelope (see src/daemon/status.rs)."""
    return json.dumps(
        {
            "app_version": "1.0.0",
            "version": "7.7.0",
            "diagnostic": diagnostic,
            "report": {"version": "7.7.0", "last_tick_started": last_tick_started},
        }
    )


def _stub_chained(
    adapter,
    monkeypatch: pytest.MonkeyPatch,
    *,
    doctor: str = "",
    status: str = "",
    doctor_rc: int = 0,
    status_rc: int = 0,
) -> None:
    """Replace ``_run`` with canned chained doctor/status results."""

    def fake_run(argv, *, cwd=None, timeout=None):
        sub = argv[1] if len(argv) > 1 else ""
        if sub == "doctor":
            return subprocess.CompletedProcess(argv, doctor_rc, doctor, "")
        if sub == "status":
            return subprocess.CompletedProcess(argv, status_rc, status, "")
        return subprocess.CompletedProcess(argv, 0, "", "")

    monkeypatch.setattr(adapter, "_run", fake_run)


_PASSING_CHECKS = [
    {"id": "platform", "status": "pass", "detail": "Linux host", "remediation": None},
    {"id": "engine", "status": "pass", "detail": "docker reachable", "remediation": None},
]

_TRUSTED_HOST_CHECKS = [
    {
        "id": "engine",
        "status": "fail",
        "detail": "no sandbox configuration is present",
        "remediation": "configure executor_mode: oci or keep trusted_host mode",
    }
]


def test_doctor_chained_renders_both_families(
    adapter,
    install_with_fake_binary: Path,
    healthy_home: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """One run reports install health, OCI readiness, and tick freshness (AC-1)."""
    from caduceus import _runtime

    _stub_chained(
        adapter,
        monkeypatch,
        doctor=_doctor_report("READY", _PASSING_CHECKS),
        status=_status_envelope(_minutes_ago(1)),
    )
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()

    out = capsys.readouterr().out
    assert rc == 0
    assert "[OK] Binary \u2014" in out
    assert "[OK] OCI Readiness \u2014 OCI readiness: READY (2 checks, all passed)" in out
    assert "[OK] Tick Freshness \u2014 last tick 1 minute ago" in out
    assert "\x1b" not in out


def test_doctor_chained_trusted_host_is_warn_not_fail(
    adapter,
    install_with_fake_binary: Path,
    healthy_home: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Trusted-host UNAVAILABLE renders as [WARN] and keeps exit 0 (D2)."""
    from caduceus import _runtime

    _stub_chained(
        adapter,
        monkeypatch,
        doctor=_doctor_report("UNAVAILABLE", _TRUSTED_HOST_CHECKS),
        status=_status_envelope(_minutes_ago(1)),
    )
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()

    out = capsys.readouterr().out
    assert rc == 0
    assert "[WARN] OCI Readiness \u2014 OCI readiness: UNAVAILABLE (1 check, 1 failed)" in out
    assert "[FAIL] OCI Readiness" not in out


def test_doctor_tick_freshness_dead_flips_exit_to_1(
    adapter,
    install_with_fake_binary: Path,
    healthy_home: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A tick 45 minutes stale is a FAIL and drives exit 1 (AC-2)."""
    from caduceus import _runtime

    _stub_chained(
        adapter,
        monkeypatch,
        doctor=_doctor_report("READY", _PASSING_CHECKS),
        status=_status_envelope(_minutes_ago(45)),
    )
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()

    out = capsys.readouterr().out
    assert rc == 1
    assert "[FAIL] Tick Freshness \u2014 last tick 45 minutes ago" in out


def test_doctor_tick_freshness_warn_keeps_exit_0(
    adapter,
    install_with_fake_binary: Path,
    healthy_home: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A tick 10 minutes stale is advisory: [WARN] and exit 0 (D1, AC-2)."""
    from caduceus import _runtime

    _stub_chained(
        adapter,
        monkeypatch,
        doctor=_doctor_report("READY", _PASSING_CHECKS),
        status=_status_envelope(_minutes_ago(10)),
    )
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()

    out = capsys.readouterr().out
    assert rc == 0
    assert "[WARN] Tick Freshness \u2014 last tick 10 minutes ago" in out


def test_doctor_exit_codes_preserved_with_new_checks(
    adapter,
    install_with_fake_binary: Path,
    healthy_home: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The chained checks leave the 0/1/2 exit-code contract intact."""
    from caduceus import _DoctorFinding, _runtime

    _stub_chained(
        adapter,
        monkeypatch,
        doctor=_doctor_report("READY", _PASSING_CHECKS),
        status=_status_envelope(_minutes_ago(1)),
    )
    original_secret = adapter._doctor_check_provider_secret

    def _failing_secret():
        return _DoctorFinding(
            category="config-incomplete",
            status="fail",
            detail="provider secret not configured",
            next_action="set CADUCEUS_GITHUB_TOKEN",
        )

    try:
        rc_healthy = adapter._cli_doctor()
        capsys.readouterr()

        adapter._doctor_check_provider_secret = _failing_secret  # type: ignore[assignment]
        rc_config = adapter._cli_doctor()
        capsys.readouterr()

        install_with_fake_binary.unlink()
        rc_prereq = adapter._cli_doctor()
        capsys.readouterr()
    finally:
        adapter._doctor_check_provider_secret = original_secret
        _runtime.reset_dispatcher()

    assert (rc_healthy, rc_config, rc_prereq) == (0, 1, 2)

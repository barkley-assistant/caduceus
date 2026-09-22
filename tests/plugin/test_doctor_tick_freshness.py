"""Tick-freshness check tests (issue #414, AC-2).

The check reads ``last_tick_started`` from the binary's
``caduceus status --json`` envelope and ages it against the registered
2-minute cron cadence: under 5 minutes is OK, 5-30 minutes is WARN, and
30 minutes or more is FAIL (``daemon-defect`` -> exit 1).

``_run`` is stubbed with payloads built relative to ``datetime.now`` so
the thresholds are exercised deterministically without depending on
wall-clock time.
"""

from __future__ import annotations

import json
import subprocess
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Dict, Iterator, List, Optional

import pytest

from tests.plugin._helpers import _stub_cron_runtime


def _iso(minutes_ago: float) -> str:
    """An RFC3339 timestamp *minutes_ago* in the past (chrono emits ``Z``)."""
    stamp = datetime.now(timezone.utc) - timedelta(minutes=minutes_ago)
    return stamp.isoformat().replace("+00:00", "Z")


def _status_stdout(
    last_tick_started: Optional[str],
    *,
    diagnostic: Optional[str] = None,
) -> str:
    """The ``caduceus status --json`` envelope (see src/daemon/status.rs)."""
    return json.dumps(
        {
            "app_version": "1.0.0",
            "version": "7.7.0",
            "diagnostic": diagnostic,
            "report": {
                "version": "7.7.0",
                "last_tick_started": last_tick_started,
                "last_tick_finished": None,
                "last_outcome": "idle",
            },
        }
    )


@pytest.fixture
def healthy_env(
    adapter,
    isolated_hermes_home: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> Iterator[None]:
    """Healthy install-health environment (bridge, secret, cron) without the binary."""
    monkeypatch.setenv("CADUCEUS_GITHUB_TOKEN", "ghp_test-secret-configured")
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)
    _stub_cron_runtime(adapter, {})
    yield


def _stub_status(
    adapter,
    monkeypatch: pytest.MonkeyPatch,
    *,
    stdout: str = "",
    returncode: int = 0,
    error: Optional[str] = None,
    argv_sink: Optional[List[List[str]]] = None,
) -> None:
    """Replace ``_run`` so the chained status call is canned.

    The OCI-readiness check shares ``_run``; it gets a benign READY report
    so this suite isolates the freshness check.
    """

    def fake_run(argv, *, cwd=None, timeout=None):
        sub = argv[1] if len(argv) > 1 else ""
        if argv_sink is not None:
            argv_sink.append(list(argv))
        if sub == "status":
            if error is not None:
                raise RuntimeError(error)
            return subprocess.CompletedProcess(argv, returncode, stdout, "")
        return subprocess.CompletedProcess(
            argv, 0, json.dumps({"verdict": "READY", "checks": []}), ""
        )

    monkeypatch.setattr(adapter, "_run", fake_run)


def _freshness_line(out: str) -> str:
    for line in out.splitlines():
        if line.startswith(("[OK]", "[WARN]", "[FAIL]")) and "Tick Freshness" in line:
            return line
    raise AssertionError(f"no Tick Freshness line in report:\n{out}")


def test_tick_freshness_recent_is_ok(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A tick one minute old is healthy and keeps exit 0."""
    _stub_status(adapter, monkeypatch, stdout=_status_stdout(_iso(1)))

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 0
    assert _freshness_line(out) == "[OK] Tick Freshness — last tick 1 minute ago"


def test_tick_freshness_stale_is_warn(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A tick 10 minutes old is advisory: [WARN] with exit 0 (D1)."""
    _stub_status(adapter, monkeypatch, stdout=_status_stdout(_iso(10)))

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 0
    assert (
        _freshness_line(out)
        == "[WARN] Tick Freshness — last tick 10 minutes ago (cron fires every 2 min)"
    )


def test_tick_freshness_dead_is_fail(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A tick 45 minutes old is a daemon defect: [FAIL] and exit 1."""
    _stub_status(adapter, monkeypatch, stdout=_status_stdout(_iso(45)))

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 1
    assert (
        _freshness_line(out)
        == "[FAIL] Tick Freshness — last tick 45 minutes ago — the daemon appears dead"
    )


def test_tick_freshness_null_is_warn(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``last_tick_started: null`` means "never ticked": advisory (D6)."""
    _stub_status(adapter, monkeypatch, stdout=_status_stdout(None))

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 0
    assert (
        _freshness_line(out)
        == "[WARN] Tick Freshness — daemon has never ticked (no tick recorded yet)"
    )


def test_tick_freshness_no_state_is_warn(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """exit 2 + ``no_state`` is a fresh install, not a defect (D6)."""
    _stub_status(
        adapter,
        monkeypatch,
        stdout=_status_stdout(None, diagnostic="no_state"),
        returncode=2,
    )

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 0
    assert (
        _freshness_line(out)
        == "[WARN] Tick Freshness — no state directory yet — the daemon has not ticked"
    )


def test_tick_freshness_corrupt_is_warn(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """exit 1 + ``corrupt_state`` is advisory; state health is checked elsewhere (D6)."""
    _stub_status(
        adapter,
        monkeypatch,
        stdout=_status_stdout(None, diagnostic="corrupt_state"),
        returncode=1,
    )

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 0
    assert (
        _freshness_line(out)
        == "[WARN] Tick Freshness — could not read daemon status (corrupt_state)"
    )


def test_tick_freshness_timeout_is_warn(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A status call that cannot run is advisory, never a failure."""
    _stub_status(adapter, monkeypatch, error="timeout after 15s running caduceus status --json")

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 0
    assert _freshness_line(out).startswith(
        "[WARN] Tick Freshness — could not read daemon status (timeout after 15s"
    )


def test_tick_freshness_nanosecond_rfc3339_parses(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Chrono's nanosecond RFC3339 (9 fractional digits) parses on 3.12."""
    stamp = datetime.now(timezone.utc) - timedelta(minutes=1, seconds=30)
    nanosecond = f"{stamp.strftime('%Y-%m-%dT%H:%M:%S')}.394878683Z"
    _stub_status(adapter, monkeypatch, stdout=_status_stdout(nanosecond))

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 0
    assert _freshness_line(out) == "[OK] Tick Freshness — last tick 1 minute ago"
    # The literal value observed in ~/.hermes/caduceus-state/doctor.json.
    assert adapter._parse_rfc3339("2026-09-12T15:40:24.394878683Z") is not None


def test_tick_freshness_binary_missing_is_ok_skipped(
    adapter,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Without a binary the check is skipped; the Binary check owns exit 2."""
    _stub_status(adapter, monkeypatch, stdout=_status_stdout(_iso(1)))

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 2
    assert (
        _freshness_line(out)
        == "[OK] Tick Freshness — tick freshness skipped (binary not installed "
        "— see Binary check)"
    )

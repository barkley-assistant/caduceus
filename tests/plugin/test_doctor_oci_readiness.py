"""Chained binary OCI-readiness check tests (issue #414, AC-1).

The wrapper chains the binary's ``caduceus doctor --json --skip-canary``
and maps the report verdict into the unified report. The subprocess is
stubbed (``_run``) rather than exercised through the shared fake binary:
``tests/conftest.py::install_with_fake_binary`` has no ``doctor`` arm, and
stubbing keeps these tests deterministic under parallel threads.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any, Dict, Iterator, List, Optional

import pytest

from tests.plugin._helpers import _stub_cron_runtime


READY_REPORT: Dict[str, Any] = {
    "schema_version": "1.0.0",
    "generated_at": "2026-09-12T15:40:24.394878683Z",
    "verdict": "READY",
    "checks": [
        {"id": "platform", "status": "pass", "detail": "Linux host", "remediation": None},
        {"id": "engine", "status": "pass", "detail": "docker daemon is reachable", "remediation": None},
    ],
}

# Trusted-host boxes (the default executor_mode) have no sandbox section,
# so the binary reports a single Engine failure and exits 1.
TRUSTED_HOST_REPORT: Dict[str, Any] = {
    "schema_version": "1.0.0",
    "generated_at": "2026-09-12T15:40:24.394878683Z",
    "verdict": "UNAVAILABLE",
    "checks": [
        {
            "id": "engine",
            "status": "fail",
            "detail": "no sandbox configuration is present",
            "remediation": (
                "configure executor_mode: oci and a complete sandbox section, "
                "or keep trusted_host mode"
            ),
        }
    ],
}


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


def _stub_doctor(
    adapter,
    monkeypatch: pytest.MonkeyPatch,
    *,
    stdout: str = "",
    returncode: int = 0,
    error: Optional[str] = None,
    argv_sink: Optional[List[List[str]]] = None,
) -> None:
    """Replace ``_run`` so the chained binary-doctor call is canned.

    The tick-freshness check shares ``_run``; it gets a benign
    never-ticked status payload so this suite isolates the OCI check.
    """

    def fake_run(argv, *, cwd=None, timeout=None):
        sub = argv[1] if len(argv) > 1 else ""
        if argv_sink is not None:
            argv_sink.append(list(argv))
        if sub == "doctor":
            if error is not None:
                raise RuntimeError(error)
            return subprocess.CompletedProcess(argv, returncode, stdout, "")
        return subprocess.CompletedProcess(
            argv, 0, json.dumps({"diagnostic": None, "report": {}}), ""
        )

    monkeypatch.setattr(adapter, "_run", fake_run)


def test_oci_readiness_ready_is_ok(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """verdict READY renders as [OK] and keeps the doctor exit code at 0."""
    _stub_doctor(adapter, monkeypatch, stdout=json.dumps(READY_REPORT))

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 0
    assert "[OK] OCI Readiness — OCI readiness: READY (2 checks, all passed)" in out


def test_oci_readiness_unavailable_is_warn(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Trusted-host UNAVAILABLE is advisory: [WARN], exit code unchanged (D2)."""
    _stub_doctor(adapter, monkeypatch, stdout=json.dumps(TRUSTED_HOST_REPORT))

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 0
    assert (
        "[WARN] OCI Readiness — OCI readiness: UNAVAILABLE (1 check, 1 failed) "
        "— no sandbox configuration is present" in out
    )


def test_oci_readiness_binary_missing_is_ok_skipped(
    adapter,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Without a binary the chained check is skipped, not failed (D4).

    The Binary check owns "binary missing" and drives exit 2; the OCI
    check must not double-report it.
    """
    _stub_doctor(adapter, monkeypatch, stdout=json.dumps(READY_REPORT))

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 2
    assert (
        "[OK] OCI Readiness — binary OCI doctor skipped (binary not installed "
        "— see Binary check)" in out
    )


def test_oci_readiness_timeout_is_warn(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A hung/timed-out binary doctor is advisory, never blocking (D8)."""
    _stub_doctor(
        adapter,
        monkeypatch,
        error="timeout after 15s running /plugin/bin/caduceus doctor --json --skip-canary",
    )

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 0
    assert "[WARN] OCI Readiness — binary OCI doctor could not run (timeout after 15s" in out


def test_oci_readiness_malformed_json_is_warn(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Unparsable stdout is advisory and carries the exit code in the detail."""
    _stub_doctor(adapter, monkeypatch, stdout="not json", returncode=1)

    rc = adapter._cli_doctor()
    out = capsys.readouterr().out

    assert rc == 0
    assert (
        "[WARN] OCI Readiness — binary OCI doctor returned no usable report (exit 1)"
        in out
    )


def test_oci_readiness_skip_canary_in_argv(
    adapter,
    install_with_fake_binary: Path,
    healthy_env: None,
    capsys: pytest.CaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The chained call skips the diagnostic canary to stay inside the timeout (D8)."""
    argv_sink: List[List[str]] = []
    _stub_doctor(adapter, monkeypatch, stdout=json.dumps(READY_REPORT), argv_sink=argv_sink)

    rc = adapter._cli_doctor()
    capsys.readouterr()

    doctor_calls = [argv for argv in argv_sink if len(argv) > 1 and argv[1] == "doctor"]
    assert rc == 0
    assert len(doctor_calls) == 1
    assert doctor_calls[0][-2:] == ["--json", "--skip-canary"]
    assert doctor_calls[0][0] == str(adapter._binary_path())

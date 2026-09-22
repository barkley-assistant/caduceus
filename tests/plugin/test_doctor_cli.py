"""Doctor CLI exit-code/report tests."""

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

from tests.plugin._helpers import _stub_cron_runtime


def test_doctor_exit_0_when_all_healthy(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path, monkeypatch, capsys
) -> None:
    """_cli_doctor returns 0 when all checks pass (AC-06)."""
    from caduceus import _runtime

    # Set up healthy environment: binary exists, bridge is executable,
    # cron works, and provider secret is configured.
    monkeypatch.setenv("CADUCEUS_GITHUB_TOKEN", "ghp_test-secret-configured")
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)

    registry = {}
    _stub_cron_runtime(adapter, registry)

    # The shared fake binary has no `doctor` arm, so stub the chained
    # calls: a READY OCI report and a one-minute-old tick (issue #414).
    recent = (datetime.now(timezone.utc) - timedelta(minutes=1)).isoformat()
    doctor_json = json.dumps(
        {
            "schema_version": "1.0.0",
            "verdict": "READY",
            "checks": [{"id": "engine", "status": "pass", "detail": "ok"}],
        }
    )
    status_json = json.dumps({"diagnostic": None, "report": {"last_tick_started": recent}})

    def fake_run(argv, *, cwd=None, timeout=None):
        sub = argv[1] if len(argv) > 1 else ""
        if sub == "doctor":
            return subprocess.CompletedProcess(argv, 0, doctor_json, "")
        if sub == "status":
            return subprocess.CompletedProcess(argv, 0, status_json, "")
        return subprocess.CompletedProcess(argv, 0, "", "")

    monkeypatch.setattr(adapter, "_run", fake_run)
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()
    out = capsys.readouterr().out
    assert rc == 0
    assert "[OK] OCI Readiness — OCI readiness: READY (1 check, all passed)" in out
    assert "[OK] Tick Freshness — last tick 1 minute ago" in out




def test_doctor_exit_1_for_config_defect(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path
) -> None:
    """_cli_doctor returns 1 for config-incomplete or daemon-defect (AC-08)."""
    from caduceus import _runtime

    # Binary present, bridge executable, cron works — but provider secret
    # is missing (config-incomplete).
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)

    registry = {}
    _stub_cron_runtime(adapter, registry)

    # Make provider secret check return fail (config-incomplete).
    original_secret = adapter._doctor_check_provider_secret
    def _failing_secret():
        from caduceus import _DoctorFinding
        return _DoctorFinding(
            category="config-incomplete",
            status="fail",
            detail="provider secret not configured",
            next_action="set HERMES_PROVIDER_SECRET in environment",
        )

    try:
        adapter._doctor_check_provider_secret = _failing_secret  # type: ignore[assignment]
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()
        adapter._doctor_check_provider_secret = original_secret
    assert rc == 1




def test_doctor_exit_2_for_missing_binary(
    adapter, isolated_hermes_home: Path
) -> None:
    """_cli_doctor returns 2 for host-capability-unavailable (AC-11)."""
    from caduceus import _runtime

    # No binary installed — exit 2.
    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()
    assert rc == 2




def test_doctor_exit_2_takes_precedence_over_exit_1(
    adapter, isolated_hermes_home: Path
) -> None:
    """When both exit-1 and exit-2 failures exist, exit 2 wins (design #9)."""
    from caduceus import _runtime

    # Binary missing (exit 2) AND config defect (exit 1) — exit 2 wins.
    registry = {}
    _stub_cron_runtime(adapter, registry)
    original_secret = adapter._doctor_check_provider_secret
    def _failing_secret():
        from caduceus import _DoctorFinding
        return _DoctorFinding(
            category="config-incomplete",
            status="fail",
            detail="provider secret not configured",
            next_action="set HERMES_PROVIDER_SECRET in environment",
        )

    try:
        adapter._doctor_check_provider_secret = _failing_secret  # type: ignore[assignment]
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()
        adapter._doctor_check_provider_secret = original_secret
    assert rc == 2




def test_doctor_prints_operator_finding(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path, capsys: pytest.CaptureFixture, monkeypatch
) -> None:
    """_cli_doctor prints each finding on one operator-readable line (AC-07)."""
    from caduceus import _runtime

    monkeypatch.setenv("CADUCEUS_GITHUB_TOKEN", "ghp_test-secret-configured")
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()

    captured = capsys.readouterr()
    assert rc == 0
    assert "[OK] Binary —" in captured.out
    assert "[OK] Bridge Harness —" in captured.out
    assert "[OK] Provider Secret —" in captured.out
    assert "[OK] Cron Capability —" in captured.out
    assert "[OK] Hermes Home —" in captured.out
    assert "       detail:      " not in captured.out
    assert "       category:    " not in captured.out




def test_doctor_default_does_not_print_internal_detail(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path, capsys: pytest.CaptureFixture, monkeypatch
) -> None:
    """Default output is operator-only: no internal detail or category lines."""
    from caduceus import _runtime

    monkeypatch.setenv("CADUCEUS_GITHUB_TOKEN", "ghp_test-secret-configured")
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()

    captured = capsys.readouterr()
    assert "       detail:      " not in captured.out
    assert "       category:    " not in captured.out




def test_doctor_verbose_prints_internal_detail(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path, capsys: pytest.CaptureFixture, monkeypatch
) -> None:
    """--verbose adds the internal detail and category on FAIL lines."""
    from caduceus import _runtime

    monkeypatch.setenv("CADUCEUS_GITHUB_TOKEN", "ghp_test-secret-configured")
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        adapter._cli_doctor(verbose=True)
    finally:
        _runtime.reset_dispatcher()

    captured = capsys.readouterr()
    assert "       detail:      cron list returned 0 Caduceus jobs" in captured.out




def test_doctor_verbose_honoured_on_ci(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path, capsys: pytest.CaptureFixture, monkeypatch
) -> None:
    """The verbose flag is always honoured when the operator passes it.

    CI log hygiene is achieved by the default output being operator-only,
    not by overriding an explicit verbose flag. Operators running
    ``--verbose`` from a CI shell (e.g. for debugging) get the internal
    detail. This test pins that contract.
    """
    from caduceus import _runtime

    monkeypatch.setenv("CADUCEUS_GITHUB_TOKEN", "ghp_test-secret-configured")
    monkeypatch.setenv("CI", "1")
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        adapter._cli_doctor(verbose=True)
    finally:
        _runtime.reset_dispatcher()

    captured = capsys.readouterr()
    assert "       detail:      cron list returned 0 Caduceus jobs" in captured.out




def test_doctor_output_never_contains_malformed_response(
    adapter, install_with_fake_binary: Path, capsys: pytest.CaptureFixture
) -> None:
    """The literal substring 'malformed-response:' does not reach a non-CI operator."""
    from caduceus import _runtime
    from caduceus._runtime import CronCapabilityError
    from tests.plugin._helpers import subprocess_run_recorder

    def raise_malformed(argv, kwargs):
        raise CronCapabilityError(
            "malformed-response",
            "Hermes returned an unexpected payload shape",
            "garbled",
        )

    with subprocess_run_recorder({"list": raise_malformed}):
        rc = adapter._cli_doctor()
        captured = capsys.readouterr()

    assert rc == 2
    assert "malformed-response:" not in captured.out
    assert "[FAIL] Cron Capability — Hermes returned an unexpected payload shape" in captured.out




def test_doctor_prints_failures_on_exit_2(
    adapter, capsys: pytest.CaptureFixture
) -> None:
    """_cli_doctor prints failure details when exiting 2."""
    from caduceus import _runtime

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()

    captured = capsys.readouterr()
    assert rc == 2
    # Should show what failed.
    assert "fail" in captured.out.lower() or "FAIL" in captured.out



def test_doctor_exit_1_when_worktree_lock_stale(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path, monkeypatch
) -> None:
    """A stale .worktrees/.lock makes _cli_doctor exit 1 (daemon-defect)."""
    from caduceus import _runtime

    monkeypatch.setenv("CADUCEUS_GITHUB_TOKEN", "ghp_test-secret-configured")
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)

    lock = (
        isolated_hermes_home
        / "projects"
        / "octocat"
        / "Hello-World"
        / ".worktrees"
        / ".lock"
    )
    lock.parent.mkdir(parents=True, exist_ok=True)
    lock.write_text("")

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()
    assert rc == 1


# ---------------------------------------------------------------------------
# ``hermes caduceus doctor --json`` (issue #417)
# ---------------------------------------------------------------------------


def _healthy_install(adapter, isolated_hermes_home: Path, monkeypatch) -> None:
    """Install the all-healthy fixture: token, bridge executable, cron stub."""
    monkeypatch.setenv("CADUCEUS_GITHUB_TOKEN", "ghp_test-secret-configured")
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)
    _stub_cron_runtime(adapter, {})


def _stub_ready_chain(adapter, monkeypatch) -> None:
    """Stub the chained binary calls: READY OCI readiness, fresh tick."""
    recent = (datetime.now(timezone.utc) - timedelta(minutes=1)).isoformat()
    doctor_json = json.dumps(
        {
            "schema_version": "1.0.0",
            "verdict": "READY",
            "checks": [{"id": "engine", "status": "pass", "detail": "ok"}],
        }
    )
    status_json = json.dumps({"diagnostic": None, "report": {"last_tick_started": recent}})

    def fake_run(argv, *, cwd=None, timeout=None):
        sub = argv[1] if len(argv) > 1 else ""
        if sub == "doctor":
            return subprocess.CompletedProcess(argv, 0, doctor_json, "")
        if sub == "status":
            return subprocess.CompletedProcess(argv, 0, status_json, "")
        return subprocess.CompletedProcess(argv, 0, "", "")

    monkeypatch.setattr(adapter, "_run", fake_run)


def _failing_provider_secret(adapter):
    """Patch the provider-secret check to a config-incomplete failure."""
    from caduceus import _DoctorFinding

    original = adapter._doctor_check_provider_secret
    adapter._doctor_check_provider_secret = lambda: _DoctorFinding(  # type: ignore[assignment]
        category="config-incomplete",
        status="fail",
        detail="provider secret not configured",
        next_action="set HERMES_PROVIDER_SECRET in environment",
    )
    return original


_CHECK_KEYS = {"name", "status", "category", "detail", "next_action", "internal_detail"}


def test_doctor_json_emits_parseable_report_with_severity_zero(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path,
    monkeypatch, capsys: pytest.CaptureFixture,
) -> None:
    """--json on a healthy box: one document, severity 0, nine checks."""
    from caduceus import _runtime

    _healthy_install(adapter, isolated_hermes_home, monkeypatch)
    _stub_ready_chain(adapter, monkeypatch)
    try:
        rc = adapter._cli_doctor(json_mode=True)
    finally:
        _runtime.reset_dispatcher()

    out = capsys.readouterr().out
    doc = json.loads(out)
    assert rc == 0
    assert doc["command"] == "hermes caduceus doctor"
    assert doc["severity"] == 0
    assert doc["severity_label"] == "ok"
    assert isinstance(doc["checks"], list) and len(doc["checks"]) == 9
    for check in doc["checks"]:
        assert set(check) == _CHECK_KEYS, f"unexpected check shape: {sorted(check)}"
        assert check["status"] in {"ok", "warn"}, check


def test_doctor_json_severity_matches_exit_code_1(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path,
    monkeypatch, capsys: pytest.CaptureFixture,
) -> None:
    """A config defect is severity 1 in JSON and exit 1 in the human path."""
    from caduceus import _runtime

    _healthy_install(adapter, isolated_hermes_home, monkeypatch)
    original_secret = _failing_provider_secret(adapter)
    try:
        rc = adapter._cli_doctor(json_mode=True)
    finally:
        _runtime.reset_dispatcher()
        adapter._doctor_check_provider_secret = original_secret

    doc = json.loads(capsys.readouterr().out)
    assert rc == 1
    assert doc["severity"] == 1
    assert doc["severity_label"] == "config-runtime"
    secret = next(c for c in doc["checks"] if c["name"] == "Provider Secret")
    assert secret["status"] == "fail"
    assert secret["category"] == "config-incomplete"


def test_doctor_json_severity_2_wins_over_1(
    adapter, isolated_hermes_home: Path, capsys: pytest.CaptureFixture
) -> None:
    """A missing binary (severity 2) outranks a config defect (severity 1)."""
    from caduceus import _runtime

    from tests.plugin._helpers import _stub_cron_runtime as _stub

    _stub(adapter, {})
    original_secret = _failing_provider_secret(adapter)
    try:
        rc = adapter._cli_doctor(json_mode=True)
    finally:
        _runtime.reset_dispatcher()
        adapter._doctor_check_provider_secret = original_secret

    doc = json.loads(capsys.readouterr().out)
    assert rc == 2
    assert doc["severity"] == 2
    assert doc["severity_label"] == "host-capability-unavailable"


def test_doctor_json_is_single_json_document(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path,
    monkeypatch, capsys: pytest.CaptureFixture,
) -> None:
    """--json replaces the human report instead of appending to it."""
    from caduceus import _runtime

    _healthy_install(adapter, isolated_hermes_home, monkeypatch)
    _stub_ready_chain(adapter, monkeypatch)
    try:
        adapter._cli_doctor(json_mode=True)
    finally:
        _runtime.reset_dispatcher()

    out = capsys.readouterr().out
    assert json.loads(out.strip())  # whole stdout is one document
    assert "[OK]" not in out and "[FAIL]" not in out and "[WARN]" not in out
    assert "\x1b[" not in out


def test_doctor_json_ignores_verbose(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path,
    monkeypatch, capsys: pytest.CaptureFixture,
) -> None:
    """--verbose adds nothing in JSON mode: every field is always present."""
    from caduceus import _runtime

    _healthy_install(adapter, isolated_hermes_home, monkeypatch)
    _stub_ready_chain(adapter, monkeypatch)
    try:
        rc = adapter._cli_doctor(verbose=True, json_mode=True)
    finally:
        _runtime.reset_dispatcher()

    out = capsys.readouterr().out
    doc = json.loads(out)
    assert rc == 0
    assert set(doc) == {"command", "severity", "severity_label", "checks"}
    for check in doc["checks"]:
        assert set(check) == _CHECK_KEYS
    assert "detail:" not in out
    assert "\x1b[" not in out


def test_doctor_cli_dispatches_json_flag(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path,
    monkeypatch, fake_ctx: FakePluginContext, capsys: pytest.CaptureFixture,
) -> None:
    """``hermes caduceus doctor --json`` reaches the JSON path (issue #417).

    The flag-to-handler wiring is what this pins: the human path stays the
    default when the flag is absent.
    """
    from caduceus import _runtime

    _healthy_install(adapter, isolated_hermes_home, monkeypatch)
    _stub_ready_chain(adapter, monkeypatch)
    adapter.register(fake_ctx)
    parser = fake_ctx.cli_commands["caduceus"].parser

    args = parser.parse_args(["doctor", "--json"])
    try:
        rc = args.func(args)
    finally:
        _runtime.reset_dispatcher()
    doc = json.loads(capsys.readouterr().out)
    assert rc == 0
    assert doc["severity"] == 0
    assert len(doc["checks"]) == 9

    # ``_runtime.reset_dispatcher()`` above wipes the cron stub set by
    # ``_stub_cron_runtime`` (the helper writes ``_runtime._subprocess_run``
    # and ``_runtime._HERMES_PATH`` directly without ``monkeypatch.setattr``,
    # so the reset is destructive). Restub before the second invocation so the
    # human path sees the same healthy fixture the JSON path saw; otherwise
    # the second ``_cli_doctor`` call would invoke real ``subprocess.run`` and
    # raise ``CronCapabilityError`` on a hermetic CI runner with no ``hermes``
    # on PATH, mapping to ``max_severity = 2`` and breaking this assertion.
    _stub_cron_runtime(adapter, {})

    args = parser.parse_args(["doctor"])
    try:
        rc = args.func(args)
    finally:
        _runtime.reset_dispatcher()
    out = capsys.readouterr().out
    assert rc == 0
    assert "[OK] Binary —" in out

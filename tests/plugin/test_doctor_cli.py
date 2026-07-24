"""Doctor CLI exit-code/report tests."""

from __future__ import annotations

import json
import os
import re
import shutil
import stat
import subprocess
import sys
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
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path, monkeypatch
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
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()
    assert rc == 0




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




def test_doctor_prints_structured_report(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path, capsys: pytest.CaptureFixture, monkeypatch
) -> None:
    """_cli_doctor prints each finding with status, detail, next_action (AC-07)."""
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
    # Each finding category should appear in the output.
    assert "binary" in captured.out.lower() or "Binary" in captured.out
    assert "cron" in captured.out.lower() or "Cron" in captured.out
    assert "bridge" in captured.out.lower() or "Bridge" in captured.out
    assert "ok" in captured.out.lower() or "OK" in captured.out




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

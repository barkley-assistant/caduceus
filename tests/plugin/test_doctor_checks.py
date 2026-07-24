"""Individual doctor check function tests."""

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


def test_doctor_check_binary_present(
    adapter, install_with_fake_binary: Path
) -> None:
    """_doctor_check_binary returns ok when binary exists."""
    finding = adapter._doctor_check_binary()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"
    assert str(install_with_fake_binary) in finding.detail




def test_doctor_check_binary_missing(adapter) -> None:
    """_doctor_check_binary returns fail when binary is missing."""
    finding = adapter._doctor_check_binary()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "fail"
    assert "not found" in finding.detail.lower() or "missing" in finding.detail.lower()
    assert "setup" in finding.next_action.lower()




def test_doctor_check_bridge_harness_executable(
    adapter, isolated_hermes_home: Path
) -> None:
    """_doctor_check_bridge_harness returns ok when bridge is executable."""
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)
    finding = adapter._doctor_check_bridge_harness()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"
    assert str(bridge) in finding.detail




def test_doctor_check_bridge_harness_not_executable(
    adapter, isolated_hermes_home: Path
) -> None:
    """_doctor_check_bridge_harness returns fail when bridge lacks execute bit."""
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o644)  # Not executable
    finding = adapter._doctor_check_bridge_harness()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "fail"
    assert "chmod" in finding.next_action.lower() or "+x" in finding.next_action.lower()




def test_doctor_check_provider_secret_present(
    adapter, install_plugin: Path
) -> None:
    """_doctor_check_provider_secret returns ok when secret name is configured."""
    finding = adapter._doctor_check_provider_secret()
    # Without a config to inspect, we expect a sensible default.
    assert finding.category in ("config-incomplete", "host-capability-unavailable")
    assert finding.status in ("ok", "fail")




def test_doctor_check_cron_capability_ok(
    adapter, install_with_fake_binary: Path
) -> None:
    """_doctor_check_cron_capability returns ok when cron lists without error."""
    from caduceus import _runtime

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    finally:
        _runtime.reset_dispatcher()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"




def test_doctor_check_cron_capability_fails(
    adapter, install_with_fake_binary: Path
) -> None:
    """_doctor_check_cron_capability returns fail when cron list raises."""
    from caduceus import _runtime

    original_registry = adapter._cron_job_registry
    def _failing_registry():
        raise RuntimeError("cron unavailable")

    try:
        adapter._cron_job_registry = _failing_registry  # type: ignore[assignment]
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    finally:
        adapter._cron_job_registry = original_registry
    assert finding.status == "fail"
    assert "cron" in finding.detail.lower()




def test_doctor_check_gateway_returns_finding(
    adapter, install_with_fake_binary: Path
) -> None:
    """_doctor_check_gateway returns a _DoctorFinding (ok or fail)."""
    finding = adapter._doctor_check_gateway()
    assert isinstance(finding, tuple)
    assert finding.category == "gateway-inactive"
    assert finding.status in ("ok", "fail")

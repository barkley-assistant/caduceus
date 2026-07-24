"""Individual doctor check function tests."""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Dict

import pytest

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
    assert "run setup to build it" in finding.detail
    assert "hermes caduceus setup" in finding.next_action



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
    assert "worker bridge" in finding.detail.lower()



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
    assert "worker bridge" in finding.detail.lower()
    assert "chmod" in finding.next_action.lower() or "+x" in finding.next_action.lower()



def test_doctor_check_bridge_harness_not_yet_seeded(
    adapter, isolated_hermes_home: Path
) -> None:
    """A missing bridge is OK but framed as an external prerequisite."""
    finding = adapter._doctor_check_bridge_harness()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"
    assert "worker bridge not yet seeded" in finding.detail.lower()
    assert "external prerequisite" in finding.detail.lower()



def test_doctor_check_provider_secret_present(
    adapter, install_plugin: Path, monkeypatch
) -> None:
    """_doctor_check_provider_secret returns ok when secret name is configured."""
    monkeypatch.setenv("GITHUB_TOKEN", "ghp_test-secret-name")
    finding = adapter._doctor_check_provider_secret()
    assert finding.category == "config-incomplete"
    assert finding.status == "ok"
    assert "provider secret name GITHUB_TOKEN is configured" in finding.detail
    assert "no value read" in finding.detail



def test_doctor_check_provider_secret_missing(adapter) -> None:
    """_doctor_check_provider_secret returns fail when no secret name is set."""
    for var in ("CADUCEUS_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"):
        os.environ.pop(var, None)
    finding = adapter._doctor_check_provider_secret()
    assert finding.category == "config-incomplete"
    assert finding.status == "fail"
    assert "no provider secret name configured" in finding.detail.lower()
    assert finding.next_action.startswith("set one of")



def test_doctor_check_cron_capability_ok_with_jobs(
    adapter, install_with_fake_binary: Path
) -> None:
    """_doctor_check_cron_capability returns ok with caduceus job count."""
    from caduceus import _runtime

    registry = {
        "abc": {"id": "abc", "name": "caduceus", "schedule": "every 2m"},
    }
    _stub_cron_runtime(adapter, registry)
    try:
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    finally:
        _runtime.reset_dispatcher()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"
    assert "1 Caduceus cron job registered" in finding.detail
    assert "external prerequisite, exercised" in finding.detail



def test_doctor_check_cron_capability_no_caduceus_job(
    adapter, install_with_fake_binary: Path
) -> None:
    """A reachable cron subsystem with no caduceus job is a prerequisite."""
    from caduceus import _runtime

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    finally:
        _runtime.reset_dispatcher()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"
    assert "no Caduceus cron job registered yet" in finding.detail
    assert "external prerequisite, not exercised" in finding.detail
    assert "hermes caduceus cron-install" in finding.next_action



def test_doctor_check_cron_capability_hermes_not_on_path(
    adapter, install_with_fake_binary: Path
) -> None:
    """Missing hermes CLI points at PATH/install, not the gateway state."""
    from caduceus import _runtime

    _runtime.reset_dispatcher()
    _runtime._HERMES_PATH = None
    finding = adapter._doctor_check_cron_capability(ctx=adapter)
    assert finding.status == "fail"
    assert "hermes cli not on path" in finding.detail.lower()
    assert finding.next_action.startswith("install Hermes Agent")
    assert "hermes" in finding.next_action



def test_doctor_check_gateway_renamed_to_hermes_home(
    adapter, install_with_fake_binary: Path
) -> None:
    """_doctor_check_hermes_home returns a _DoctorFinding with the new label surface."""
    finding = adapter._doctor_check_hermes_home()
    assert isinstance(finding, tuple)
    assert finding.category == "gateway-inactive"
    assert finding.status in ("ok", "fail")

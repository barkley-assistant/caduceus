"""Cron install reconciliation tests."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Any, Dict

import pytest

from tests.plugin._helpers import _stub_cron_runtime


def test_cron_install_zero_matches_creates(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    from caduceus import _runtime

    registry: Dict[str, Dict[str, Any]] = {}
    _stub_cron_runtime(adapter, registry)
    try:
        action, note = adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()
    assert action == "created"
    assert len(note) >= 8  # hex job id from the subprocess recorder
    caduceus_jobs = [j for j in registry.values() if j.get("name") == "caduceus"]
    assert len(caduceus_jobs) == 1
    assert caduceus_jobs[0]["schedule"] == "every 2m"
    # Wrapper was written.
    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    assert wrapper.is_file()



def test_cron_install_one_match_reuses(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    from caduceus import _runtime

    registry = {
        "abc": {
            "id": "abc",
            "name": "caduceus",
            "schedule": "every 5m",
            "script": "caduceus-pulse.sh",
            "no_agent": False,
        }
    }
    _stub_cron_runtime(adapter, registry)
    try:
        action, note = adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()
    assert action == "reused"
    assert note == "abc"
    # The original job is removed+recreated so one caduceus job remains.
    caduceus_jobs = [j for j in registry.values() if j.get("name") == "caduceus"]
    assert len(caduceus_jobs) == 1
    assert caduceus_jobs[0]["schedule"] == "every 2m"
    assert caduceus_jobs[0]["no_agent"] is True
    assert caduceus_jobs[0]["script"] == "caduceus-pulse.sh"



def test_cron_install_multiple_matches_fails(
    adapter, install_with_fake_binary: Path
) -> None:
    from caduceus import _runtime

    registry = {
        "aaaaaaaaaaaa": {"id": "aaaaaaaaaaaa", "name": "caduceus", "schedule": "every 2m"},
        "bbbbbbbbbbbb": {"id": "bbbbbbbbbbbb", "name": "caduceus", "schedule": "every 2m"},
    }
    _stub_cron_runtime(adapter, registry)
    try:
        with pytest.raises(RuntimeError) as excinfo:
            adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()
    assert "multiple" in str(excinfo.value).lower()



def test_cron_install_invokes_no_agent_exec(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path, tmp_path: Path
) -> None:
    """The no-agent cron job is created by exec'ing the bash wrapper."""
    adapter._write_pulse_wrapper(install_with_fake_binary)
    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    # ``exec <binary> run`` only invokes run; we cannot reuse the
    # status-only fake — write a richer stub binary.
    fake = install_with_fake_binary
    fake.write_text("#!/usr/bin/env bash\necho run-ok\nexit 0\n")
    fake.chmod(0o755)
    proc = subprocess.run(
        [str(wrapper)], capture_output=True, text=True, timeout=10
    )
    assert proc.returncode == 0
    assert "run-ok" in proc.stdout

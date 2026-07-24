"""Cron install reconciliation tests."""

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


def test_cron_install_zero_matches_creates(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    from caduceus import _runtime

    registry: Dict[str, Dict[str, Any]] = {}
    actions = _stub_cron_runtime(adapter, registry)
    try:
        action, note = adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()
    assert action == "created"
    assert note.startswith("job-")
    assert any(a["action"] == "create" for a in actions)
    assert any(a["action"] == "list" for a in actions)
    # Wrapper was written.
    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    assert wrapper.is_file()




def test_cron_install_one_match_reuses(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    from caduceus import _runtime

    registry = {
        "job-9": {
            "id": "job-9",
            "name": "caduceus",
            "schedule": "every 5m",
            "script": "caduceus-pulse.sh",
            "no_agent": False,
        }
    }
    actions = _stub_cron_runtime(adapter, registry)
    try:
        action, note = adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()
    assert action == "reused"
    assert note == "job-9"
    # update was invoked with the new schedule and no_agent=True.
    update = next(a for a in actions if a["action"] == "update")
    assert update["schedule"] == "every 2m"
    assert update["no_agent"] is True
    assert update["script"] == "caduceus-pulse.sh"




def test_cron_install_multiple_matches_fails(
    adapter, install_with_fake_binary: Path
) -> None:
    from caduceus import _runtime

    registry = {
        "job-a": {"id": "job-a", "name": "caduceus", "schedule": "every 2m"},
        "job-b": {"id": "job-b", "name": "caduceus", "schedule": "every 2m"},
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

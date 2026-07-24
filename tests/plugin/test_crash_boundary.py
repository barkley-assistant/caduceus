"""Crash-boundary idempotency tests."""

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

from tests.plugin._helpers import _stub_wrapper_file, _stub_cron_runtime


def test_cron_install_crash_after_wrapper_before_create_is_idempotent(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """Re-running cron-install after a crash between wrapper write and
    job create converges to the single intended state."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"

    # First run: write wrapper, then fail on cron list (simulating crash).
    adapter._write_pulse_wrapper(install_with_fake_binary)
    assert wrapper.is_file()

    # Second run should succeed and create the job.
    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        action, note = adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()

    assert action == "created"
    assert wrapper.is_file()




def test_cron_remove_crash_after_remove_before_wrapper_delete(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """Re-running cron-remove after a crash between job remove and
    wrapper delete converges to clean state."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    # First run: remove the job manually (simulating crash after remove).
    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_cron_remove()
    finally:
        _runtime.reset_dispatcher()

    # Should succeed — job is already gone, wrapper gets removed.
    assert rc == 0
    assert not wrapper.exists()




def test_cron_install_crash_between_create_and_update_is_idempotent(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """If cron-install crashes after creating a job but before the
    reconcile check, a second run updates the existing job."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    # Pre-seed a caduceus job with stale schedule (simulating partial state).
    registry = {
        "abc": {
            "id": "abc",
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
    # The job should have been updated.
    assert registry["abc"]["schedule"] == "every 2m"
    assert registry["abc"]["no_agent"] is True

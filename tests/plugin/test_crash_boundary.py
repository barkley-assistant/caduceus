"""Crash-boundary idempotency tests."""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Dict

import pytest

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
    registry: Dict[str, Dict[str, Any]] = {}
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
    registry: Dict[str, Dict[str, Any]] = {}
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
    _stub_cron_runtime(adapter, registry)
    try:
        action, note = adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()

    assert action == "reused"
    # The original job is removed and recreated with the desired state.
    caduceus_jobs = [j for j in registry.values() if j.get("name") == "caduceus"]
    assert len(caduceus_jobs) == 1
    assert caduceus_jobs[0]["schedule"] == "every 2m"
    assert caduceus_jobs[0]["no_agent"] is True

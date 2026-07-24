"""Cron remove snapshot-before-mutation tests."""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Dict

import pytest

from tests.plugin._helpers import _stub_wrapper_file, _stub_cron_runtime


def test_cron_remove_snapshots_before_mutation(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """_cli_cron_remove snapshots wrapper and job before removal (AC-01/03)."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    registry = {
        "abc": {
            "id": "abc",
            "name": "caduceus",
            "schedule": "every 2m",
        }
    }
    actions = _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_cron_remove()
    finally:
        _runtime.reset_dispatcher()

    assert rc == 0
    # List should happen before remove.
    list_actions = [a for a in actions if a["action"] == "list"]
    remove_actions = [a for a in actions if a["action"] == "remove"]
    assert len(list_actions) >= 1
    if remove_actions:
        assert actions.index(list_actions[0]) < actions.index(remove_actions[0])
    assert not wrapper.exists()



def test_cron_remove_reconciles_on_failure(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """When cron-remove fails mid-way, reconcile restores stable state."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)
    original_bytes = wrapper.read_bytes()

    registry = {
        "abc": {
            "id": "abc",
            "name": "caduceus",
            "schedule": "every 2m",
        }
    }
    actions = _stub_cron_runtime(adapter, registry)

    # Make cron_remove_job fail on first call.
    original_remove = adapter._cronjob_remove
    fail_count = [0]

    def _failing_remove(job_id: str):
        fail_count[0] += 1
        if fail_count[0] == 1:
            raise RuntimeError("simulated remove failure")
        return original_remove(job_id)

    adapter._cronjob_remove = _failing_remove  # type: ignore[assignment]
    try:
        rc = adapter._cli_cron_remove()
    finally:
        _runtime.reset_dispatcher()
        adapter._cronjob_remove = original_remove

    # Reconcile restored stable state — wrapper and job are preserved.
    assert rc == 0
    assert wrapper.is_file()
    assert wrapper.read_bytes() == original_bytes
    # A caduceus job still exists after reconcile re-created it.
    caduceus_jobs = [j for j in registry.values() if j.get("name") == "caduceus"]
    assert len(caduceus_jobs) == 1

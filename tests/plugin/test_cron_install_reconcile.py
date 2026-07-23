"""Cron install snapshot-before-mutation tests."""

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


def test_cron_install_snapshots_before_mutation(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """_cron_install snapshots the wrapper and job before any mutation (AC-01)."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    registry = {}
    actions = _stub_cron_runtime(adapter, registry)
    try:
        action, note = adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()

    assert action in ("created", "reused")
    # The first list action is the snapshot — it happens before create.
    list_actions = [a for a in actions if a["action"] == "list"]
    create_actions = [a for a in actions if a["action"] == "create"]
    assert len(list_actions) >= 1
    if create_actions:
        assert actions.index(list_actions[0]) < actions.index(create_actions[0])




def test_cron_install_checks_capability_before_wrapper(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """Cron capability is checked BEFORE the wrapper is written."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    registry = {}
    _stub_cron_runtime(adapter, registry)

    # Patch _cron_job_registry to raise CronCapabilityError(denied).
    original_registry = adapter._cron_job_registry
    def _failing_registry():
        raise _runtime.CronCapabilityError("denied", "cron denied")

    try:
        adapter._cron_job_registry = _failing_registry  # type: ignore[assignment]
        with pytest.raises((_runtime.CronCapabilityError, RuntimeError)):
            adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()
        adapter._cron_job_registry = original_registry

    # Wrapper should NOT have been written — capability check fails first.
    assert not wrapper.exists(), (
        "wrapper should not exist when cron capability check fails"
    )

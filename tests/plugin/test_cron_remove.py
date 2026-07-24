"""Cron remove idempotency tests."""

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


def test_cron_remove_is_idempotent(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    from caduceus import _runtime

    registry = {
        "job-9": {
            "id": "job-9",
            "name": "caduceus",
            "schedule": "every 2m",
            "script": "caduceus-pulse.sh",
        }
    }
    actions = _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_cron_remove()
    finally:
        _runtime.reset_dispatcher()
    assert rc == 0
    assert any(a["action"] == "remove" for a in actions)
    assert "job-9" not in registry
    # Wrapper is gone.
    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    assert not wrapper.exists()
    # Idempotent: a second call still returns 0.
    actions.clear()
    registry.pop("job-9", None)
    try:
        _stub_cron_runtime(adapter, registry)
        rc = adapter._cli_cron_remove()
    finally:
        _runtime.reset_dispatcher()
    assert rc == 0

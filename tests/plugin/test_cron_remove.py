"""Cron remove idempotency tests."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Any, Dict

import pytest

from tests.plugin._helpers import _stub_cron_runtime


def test_cron_remove_is_idempotent(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    from caduceus import _runtime

    registry = {
        "abc": {
            "id": "abc",
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
    assert "abc" not in registry
    # Wrapper is gone.
    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    assert not wrapper.exists()
    # Idempotent: a second call still returns 0.
    actions.clear()
    registry.pop("abc", None)
    try:
        _stub_cron_runtime(adapter, registry)
        rc = adapter._cli_cron_remove()
    finally:
        _runtime.reset_dispatcher()
    assert rc == 0

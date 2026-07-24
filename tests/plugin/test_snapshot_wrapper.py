"""Wrapper/job snapshot tests."""

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


def test_snapshot_wrapper_and_job_captures_bytes_and_mode(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """_snapshot_wrapper_and_job captures wrapper bytes, mode, and matching job."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    registry = {
        "abc": {
            "id": "abc",
            "name": "caduceus",
            "schedule": "every 2m",
            "script": "caduceus-pulse.sh",
        }
    }
    _stub_cron_runtime(adapter, registry)
    try:
        snap = adapter._snapshot_wrapper_and_job(wrapper, "caduceus", registry)
    finally:
        _runtime.reset_dispatcher()

    assert isinstance(snap, adapter._Snapshot)
    assert snap.wrapper_bytes == wrapper.read_bytes()
    assert snap.wrapper_mode == 0o755
    assert snap.job_dict is not None
    assert snap.job_dict["id"] == "abc"




def test_snapshot_wrapper_and_job_no_matching_job(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """When no matching job exists, job_dict is None."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    # Registry has jobs but none named "caduceus".
    registry = {
        "xyz": {
            "id": "xyz",
            "name": "other-service",
            "schedule": "every 5m",
        }
    }
    _stub_cron_runtime(adapter, registry)
    try:
        snap = adapter._snapshot_wrapper_and_job(wrapper, "caduceus", registry)
    finally:
        _runtime.reset_dispatcher()

    assert snap.job_dict is None
    assert snap.wrapper_bytes == wrapper.read_bytes()




def test_snapshot_wrapper_and_job_no_wrapper_file(
    adapter, isolated_hermes_home: Path
) -> None:
    """When the wrapper file does not exist, bytes are empty and mode is 0."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    registry = {}

    _stub_cron_runtime(adapter, registry)
    try:
        snap = adapter._snapshot_wrapper_and_job(wrapper, "caduceus", registry)
    finally:
        _runtime.reset_dispatcher()

    assert snap.wrapper_bytes == b""
    assert snap.wrapper_mode == 0
    assert snap.job_dict is None

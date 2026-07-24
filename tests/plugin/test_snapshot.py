"""Snapshot dataclass tests."""

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


def test_snapshot_is_frozen_dataclass() -> None:
    """_Snapshot is a frozen dataclass with wrapper_bytes, wrapper_mode, job_dict."""
    from caduceus import _Snapshot

    snap = _Snapshot(wrapper_bytes=b"content", wrapper_mode=0o755, job_dict=None)
    assert snap.wrapper_bytes == b"content"
    assert snap.wrapper_mode == 0o755
    assert snap.job_dict is None

    # Frozen — cannot set attributes.
    with pytest.raises(AttributeError):
        snap.wrapper_bytes = b"other"

    # Frozen — cannot delete attributes.
    with pytest.raises(AttributeError):
        del snap.wrapper_bytes




def test_snapshot_accepts_job_dict() -> None:
    """_Snapshot accepts a non-None job_dict."""
    from caduceus import _Snapshot

    job = {"id": "abc", "name": "caduceus", "schedule": "every 2m"}
    snap = _Snapshot(wrapper_bytes=b"x", wrapper_mode=0o644, job_dict=job)
    assert snap.job_dict == job

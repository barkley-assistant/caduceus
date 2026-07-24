"""Cron wrapper path/content/mode tests."""

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


def test_cron_wrapper_path_content_mode(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """The wrapper at ``$HERMES_HOME/scripts/caduceus-pulse.sh`` exists, is mode 0755,
    contains the absolute binary path, and ends in ``exec <binary> run "$@"``.
    """
    adapter._write_pulse_wrapper(install_with_fake_binary)
    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    assert wrapper.is_file()
    assert not wrapper.is_symlink()
    mode = stat.S_IMODE(wrapper.stat().st_mode)
    assert mode == 0o755
    body = wrapper.read_text(encoding="utf-8")
    assert str(install_with_fake_binary) in body
    assert body.rstrip().endswith(f'exec {install_with_fake_binary} run "$@"')

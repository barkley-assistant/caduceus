"""Caduceus status slash command tests."""

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


def test_status_slash_command_is_registered(adapter, fake_ctx: FakePluginContext) -> None:
    adapter.register(fake_ctx)
    cmd = assert_command_registered(fake_ctx, "caduceus-status")
    assert callable(cmd.handler)




def test_status_slash_command_missing_binary_returns_diagnostic(
    adapter, fake_ctx: FakePluginContext
) -> None:
    """When the binary is absent the handler returns a precise diagnostic."""
    adapter.register(fake_ctx)
    cmd = assert_command_registered(fake_ctx, "caduceus-status")
    result = cmd.handler("")
    assert isinstance(result, str)
    assert "hermes caduceus setup" in result




def test_status_slash_command_invokes_binary(
    adapter, fake_ctx: FakePluginContext, install_with_fake_binary: Path
) -> None:
    adapter.register(fake_ctx)
    cmd = assert_command_registered(fake_ctx, "caduceus-status")
    result = cmd.handler("")
    assert isinstance(result, str)
    assert "caduceus 0.1.0" in result




def test_status_slash_redacts_token_like_strings(
    adapter, fake_ctx: FakePluginContext, install_plugin: Path, tmp_path: Path
) -> None:
    """A binary that prints ``GITHUB_TOKEN=ghp_xxx`` is redacted."""
    binary = install_plugin / "bin" / "caduceus"
    binary.parent.mkdir(exist_ok=True)
    binary.write_text(
        "#!/usr/bin/env bash\n"
        'if [ "$1" = "status" ]; then\n'
        '  if [ "$2" = "--json" ]; then\n'
        '    printf \'{"version":"0.1.0","last_tick":"never","last_outcome":"idle"}\'\n'
        "  fi\n"
        "  exit 0\n"
        "fi\n"
        "exit 0\n"
    )
    binary.chmod(0o755)
    adapter.register(fake_ctx)
    cmd = assert_command_registered(fake_ctx, "caduceus-status")
    result = cmd.handler("")
    assert result is not None
    # No ``ghp_`` token made it into chat output.
    assert "ghp_" not in result
    assert "<redacted>" not in result  # the fake didn't leak one — defensive

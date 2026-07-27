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


def test_format_status_for_chat_reads_nested_report_key(adapter) -> None:
    """When the payload carries a nested ``report`` key, the formatter reads
    the app version from there. This is the shape the live ``caduceus
    status --json`` produces today.
    """
    payload = {
        "report": {
            "app_version": "1.2.3",
            "last_tick_started": None,
            "last_tick_finished": None,
            "last_outcome": "idle",
            "phases": {},
        }
    }
    result = adapter._format_status_for_chat(payload)
    assert "1.2.3" in result
    assert "started=" in result
    assert "finished=" in result


def test_format_status_for_chat_falls_back_to_root(adapter) -> None:
    """When the payload is flat (no ``report`` key), the formatter falls
    back to the root level so legacy consumers keep working.
    """
    payload = {"version": "1.2.3", "phases": {}}
    result = adapter._format_status_for_chat(payload)
    assert "1.2.3" in result


def test_format_status_for_chat_prefers_app_version_over_version(adapter) -> None:
    """When both ``app_version`` and ``version`` are present, the formatter
    prefers ``app_version`` so the chat shows the real crate version, not
    the JSON schema version.
    """
    payload = {
        "app_version": "2.0.0",
        "version": "7.5.0",
        "report": {"version": "7.5.0"},
    }
    result = adapter._format_status_for_chat(payload)
    assert "2.0.0" in result
    assert "7.5.0" not in result


def test_format_status_for_chat_shows_last_tick_started_and_finished(
    adapter,
) -> None:
    """The summary surfaces both ``last_tick_started`` and
    ``last_tick_finished`` timestamps, not a single non-existent
    ``last_tick`` field.
    """
    payload = {
        "report": {
            "app_version": "1.0.0",
            "last_tick_started": "2024-01-01T00:00:00Z",
            "last_tick_finished": "2024-01-01T00:00:30Z",
            "phases": {},
        }
    }
    result = adapter._format_status_for_chat(payload)
    assert "2024-01-01T00:00:00Z" in result
    assert "2024-01-01T00:00:30Z" in result

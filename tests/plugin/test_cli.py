"""Caduceus CLI command registration tests."""

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


def _register_and_get_parser(adapter, fake_ctx):
    adapter.register(fake_ctx)
    return fake_ctx.cli_commands["caduceus"].parser




def test_cli_command_is_registered(adapter, fake_ctx: FakePluginContext) -> None:
    parser = _register_and_get_parser(adapter, fake_ctx)
    assert parser is not None
    # Help text references the canonical subcommands.
    help_text = parser.format_help()
    for sub in (
        "setup",
        "doctor",
        "status",
        "cron-install",
        "cron-remove",
        "run",
        "review",
        "queue",
        "worktree-gc",
        "migrate-state",
    ):
        assert sub in help_text, f"missing subcommand {sub} in help"




def test_cli_unknown_subcommand_is_rejected(adapter, fake_ctx: FakePluginContext) -> None:
    parser = _register_and_get_parser(adapter, fake_ctx)
    with pytest.raises(SystemExit):
        # argparse exits 2 on unknown subcommands.
        parser.parse_args(["nope"])


def test_cli_bare_invocation_prints_help_and_exits_zero(
    adapter, fake_ctx: FakePluginContext, capsys: pytest.CaptureFixture
) -> None:
    """Bare ``hermes caduceus`` prints help to stdout and returns 0 (issue #411).

    The parser here is the fixture's (``prog="caduceus"``), so the
    assertion is on the usage-line *shape*, not the exact ``prog``.
    """
    parser = _register_and_get_parser(adapter, fake_ctx)
    args = parser.parse_args([])  # bare invocation parses
    assert getattr(args, "caduceus_command", "missing") is None
    rc = args.func(args)  # handler renders help
    assert rc == 0
    captured = capsys.readouterr()
    assert captured.err == ""  # no argparse error on stderr
    out = captured.out
    assert out.startswith("usage: ")  # help on stdout
    assert "COMMAND" in out  # readable metavar, not the {...} choice blob
    for sub in (
        "setup",
        "doctor",
        "status",
        "queue",
        "run",
        "review",
        "worktree-gc",
        "migrate-state",
        "cron-install",
        "cron-remove",
    ):
        assert sub in out, f"missing subcommand {sub} in bare help"
    assert "Examples:" in out


def test_cli_explicit_help_still_works(
    adapter, fake_ctx: FakePluginContext, capsys: pytest.CaptureFixture
) -> None:
    """``hermes caduceus --help`` still exits 0 and prints the same page."""
    parser = _register_and_get_parser(adapter, fake_ctx)
    with pytest.raises(SystemExit) as exc:
        parser.parse_args(["--help"])
    assert exc.value.code == 0
    captured = capsys.readouterr()
    assert "usage: " in captured.out
    assert "Examples:" in captured.out

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
    for sub in ("setup", "doctor", "status", "cron-install", "cron-remove"):
        assert sub in help_text, f"missing subcommand {sub} in help"




def test_cli_unknown_subcommand_is_rejected(adapter, fake_ctx: FakePluginContext) -> None:
    parser = _register_and_get_parser(adapter, fake_ctx)
    with pytest.raises(SystemExit):
        # argparse exits 2 on unknown subcommands.
        parser.parse_args(["nope"])

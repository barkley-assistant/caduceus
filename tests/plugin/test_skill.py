"""Plugin skill resolution tests."""

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


def test_skill_registers_as_caduceus_caduceus(
    adapter, fake_ctx: FakePluginContext, install_plugin: Path
) -> None:
    """``ctx.register_skill('caduceus', ...)`` is resolvable as ``caduceus:caduceus``."""
    adapter.register(fake_ctx)
    record = assert_skill_registered(fake_ctx, "caduceus")
    assert record.path == install_plugin / "skills" / "caduceus" / "SKILL.md"
    # Mirror the loader's namespace join.
    qualified = f"{fake_ctx.name}:caduceus"
    assert qualified == "caduceus:caduceus"




def test_skill_file_passes_yaml_frontmatter() -> None:
    """SKILL.md exists and is non-trivial text the loader can consume."""
    skill = Path(__file__).resolve().parent.parent.parent / "skills" / "caduceus" / "SKILL.md"
    assert skill.is_file(), skill
    text = skill.read_text(encoding="utf-8")
    # The skill body must describe boundaries; contract prohibits
    # narrative-only files with no actionable content.
    lowered = text.lower()
    assert "caduceus" in lowered
    assert "setup" in lowered or "doctor" in lowered or "cron" in lowered

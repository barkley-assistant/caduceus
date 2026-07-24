"""Plugin install surface tests."""

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


def test_install_copies_plugin_tree_into_hermes_home(
    install_plugin: Path, isolated_hermes_home: Path
) -> None:
    """``hermes plugins install`` clones the repository root."""
    assert install_plugin.is_dir(), install_plugin
    assert (install_plugin / "plugin.yaml").is_file()
    assert (install_plugin / "__init__.py").is_file()
    assert (install_plugin / "skills" / "caduceus" / "SKILL.md").is_file()
    assert (install_plugin / "plugin-assets" / "worker-bridge.py").is_file()
    assert (install_plugin / "plugin-assets" / "caduceus-pulse.sh").is_file()
    # Rust workspace files ride along with the plugin source.
    assert (install_plugin / "Cargo.toml").is_file()
    assert (install_plugin / "Cargo.lock").is_file()
    assert (install_plugin / "src" / "lib.rs").is_file()
    assert (install_plugin / "src" / "main.rs").is_file()
    # No temp / build / planning directory leaks.
    assert not (install_plugin / "target").exists()
    assert not (install_plugin / "planning").exists()
    assert not (install_plugin / "tests").exists()




def test_install_root_is_canonical(plugin_root: Path) -> None:
    """The repository *root* itself must already be the installable surface."""
    assert (plugin_root / "plugin.yaml").is_file()
    assert (plugin_root / "__init__.py").is_file()
    assert (plugin_root / "Cargo.toml").is_file()

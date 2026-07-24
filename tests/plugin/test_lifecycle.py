"""Plugin source update/removal lifecycle tests."""

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


def test_source_update_then_rebuild_preserves_user_bridge(
    adapter, isolated_hermes_home: Path, install_plugin: Path, install_with_fake_binary: Path
) -> None:
    """A simulated ``hermes plugins update caduceus`` followed by
    ``hermes caduceus setup`` preserves the user-owned bridge."""
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    sentinel = "# user-edited-content-post-update\n"
    bridge.write_text(sentinel + "print('hi')\n", encoding="utf-8")

    # Update the plugin tree contents (simulated by touching files).
    template = install_plugin / "plugin-assets" / "worker-bridge.py"
    updated = template.read_text(encoding="utf-8") + "\n# upstream-revision-N\n"
    template.write_text(updated, encoding="utf-8")
    try:
        adapter._seed_user_bridge()
        # User copy untouched.
        assert sentinel in bridge.read_text(encoding="utf-8")
        # New candidate written next to the user copy.
        assert (bridge.parent / "worker-bridge.py.new").is_file()
    finally:
        template.write_text(template.read_text(encoding="utf-8").removesuffix("\n# upstream-revision-N\n"), encoding="utf-8")




def test_plugin_removal_preserves_state(isolated_hermes_home: Path) -> None:
    """Removing the plugin directory leaves the user-owned bridge and state alone."""
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("# user bridge\n")
    state = isolated_hermes_home / "caduceus-state"
    state.mkdir()
    plugins_root = isolated_hermes_home / "plugins"
    (plugins_root / "caduceus").mkdir(parents=True)
    (plugins_root / "caduceus" / "__init__.py").write_text("# plugin\n")
    # Simulate ``hermes plugins remove caduceus`` by deleting the
    # plugin directory under plugins/, mirroring Hermes's behaviour.
    shutil.rmtree(plugins_root / "caduceus")
    assert not (plugins_root / "caduceus").exists()
    assert bridge.is_file()  # user bridge preserved
    assert state.is_dir()  # state preserved




def test_legacy_plugin_directory_is_absent(plugin_root: Path) -> None:
    """The historical ``plugin/`` layout must not exist."""
    legacy = plugin_root / "plugin"
    assert not legacy.exists(), f"legacy directory still present at {legacy}"




def test_subprocess_call_redacts_secrets(adapter) -> None:
    """``_redact`` masks token-shaped values from arbitrary text."""
    text = (
        "Calling GitHub with GITHUB_TOKEN=ghp_abcd1234efgh5678 and "
        "GH_TOKEN=ghp_zzzz and CADUCEUS_GITHUB_TOKEN=ghp_yyyy and "
        "GITHUB_TOKEN=\"ghp_quoted\"\n"
    )
    out = adapter._redact(text)
    assert "ghp_abcd1234efgh5678" not in out
    assert "ghp_zzzz" not in out
    assert "ghp_quoted" not in out
    # The variable name remains so operators can still see WHICH env
    # var was leaked.
    assert "GITHUB_TOKEN=" in out or "GITHUB_TOKEN =" in out

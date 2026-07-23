"""Plugin manifest field allowlist tests."""

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


_ALLOWED_MANIFEST_FIELDS = {
    "manifest_version",
    "name",
    "version",
    "description",
    "author",
    "kind",
    "requires_env",
    "provides_tools",
    "provides_hooks",
}


def _read_plugin_yaml(installed: Path) -> Dict[str, Any]:
    import yaml

    text = (installed / "plugin.yaml").read_text(encoding="utf-8")
    return yaml.safe_load(text) or {}




def _manifest_field_check(plugin_yaml_text: str) -> tuple:
    """Return ``(allowed_fields, rejected_fields)`` against the contract list."""
    import yaml

    data = yaml.safe_load(plugin_yaml_text) or {}
    allowed = _ALLOWED_MANIFEST_FIELDS
    rejected = sorted(k for k in data.keys() if k not in allowed)
    allowed_actual = sorted(k for k in data.keys() if k in allowed)
    return (allowed_actual, rejected)




def test_manifest_uses_only_supported_fields(install_plugin: Path) -> None:
    """``plugin.yaml`` declares only fields the v0.18.2 loader accepts."""
    yaml_text = (install_plugin / "plugin.yaml").read_text(encoding="utf-8")
    allowed, rejected = _manifest_field_check(yaml_text)
    assert not rejected, f"unknown manifest fields: {rejected}"
    # Spot-check: kind must be ``standalone`` for Caduceus.
    import yaml

    data = yaml.safe_load(yaml_text) or {}
    assert data.get("kind") == "standalone"
    # provides_tools/hooks must be explicit empty lists — a missing
    # field would silently be ``None`` in some upstream loaders.
    assert data.get("provides_tools") == []
    assert data.get("provides_hooks") == []




def test_negative_manifest_with_legacy_fields_is_rejected(
    tmp_path: Path, plugin_root: Path
) -> None:
    """A negative fixture containing legacy fields must fail the check."""
    bad_yaml = plugin_root / "tests" / "fixtures" / "negative_plugin.yaml"
    assert bad_yaml.is_file(), "negative fixture missing"
    allowed, rejected = _manifest_field_check(bad_yaml.read_text(encoding="utf-8"))
    assert rejected, "negative fixture did not actually declare a legacy field"

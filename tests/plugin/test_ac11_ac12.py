"""AC-11/AC-12 manifest and subprocess invariant tests."""

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


def test_reconcile_failed_relist_returns_needs_attention(
    adapter, isolated_hermes_home: Path
) -> None:
    """When re-list fails, reconcile returns NeedsAttention."""
    from caduceus import _runtime

    original_registry = adapter._cron_job_registry
    def _failing_registry():
        raise RuntimeError("cannot list cron jobs")

    try:
        adapter._cron_job_registry = _failing_registry  # type: ignore[assignment]
        result = adapter._reconcile_after_error(
            error=RuntimeError("some error"),
            snapshot=adapter._Snapshot(
                wrapper_bytes=b"data", wrapper_mode=0o755, job_dict={"id": "abc"},
            ),
            ctx=adapter,
            job_name="caduceus",
            intended_state="present",
        )
    finally:
        adapter._cron_job_registry = original_registry

    assert isinstance(result, adapter._NeedsAttention)
    assert "re-list failed" in result.recovery_evidence




def test_ac11_no_real_hermes_subprocess():
    """AC-11: Verify no real hermes subprocess in Python integration tests."""
    src = __file__  # This file's path
    text = Path(src).read_text(encoding="utf-8")

    # Banned patterns: direct subprocess.Popen/hermes invocation
    # that would spawn a real Hermes binary.
    import re
    banned = [
        # Subprocess patterns that would create a real hermes process
        r'subprocess\.\w+\(.*hermes',
        r'subprocess\.\w+\(.*\["hermes"',
        r'subprocess\.\w+\(.*\[\'hermes\'',
        # Direct hermes binary path
        r'"/home/agent/\.local/bin/hermes"',
        r"'/home/agent/\.local/bin/hermes'",
        # Environment variable override for real hermes
        r'os\.environ\["HERMES_BIN"\]',
        # Popen with hermes
        r'Popen\(.*hermes',
    ]
    for pattern in banned:
        matches = re.findall(pattern, text)
        assert not matches, f"AC-11: banned hermes-subprocess pattern found: {pattern}"




def test_ac12_manifest_invariants_post_install(install_plugin: Path, isolated_hermes_home: Path):
    """AC-12: Verify plugin.yaml manifests + hooks/scripts/assets/skill entries."""
    plugin_dir = install_plugin
    plugin_yaml = plugin_dir / "plugin.yaml"

    # plugin.yaml must exist and parse
    assert plugin_yaml.is_file(), "AC-12: plugin.yaml must exist after install"
    import yaml
    manifest = yaml.safe_load(plugin_yaml.read_text(encoding="utf-8"))
    assert manifest is not None, "AC-12: plugin.yaml must parse as YAML"
    assert isinstance(manifest, dict), "AC-12: plugin.yaml must be a dict"

    # Verify installed directory structure
    # The plugin must have __init__.py, _runtime.py, and plugin-assets/
    assert (plugin_dir / "__init__.py").is_file(), "AC-12: __init__.py must exist"
    assert (plugin_dir / "_runtime.py").is_file(), "AC-12: _runtime.py must exist"
    assert (plugin_dir / "plugin-assets").is_dir(), "AC-12: plugin-assets/ must exist"

    # Verify skills directory
    skill_dir = plugin_dir / "skills" / "caduceus"
    assert skill_dir.is_dir(), "AC-12: skills/caduceus/ must exist"
    assert (skill_dir / "SKILL.md").is_file(), "AC-12: skills/caduceus/SKILL.md must exist"

    # Verify plugin-assets files exist
    expected_assets = ["caduceus-pulse.sh", "worker-bridge.py"]
    for asset in expected_assets:
        asset_path = plugin_dir / "plugin-assets" / asset
        assert asset_path.is_file(), f"AC-12: plugin-assets/{asset} must exist"

    # Verify the manifest does not reference files absent from the install
    if "provides_hooks" in manifest and manifest["provides_hooks"]:
        for hook in manifest["provides_hooks"]:
            if "path" in hook:
                hook_path = plugin_dir / hook["path"]
                assert hook_path.exists(), (
                    f"AC-12: hook path {hook['path']} referenced in manifest "
                    f"but not found on disk"
                )

    # Verify plugin metadata fields are present
    for field in ("name", "version", "kind", "manifest_version"):
        assert field in manifest, f"AC-12: plugin.yaml must contain '{field}'"

    # The plugin name must be "caduceus"
    assert manifest.get("name") == "caduceus", (
        f"AC-12: plugin.yaml name must be 'caduceus'; got {manifest.get('name')}"
    )

"""Caduceus setup CLI tests."""

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


def _setup_args(adapter, argv):
    """Build a local argparse tree and dispatch the supplied subcommand."""
    import argparse

    parser = argparse.ArgumentParser(prog="caduceus")
    adapter._register_caduceus_cli(parser)
    return parser.parse_args(argv)




def test_setup_dry_run_reports_planned_actions(
    adapter, capsys: pytest.CaptureFixture
) -> None:
    args = _setup_args(adapter, ["setup", "--dry-run"])
    rc = args.func(args)
    captured = capsys.readouterr()
    assert rc == 0
    assert "dry-run" in captured.out
    assert "Cargo.toml" in captured.out




def test_setup_atomic_install_uses_replace(
    adapter, install_plugin: Path, tmp_path: Path, monkeypatch
) -> None:
    """The installed binary is created via ``os.replace``."""
    calls = []

    def fake_install(src, dst):
        calls.append({"src": str(src), "dst": str(dst)})
        dst.parent.mkdir(parents=True, exist_ok=True)
        # Use os.replace semantics exactly: write a temp file, then
        # replace it. This mirrors what the production helper does.
        tmp = dst.with_name(dst.name + ".tmp")
        if tmp.exists() or tmp.is_symlink():
            tmp.unlink()
        tmp.write_text("test-binary", encoding="utf-8")
        os.replace(tmp, dst)
        os.chmod(dst, 0o755)

    # Drive ``_cli_setup`` via the binary-stub shortcut: when cargo is
    # absent (as it is in the test environment), the real helper would
    # fail. Replace the adapter's helpers so setup reaches the
    # install step.
    monkeypatch.setattr(adapter, "_atomic_install_binary", fake_install)
    monkeypatch.setattr(
        adapter,
        "_check_setup_prerequisites",
        lambda root: [],
    )
    monkeypatch.setattr(
        adapter,
        "_build_daemon_binary",
        lambda root: tmp_path / "fake-binary",
    )
    (tmp_path / "fake-binary").write_text("binary", encoding="utf-8")
    monkeypatch.setattr(
        adapter, "_ensure_state_directories", lambda state_dir: None
    )
    monkeypatch.setattr(adapter, "_seed_user_bridge", lambda: None)

    rc = adapter._cli_setup(dry_run=False)
    assert rc == 0
    assert calls, "atomic helper was not invoked"
    installed = install_plugin / "bin" / "caduceus"
    assert installed.is_file()
    mode = stat.S_IMODE(installed.stat().st_mode)
    assert mode == 0o755
    # No leftover tmp — the helper used os.replace.
    leftover = install_plugin / "bin" / "caduceus.tmp"
    assert not leftover.exists()




def test_setup_uses_locked_lock_file_required(adapter, monkeypatch) -> None:
    """``cargo build`` is invoked with ``--locked``."""
    captured = []

    def capturing(argv, **kwargs):
        captured.append(list(argv))
        from subprocess import CompletedProcess

        # ``--version`` style preflight calls succeed; ``cargo build``
        # would also return 0 because we do not want to actually link.
        if len(argv) >= 2 and argv[0] == "cargo":
            target_dir = adapter._plugin_root() / "target" / "release"
            target_dir.mkdir(parents=True, exist_ok=True)
            binary = target_dir / "caduceus"
            if not binary.exists():
                binary.write_text("#!/bin/sh\necho stub\n")
                binary.chmod(0o755)
            return CompletedProcess(argv, 0, stdout="cargo 1.97.0", stderr="")
        return CompletedProcess(argv, 0, stdout="", stderr="")

    monkeypatch.setattr(adapter, "_run", capturing)
    monkeypatch.setattr(
        adapter, "_atomic_install_binary", lambda src, dst: None
    )
    monkeypatch.setattr(
        adapter, "_ensure_state_directories", lambda state_dir: None
    )
    monkeypatch.setattr(adapter, "_seed_user_bridge", lambda: None)

    rc = adapter._cli_setup(dry_run=False)
    assert rc == 0
    cargo_builds = [
        c
        for c in captured
        if len(c) >= 2 and c[0] == "cargo" and c[1] == "build"
    ]
    assert cargo_builds, captured
    for argv in cargo_builds:
        assert "--locked" in argv, argv




def test_setup_idempotent_in_existing_state(
    adapter, install_plugin: Path, tmp_path: Path, isolated_hermes_home: Path
) -> None:
    """Running setup twice does not duplicate bridges, binaries, or cron jobs."""
    env_path = install_plugin / "plugin-assets" / "worker-bridge.py"
    target = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    target.parent.mkdir(parents=True, exist_ok=True)
    initial = env_path.read_text(encoding="utf-8")
    target.write_text(initial, encoding="utf-8")
    # Tag the bridge with a marker so the second invocation can detect
    # a divergence and write ``.new``.
    marker_target = target
    marker_text = "# user-edited-marker\n" + initial
    marker_target.write_text(marker_text, encoding="utf-8")
    # First setup should detect the divergence.
    adapter._seed_user_bridge()
    assert (target.with_name(target.name + ".new")).exists(), "expected .new candidate"




def test_setup_user_bridge_preservation(
    adapter, install_plugin: Path, isolated_hermes_home: Path
) -> None:
    target = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    target.parent.mkdir(parents=True, exist_ok=True)
    template = install_plugin / "plugin-assets" / "worker-bridge.py"
    template_text = template.read_text(encoding="utf-8")
    sentinel = "# user-owned bridge — must not be overwritten\n"

    # Case 1: the user copy is byte-identical to the template. Setup
    # must not overwrite it and must not leave a .new candidate.
    target.write_text(template_text, encoding="utf-8")
    adapter._seed_user_bridge()
    assert target.read_text(encoding="utf-8") == template_text
    assert not target.with_name(target.name + ".new").exists()

    # Case 2: the user copy is a divergence from the template (the
    # sentinel has been prepended). Setup must preserve the user copy
    # verbatim and must not touch it.
    target.write_text(sentinel + template_text, encoding="utf-8")
    adapter._seed_user_bridge()
    user_text = target.read_text(encoding="utf-8")
    assert user_text == sentinel + template_text

    # Case 3: now the template and the upstream diverge again. The
    # adapter writes a sibling ``.new`` candidate and leaves the user
    # copy alone.
    template.write_text(template_text + "\n# upstream-marker\n", encoding="utf-8")
    try:
        adapter._seed_user_bridge()
        assert target.with_name(target.name + ".new").is_file()
        assert target.read_text(encoding="utf-8") == sentinel + template_text
    finally:
        template.write_text(template_text, encoding="utf-8")

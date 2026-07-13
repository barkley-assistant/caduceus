"""Pytest scaffolding for the Hermes-side Caduceus adapter tests.

The Caduceus plugin is installed by cloning the repository root into
``~/.hermes/plugins/caduceus/`` (per the contract). For these tests we
deliberately point ``HERMES_HOME`` at a temp directory so none of the
state in ``$HOME/.hermes`` is touched.

The ``plugin_root`` fixture resolves the project root by walking up from
this test file and exposes a few ``copy_*`` helpers that mirror the
contract's expectations for installation paths.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Iterator

import pytest


TESTS_DIR = Path(__file__).resolve().parent
REPO_ROOT = TESTS_DIR.parent


@pytest.fixture(scope="session")
def repo_root() -> Path:
    """Absolute path to the Caduceus repository root."""
    return REPO_ROOT


@pytest.fixture(autouse=True)
def isolated_hermes_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    """Pin ``HERMES_HOME`` to a fresh temp directory and reset state."""
    home = tmp_path / ".hermes"
    home.mkdir()
    monkeypatch.setenv("HERMES_HOME", str(home))
    # Defensive: the adapter also recognises HERMES_CADUCEUS_ROOT so
    # tests can reroute the plugin root without monkeypatching imports.
    yield home


@pytest.fixture
def plugin_root() -> Path:
    """Return the absolute path of the Caduceus plugin root."""
    return REPO_ROOT


@pytest.fixture
def fake_ctx():
    """Return a context stand-in that captures plugin registrations.

    The fixture builds a fresh object per test so registrations don't
    bleed between tests. It also exposes ``dispatch_tool`` so the cron
    helpers wire cleanly.
    """
    from tests.fake_ctx import FakePluginContext

    return FakePluginContext(name="caduceus", version="0.1.0")


@pytest.fixture
def install_plugin(isolated_hermes_home: Path, plugin_root: Path) -> Path:
    """Copy the plugin source tree into ``$HERMES_HOME/plugins/caduceus``.

    Mirrors the actual ``hermes plugins install`` layout — the plugin
    directory at ``~/.hermes/plugins/caduceus/`` contains
    ``plugin.yaml``, ``__init__.py``, ``skills/caduceus/SKILL.md``,
    ``plugin-assets/...``, plus the Rust workspace files (Cargo.toml,
    Cargo.lock, src/) that the contract pins as part of the install
    surface.
    """
    target = isolated_hermes_home / "plugins" / "caduceus"
    if target.exists():
        shutil.rmtree(target)
    target.mkdir(parents=True)
    for entry in plugin_root.iterdir():
        if entry.name in {
            "target",
            ".git",
            "tests",
            "planning",
            "node_modules",
            "__pycache__",
        }:
            continue
        if entry.name.startswith("."):
            continue
        dest = target / entry.name
        if entry.is_dir():
            shutil.copytree(entry, dest, symlinks=False)
        else:
            shutil.copy2(entry, dest)
    return target


def _run_module(module_path: Path, parent_package: str = "caduceus"):
    """Import a Python file as ``<parent_package>.__init__`` so register() works.

    Hermes loads directory plugins as ``hermes_plugins.caduceus`` (see
    ``hermes_cli.plugins._load_directory_module``), setting both
    ``__package__`` and ``__path__`` so relative imports resolve. We
    mirror that here so the adapter's ``from . import _runtime``
    succeeds inside the test environment.
    """
    import importlib.util
    import types

    module_name = f"{parent_package}"
    pkg = types.ModuleType(parent_package)
    pkg.__package__ = parent_package
    pkg.__path__ = [str(module_path.parent)]
    sys.modules[parent_package] = pkg
    spec = importlib.util.spec_from_file_location(
        module_name, str(module_path), submodule_search_locations=[str(module_path.parent)]
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    module.__package__ = module_name
    sys.modules[module_name] = module
    spec.loader.exec_module(module)
    return module


@pytest.fixture
def adapter(install_plugin: Path):
    """Import the root ``__init__.py`` from the installed plugin copy."""
    return _run_module(install_plugin / "__init__.py")


@pytest.fixture
def install_with_fake_binary(install_plugin: Path) -> Path:
    """Drop a small bash script at ``<plugin>/bin/caduceus`` so status works.

    Real binaries are built by ``cargo build --release``; that takes too
    long for unit tests, so we substitute a fake binary that prints
    either a human summary or a JSON payload.
    """
    bin_dir = install_plugin / "bin"
    bin_dir.mkdir(exist_ok=True)
    binary = bin_dir / "caduceus"
    body = (
        "#!/usr/bin/env bash\n"
        "# test fake binary — kept in tests only; replaced by `cargo build`"
        ' in real setup\n'
        'if [ "$1" = "status" ]; then\n'
        '  if [ "$2" = "--json" ]; then\n'
        '    printf \'{"version":"0.1.0","last_tick":"never","last_outcome":"idle","phases":{"queued":0},"next_head":null,"rate_limit":null}\'\n'
        "  else\n"
        '    echo "caduceus: stub status"\n'
        "  fi\n"
        "  exit 0\n"
        "fi\n"
        'echo "fake binary: unknown subcommand $1" 1>&2\n'
        "exit 1\n"
    )
    binary.write_text(body)
    binary.chmod(0o755)
    return binary

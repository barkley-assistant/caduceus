"""Plugin registration surface tests."""

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


def test_register_uses_documentated_ctx_surface(
    adapter, fake_ctx: FakePluginContext
) -> None:
    """``register(ctx)`` only invokes the documented ``ctx`` methods."""
    before = {
        "skills": set(fake_ctx.skills),
        "commands": set(fake_ctx.commands),
        "cli_commands": set(fake_ctx.cli_commands),
    }
    adapter.register(fake_ctx)
    # New surfaces the adapter must touch.
    assert {"caduceus"}.issubset(set(fake_ctx.skills))
    assert {"caduceus-status"}.issubset(set(fake_ctx.commands))
    assert {"caduceus"}.issubset(set(fake_ctx.cli_commands))




def test_register_does_not_mutate_filesystem_outside_surface(
    adapter, fake_ctx: FakePluginContext, isolated_hermes_home: Path, tmp_path: Path
) -> None:
    """Registration must not create cron jobs, build artefacts, or config."""
    state_dir = isolated_hermes_home / "caduceus-state"
    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    assert not state_dir.exists()
    assert not wrapper.exists()
    assert not bridge.exists()
    # The adapter must not even attempt a network call. We patch
    # socket.socket globally for the duration of ``register`` to make
    # the assertion structural rather than time-bound.
    import socket as _socket

    original_socket = _socket.socket
    called = []

    def _deny(*args, **kwargs):
        called.append((args, kwargs))
        raise AssertionError("registration must not open a socket")

    _socket.socket = _deny
    try:
        adapter.register(fake_ctx)
    finally:
        _socket.socket = original_socket
    assert called == []
    assert not state_dir.exists()
    assert not wrapper.exists()
    assert not bridge.exists()




def test_register_is_idempotent(adapter, fake_ctx: FakePluginContext) -> None:
    """Calling ``register`` twice registers the same surfaces, no errors."""
    adapter.register(fake_ctx)
    adapter.register(fake_ctx)
    assert "caduceus" in fake_ctx.skills
    assert "caduceus-status" in fake_ctx.commands
    assert "caduceus" in fake_ctx.cli_commands

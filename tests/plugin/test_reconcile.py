"""Reconcile-after-error tests."""

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

from tests.plugin._helpers import _stub_wrapper_file, _stub_cron_runtime


def test_reconcile_intended_state_already_exists(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """If intended state already exists in the re-list, reconcile is a no-op."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    # Registry already has a caduceus job (intended state).
    registry = {
        "abc": {
            "id": "abc",
            "name": "caduceus",
            "schedule": "every 2m",
        }
    }
    _stub_cron_runtime(adapter, registry)
    try:
        result = adapter._reconcile_after_error(
            error=RuntimeError("something went wrong"),
            snapshot=adapter._Snapshot(
                wrapper_bytes=b"old", wrapper_mode=0o755, job_dict=None
            ),
            ctx=adapter,
            job_name="caduceus",
            intended_state="present",
        )
    finally:
        _runtime.reset_dispatcher()

    # Success — intended state already present.
    assert result is None or result == "ok"




def test_reconcile_nothing_changed_from_snapshot(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """If re-list shows same state as snapshot, reconcile is a no-op."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)
    wrapper_bytes = wrapper.read_bytes()
    wrapper_mode = 0o755

    # Registry has no caduceus job — matches snapshot (job_dict=None).
    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        result = adapter._reconcile_after_error(
            error=RuntimeError("something went wrong"),
            snapshot=adapter._Snapshot(
                wrapper_bytes=wrapper_bytes, wrapper_mode=wrapper_mode, job_dict=None
            ),
            ctx=adapter,
            job_name="caduceus",
            intended_state="absent",
        )
    finally:
        _runtime.reset_dispatcher()

    assert result is None or result == "ok"




def test_reconcile_restores_wrapper_and_job(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """Reconcile restores wrapper bytes/mode and re-creates the job from snapshot."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)
    original_bytes = wrapper.read_bytes()

    job_dict = {
        "id": "abc",
        "name": "caduceus",
        "schedule": "every 2m",
        "script": "caduceus-pulse.sh",
        "no_agent": True,
    }

    # After the error, registry has been cleared (simulating a partially
    # failed remove that left no jobs and no wrapper).
    registry = {}
    _stub_cron_runtime(adapter, registry)
    # Remove the wrapper too.
    if wrapper.exists():
        wrapper.unlink()

    try:
        result = adapter._reconcile_after_error(
            error=RuntimeError("remove failed"),
            snapshot=adapter._Snapshot(
                wrapper_bytes=original_bytes,
                wrapper_mode=0o755,
                job_dict=job_dict,
            ),
            ctx=adapter,
            job_name="caduceus",
            intended_state="present",
        )
    finally:
        _runtime.reset_dispatcher()

    # Should have restored.
    assert result is None or result == "ok"
    assert wrapper.is_file()
    assert wrapper.read_bytes() == original_bytes
    mode = stat.S_IMODE(wrapper.stat().st_mode)
    assert mode == 0o755




def test_reconcile_impossible_rollback_returns_needs_attention(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """When reconciliation cannot restore state, _NeedsAttention is returned."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    job_dict = {"id": "abc", "name": "caduceus", "schedule": "every 2m"}

    _stub_cron_runtime(adapter, {})
    try:
        # Remove wrapper so there's nothing to restore from.
        if wrapper.exists():
            wrapper.unlink()
        result = adapter._reconcile_after_error(
            error=RuntimeError("remove failed"),
            snapshot=adapter._Snapshot(
                wrapper_bytes=b"", wrapper_mode=0, job_dict=job_dict,
            ),
            ctx=adapter,
            job_name="caduceus",
            intended_state="present",
        )
    finally:
        _runtime.reset_dispatcher()

    # Snapshot has empty wrapper but had a job — cannot restore wrapper.
    assert isinstance(result, adapter._NeedsAttention)
    assert isinstance(result.recovery_evidence, str)
    assert len(result.recovery_evidence) > 0




def test_reconcile_absent_intended_state_already_absent(
    adapter, isolated_hermes_home: Path
) -> None:
    """Reconcile with intended_state=absent is a no-op when no job exists."""
    from caduceus import _runtime

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        result = adapter._reconcile_after_error(
            error=RuntimeError("remove failed"),
            snapshot=adapter._Snapshot(
                wrapper_bytes=b"", wrapper_mode=0, job_dict=None,
            ),
            ctx=adapter,
            job_name="caduceus",
            intended_state="absent",
        )
    finally:
        _runtime.reset_dispatcher()

    assert result is None  # No-op success

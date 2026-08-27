"""Individual doctor check function tests."""

from __future__ import annotations

import fcntl
import os
import subprocess
import threading
import time
from pathlib import Path
from typing import Any, Dict

import pytest

from tests.plugin._helpers import _stub_cron_runtime


def test_doctor_check_binary_present(
    adapter, install_with_fake_binary: Path
) -> None:
    """_doctor_check_binary returns ok when binary exists."""
    finding = adapter._doctor_check_binary()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"
    assert str(install_with_fake_binary) in finding.detail



def test_doctor_check_binary_missing(adapter) -> None:
    """_doctor_check_binary returns fail when binary is missing."""
    finding = adapter._doctor_check_binary()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "fail"
    assert "not found" in finding.detail.lower() or "missing" in finding.detail.lower()
    assert "run setup to build it" in finding.detail
    assert "hermes caduceus setup" in finding.next_action



def test_doctor_check_bridge_harness_executable(
    adapter, isolated_hermes_home: Path
) -> None:
    """_doctor_check_bridge_harness returns ok when bridge is executable."""
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)
    finding = adapter._doctor_check_bridge_harness()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"
    assert str(bridge) in finding.detail
    assert "worker bridge" in finding.detail.lower()



def test_doctor_check_bridge_harness_not_executable(
    adapter, isolated_hermes_home: Path
) -> None:
    """_doctor_check_bridge_harness returns fail when bridge lacks execute bit."""
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o644)  # Not executable
    finding = adapter._doctor_check_bridge_harness()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "fail"
    assert "worker bridge" in finding.detail.lower()
    assert "chmod" in finding.next_action.lower() or "+x" in finding.next_action.lower()



def test_doctor_check_bridge_harness_not_yet_seeded(
    adapter, isolated_hermes_home: Path
) -> None:
    """A missing bridge is OK but framed as an external prerequisite."""
    finding = adapter._doctor_check_bridge_harness()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"
    assert "worker bridge not yet seeded" in finding.detail.lower()
    assert "external prerequisite" in finding.detail.lower()



def test_doctor_check_provider_secret_present(
    adapter, install_plugin: Path, monkeypatch
) -> None:
    """_doctor_check_provider_secret returns ok when secret name is configured."""
    monkeypatch.setenv("GITHUB_TOKEN", "ghp_test-secret-name")
    finding = adapter._doctor_check_provider_secret()
    assert finding.category == "config-incomplete"
    assert finding.status == "ok"
    assert "provider secret name GITHUB_TOKEN is configured" in finding.detail
    assert "no value read" in finding.detail



def test_doctor_check_provider_secret_missing(adapter) -> None:
    """_doctor_check_provider_secret returns fail when no secret name is set."""
    for var in ("CADUCEUS_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"):
        os.environ.pop(var, None)
    finding = adapter._doctor_check_provider_secret()
    assert finding.category == "config-incomplete"
    assert finding.status == "fail"
    assert "no provider secret name configured" in finding.detail.lower()
    assert finding.next_action.startswith("set one of")



def test_doctor_check_cron_capability_ok_with_jobs(
    adapter, install_with_fake_binary: Path
) -> None:
    """_doctor_check_cron_capability returns ok with caduceus job count."""
    from caduceus import _runtime

    registry = {
        "abc": {"id": "abc", "name": "caduceus", "schedule": "every 2m"},
    }
    _stub_cron_runtime(adapter, registry)
    try:
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    finally:
        _runtime.reset_dispatcher()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"
    assert "1 Caduceus cron job registered" in finding.detail
    assert "external prerequisite, exercised" in finding.detail



def test_doctor_check_cron_capability_no_caduceus_job(
    adapter, install_with_fake_binary: Path
) -> None:
    """A reachable cron subsystem with no caduceus job is a prerequisite."""
    from caduceus import _runtime

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    finally:
        _runtime.reset_dispatcher()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"
    assert "no Caduceus cron job registered yet" in finding.detail
    assert "external prerequisite, not exercised" in finding.detail
    assert "hermes caduceus cron-install" in finding.next_action



def test_doctor_check_cron_capability_hermes_not_on_path(
    adapter, install_with_fake_binary: Path
) -> None:
    """Missing hermes CLI points at PATH/install, not the gateway state."""
    from caduceus import _runtime

    _runtime.reset_dispatcher()
    _runtime._HERMES_PATH = None
    finding = adapter._doctor_check_cron_capability(ctx=adapter)
    assert finding.status == "fail"
    assert "hermes cli not on path" in finding.detail.lower()
    assert finding.next_action.startswith("install Hermes Agent")
    assert "hermes" in finding.next_action



def test_doctor_check_gateway_renamed_to_hermes_home(
    adapter, install_with_fake_binary: Path
) -> None:
    """_doctor_check_hermes_home returns a _DoctorFinding with the new label surface."""
    finding = adapter._doctor_check_hermes_home()
    assert isinstance(finding, tuple)
    assert finding.category == "gateway-inactive"
    assert finding.status in ("ok", "fail")



def test_doctor_check_worktree_lock_no_locks(
    adapter, isolated_hermes_home: Path
) -> None:
    """No .worktrees/.lock files means the check is clean."""
    finding = adapter._doctor_check_worktree_lock(ctx=None)
    assert finding.category == "daemon-defect"
    assert finding.status == "ok"
    assert "no stale" in finding.detail.lower() or "not present" in finding.detail.lower()



def test_doctor_check_worktree_lock_stale_lock(
    adapter, isolated_hermes_home: Path
) -> None:
    """An empty, unheld .worktrees/.lock is reported as a stale daemon defect."""
    lock = (
        isolated_hermes_home
        / "projects"
        / "octocat"
        / "Hello-World"
        / ".worktrees"
        / ".lock"
    )
    lock.parent.mkdir(parents=True, exist_ok=True)
    lock.write_text("")

    finding = adapter._doctor_check_worktree_lock(ctx=None)
    assert finding.category == "daemon-defect"
    assert finding.status == "fail"
    assert str(lock) in finding.detail
    assert "stale" in finding.detail.lower()



def test_doctor_check_worktree_lock_held_lock(
    adapter, isolated_hermes_home: Path
) -> None:
    """A .worktrees/.lock currently held by the daemon is not stale."""
    lock = (
        isolated_hermes_home
        / "projects"
        / "owner"
        / "repo"
        / ".worktrees"
        / ".lock"
    )
    lock.parent.mkdir(parents=True, exist_ok=True)
    lock.write_text("")

    acquired = threading.Event()
    release = threading.Event()

    def _hold_flock() -> None:
        fd = os.open(str(lock), os.O_RDWR)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX)
            acquired.set()
            release.wait(timeout=5.0)
        finally:
            fcntl.flock(fd, fcntl.LOCK_UN)
            os.close(fd)

    thread = threading.Thread(target=_hold_flock, daemon=True)
    thread.start()
    acquired.wait(timeout=5.0)

    try:
        finding = adapter._doctor_check_worktree_lock(ctx=None)
    finally:
        release.set()
        thread.join(timeout=5.0)

    assert finding.category == "daemon-defect"
    assert finding.status == "ok"
    assert "held" in finding.detail.lower()


# ---------------------------------------------------------------------------
# _doctor_check_oci_identity (issue #244)
# ---------------------------------------------------------------------------


class _FakeProc:
    """Minimal subprocess.run result double."""

    def __init__(self, returncode: int, stdout: bytes, stderr: bytes = b"") -> None:
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def _patch_engine_probe(
    adapter, monkeypatch: pytest.MonkeyPatch, docker, podman
) -> None:
    """Patch ``subprocess.run`` so the named engine probe returns the
    canned result and the other engine is reported missing (OSError)."""

    def _run(args, **kwargs):
        binary = args[0]
        canned = {"docker": docker, "podman": podman}[binary]
        if canned is None:
            raise FileNotFoundError(binary)
        if isinstance(canned, Exception):
            raise canned
        return canned

    monkeypatch.setattr(adapter.subprocess, "run", _run)


def test_doctor_check_oci_identity_podman_rootless_ok(
    adapter, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Podman rootless (``true``) is a supported identity mode."""
    _patch_engine_probe(
        adapter,
        monkeypatch,
        docker=None,
        podman=_FakeProc(0, b"true\n"),
    )
    finding = adapter._doctor_check_oci_identity()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"
    assert "rootless" in finding.detail.lower()


def test_doctor_check_oci_identity_podman_rootful_ok(
    adapter, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Podman rootful (``false``) follows the rootful rule — supported."""
    _patch_engine_probe(
        adapter,
        monkeypatch,
        docker=None,
        podman=_FakeProc(0, b"false\n"),
    )
    finding = adapter._doctor_check_oci_identity()
    assert finding.status == "ok"


def test_doctor_check_oci_identity_docker_rootful_ok(
    adapter, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Plain rootful Docker security options are supported."""
    _patch_engine_probe(
        adapter,
        monkeypatch,
        docker=_FakeProc(0, b"[name=apparmor name=seccomp,profile=builtin]"),
        podman=None,
    )
    finding = adapter._doctor_check_oci_identity()
    assert finding.status == "ok"
    assert "rootful" in finding.detail.lower()


def test_doctor_check_oci_identity_docker_rootless_ok(
    adapter, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``name=rootless`` in the security options is supported."""
    _patch_engine_probe(
        adapter,
        monkeypatch,
        docker=_FakeProc(0, b"[name=rootless name=seccomp,profile=builtin]"),
        podman=None,
    )
    finding = adapter._doctor_check_oci_identity()
    assert finding.status == "ok"
    assert "rootless" in finding.detail.lower()


def test_doctor_check_oci_identity_rootful_userns_remap_fails(
    adapter, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Rootful Docker with userns-remap is refused → host-capability-unavailable
    (doctor exit code 2 category)."""
    _patch_engine_probe(
        adapter,
        monkeypatch,
        docker=_FakeProc(0, b"[name=userns-remap,value=default]"),
        podman=None,
    )
    finding = adapter._doctor_check_oci_identity()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "fail"
    assert "userns-remap" in finding.detail.lower()


def test_doctor_check_oci_identity_unparseable_fails(
    adapter, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An installed engine whose mode cannot be parsed is fail-closed."""
    _patch_engine_probe(
        adapter,
        monkeypatch,
        docker=_FakeProc(0, b"not-a-security-options-list"),
        podman=None,
    )
    finding = adapter._doctor_check_oci_identity()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "fail"


def test_doctor_check_oci_identity_probe_failure_fails(
    adapter, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An installed engine whose ``info`` probe exits non-zero is fail-closed."""
    _patch_engine_probe(
        adapter,
        monkeypatch,
        docker=_FakeProc(1, b"", stderr=b"Cannot connect to the Docker daemon"),
        podman=None,
    )
    finding = adapter._doctor_check_oci_identity()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "fail"


def test_doctor_check_oci_identity_probe_timeout_fails(
    adapter, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A hung engine probe is fail-closed (mirrors the daemon's bounded timeout)."""
    _patch_engine_probe(
        adapter,
        monkeypatch,
        docker=subprocess.TimeoutExpired(cmd="docker", timeout=15),
        podman=None,
    )
    finding = adapter._doctor_check_oci_identity()
    assert finding.status == "fail"


def test_doctor_check_oci_identity_no_engine_ok(
    adapter, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No OCI engine installed → not applicable (trusted-host dispatch)."""
    _patch_engine_probe(adapter, monkeypatch, docker=None, podman=None)
    # Neither binary resolves on PATH.
    monkeypatch.setattr(adapter, "_binary_on_path", lambda _name: False)
    finding = adapter._doctor_check_oci_identity()
    assert finding.status == "ok"
    assert "not applicable" in finding.detail.lower()

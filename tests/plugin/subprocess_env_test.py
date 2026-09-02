"""Regression tests for the HERMES_HOME injection into daemon subprocesses.

Issue #263: `hermes caduceus status` failed in shells that do not export
HERMES_HOME because the adapter inherited the caller env verbatim. The
adapter must inject its computed default only when the variable is unset.
"""

from __future__ import annotations

import subprocess
from pathlib import Path


def _completed(argv: list) -> "subprocess.CompletedProcess[str]":
    return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")


class TestHermesHomeInjection:
    def test_run_passes_env_kwarg_to_subprocess(
        self, adapter, monkeypatch, isolated_hermes_home: Path
    ) -> None:
        """_run passes an explicit env (the seam this fix adds)."""
        monkeypatch.setenv("HERMES_HOME", str(isolated_hermes_home))
        captured: list[dict] = []

        def fake_run(argv, **kwargs):
            captured.append(kwargs)
            return _completed(argv)

        monkeypatch.setattr(adapter.subprocess, "run", fake_run)

        adapter._run(["/bin/true"])

        assert captured, "subprocess.run was not invoked"
        assert "env" in captured[0], captured[0]
        assert isinstance(captured[0]["env"], dict)

    def test_run_injects_default_hermes_home_when_unset_strict(
        self, adapter, monkeypatch, isolated_hermes_home: Path
    ) -> None:
        """The child env carries _hermes_home()'s value when the parent
        env does not export HERMES_HOME (the #263 regression)."""
        monkeypatch.delenv("HERMES_HOME", raising=False)
        captured: list[dict] = []

        def fake_run(argv, **kwargs):
            captured.append(kwargs)
            return _completed(argv)

        monkeypatch.setattr(adapter.subprocess, "run", fake_run)

        adapter._run(["/bin/true"])

        expected = str(adapter._hermes_home())
        assert captured[0]["env"]["HERMES_HOME"] == expected

    def test_run_preserves_explicit_hermes_home_override(
        self, adapter, monkeypatch, isolated_hermes_home: Path
    ) -> None:
        """An operator-exported HERMES_HOME reaches the child untouched."""
        override = str(isolated_hermes_home / "custom-profile")
        monkeypatch.setenv("HERMES_HOME", override)
        captured: list[dict] = []

        def fake_run(argv, **kwargs):
            captured.append(kwargs)
            return _completed(argv)

        monkeypatch.setattr(adapter.subprocess, "run", fake_run)

        adapter._run(["/bin/true"])

        assert captured[0]["env"]["HERMES_HOME"] == override

    def test_run_leaves_empty_hermes_home_verbatim(
        self, adapter, monkeypatch, isolated_hermes_home: Path
    ) -> None:
        """A deliberately-empty HERMES_HOME still reaches the binary (the
        Rust guard "HERMES_HOME must not be empty" is correct operator
        feedback and must not be masked)."""
        monkeypatch.setenv("HERMES_HOME", "")
        captured: list[dict] = []

        def fake_run(argv, **kwargs):
            captured.append(kwargs)
            return _completed(argv)

        monkeypatch.setattr(adapter.subprocess, "run", fake_run)

        adapter._run(["/bin/true"])

        assert captured[0]["env"]["HERMES_HOME"] == ""
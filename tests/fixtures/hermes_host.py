"""Hermes host fixture — hermetic Hermes Agent v0.18.2 test host.

This module provides a dataclass for recording evidence and a fixture
class that wraps subprocess calls to a real pinned Hermes installation.
Every method returns an ``EvidenceRecord`` documenting the command,
exit code, structured category, and artifact path.  The fixture is
designed for reuse by Phase 02 runtime work and Phase 07 verification.

The fixture never touches the operator's real ``~/.hermes`` home.
Instead, callers provide an isolated temp directory as ``hermes_home``.
Gateway restart is represented as an explicit prerequisite — the fixture
never invokes ``hermes gateway start`` or ``hermes gateway stop``.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import List


@dataclass
class EvidenceRecord:
    """A single piece of evidence from a Hermes host operation.

    Attributes:
        command: The command that was executed (argv joined as string).
        exit_code: The subprocess returncode, or -1 on timeout.
        category: A structured category (e.g. "lifecycle", "prerequisite",
            or a HERMES-002 category).
        artifact_path: Path to a file containing captured stdout/stderr,
            or "" when no output was captured.
    """

    command: str
    exit_code: int
    category: str
    artifact_path: str


class HermesHostFixture:
    """Fixture for running Hermes CLI commands in an isolated home directory.

    Every subprocess method records an ``EvidenceRecord``.  The full
    evidence list is accessible via the ``evidence`` property.
    """

    def __init__(
        self, hermes_home: Path, hermes_bin: str, plugin_root: Path
    ) -> None:
        self._hermes_home = hermes_home
        self._hermes_bin = hermes_bin
        self._plugin_root = plugin_root
        self._evidence: List[EvidenceRecord] = []

    # ------------------------------------------------------------------
    # Public properties
    # ------------------------------------------------------------------

    @property
    def evidence(self) -> List[EvidenceRecord]:
        """Return the accumulated evidence list."""
        return list(self._evidence)

    # ------------------------------------------------------------------
    # Subprocess methods
    # ------------------------------------------------------------------

    def install_plugin(self) -> EvidenceRecord:
        """Run ``hermes plugins install <repo> --enable``."""
        return self._run(
            [
                self._hermes_bin,
                "plugins",
                "install",
                "barkley-assistant/caduceus",
                "--enable",
            ],
            "install-plugin",
        )

    def setup(self) -> EvidenceRecord:
        """Run ``hermes caduceus setup`` inside the plugin root."""
        return self._run(
            [self._hermes_bin, "caduceus", "setup"],
            "setup",
            cwd=self._plugin_root,
        )

    def cron_install(self) -> EvidenceRecord:
        """Run ``hermes caduceus cron-install``."""
        return self._run(
            [self._hermes_bin, "caduceus", "cron-install"],
            "cron-install",
            cwd=self._plugin_root,
        )

    def cron_remove(self) -> EvidenceRecord:
        """Run ``hermes caduceus cron-remove``."""
        return self._run(
            [self._hermes_bin, "caduceus", "cron-remove"],
            "cron-remove",
            cwd=self._plugin_root,
        )

    def doctor(self) -> EvidenceRecord:
        """Run ``hermes caduceus doctor``."""
        return self._run(
            [self._hermes_bin, "caduceus", "doctor"],
            "doctor",
            cwd=self._plugin_root,
        )

    def status(self) -> EvidenceRecord:
        """Run ``hermes caduceus status``."""
        return self._run(
            [self._hermes_bin, "caduceus", "status"],
            "status",
            cwd=self._plugin_root,
        )

    def manual_run(self) -> EvidenceRecord:
        """Run ``hermes caduceus`` with no subcommand (triggers help/usage)."""
        return self._run(
            [self._hermes_bin, "caduceus"],
            "manual-run",
            cwd=self._plugin_root,
        )

    def teardown(self) -> None:
        """Remove the temp HERMES_HOME and record the gateway prerequisite.

        This method never invokes ``hermes gateway start`` or
        ``hermes gateway stop`` — the gateway restart is an explicit
        external prerequisite recorded as evidence.
        """
        if self._hermes_home.exists():
            shutil.rmtree(self._hermes_home)
        self._evidence.append(
            EvidenceRecord(
                command="gateway-prerequisite",
                exit_code=0,
                category="prerequisite",
                artifact_path="",
            )
        )

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _run(
        self,
        argv: List[str],
        method_name: str,
        cwd: Path | None = None,
    ) -> EvidenceRecord:
        """Run *argv*, capture output, write artifacts, return evidence.

        Args:
            argv: The argument list (never ``shell=True``).
            method_name: Used to derive artifact filenames.
            cwd: Working directory for the subprocess.

        Returns:
            An ``EvidenceRecord`` with the captured result.
        """
        env = os.environ.copy()
        env["HERMES_HOME"] = str(self._hermes_home)
        self._hermes_home.mkdir(parents=True, exist_ok=True)

        stdout_path = self._hermes_home / f"{method_name}-stdout.txt"
        stderr_path = self._hermes_home / f"{method_name}-stderr.txt"

        try:
            proc = subprocess.run(
                argv,
                env=env,
                cwd=str(cwd) if cwd else None,
                capture_output=True,
                text=True,
                timeout=30,
            )
            exit_code = proc.returncode
            category = "lifecycle"
            stdout_path.write_text(proc.stdout or "", encoding="utf-8")
            stderr_path.write_text(proc.stderr or "", encoding="utf-8")
        except FileNotFoundError:
            exit_code = 127
            category = "lifecycle"
            stdout_path.write_text("", encoding="utf-8")
            stderr_path.write_text(
                f"command not found: {argv[0]}\n", encoding="utf-8"
            )
        except subprocess.TimeoutExpired as exc:
            exit_code = -1
            category = "timed_out"
            stdout_path.write_text(exc.stdout or "", encoding="utf-8")
            stderr_path.write_text(exc.stderr or "", encoding="utf-8")

        record = EvidenceRecord(
            command=" ".join(argv),
            exit_code=exit_code,
            category=category,
            artifact_path=str(stdout_path),
        )
        self._evidence.append(record)
        return record
"""Self-tests for the HermesHostFixture.

These tests exercise the fixture class directly without requiring a real
Hermes binary on PATH.  They verify that EvidenceRecord is constructable,
that the fixture initialises cleanly, and that the evidence list is
empty before any operations.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from tests.fixtures.hermes_host import EvidenceRecord, HermesHostFixture


# ---------------------------------------------------------------------------
# EvidenceRecord construction
# ---------------------------------------------------------------------------


def test_evidence_record_default_artifact_path() -> None:
    """``EvidenceRecord`` can be constructed with a default artifact path."""
    record = EvidenceRecord(
        command="hermes caduceus setup",
        exit_code=0,
        category="lifecycle",
        artifact_path="",
    )
    assert record.command == "hermes caduceus setup"
    assert record.exit_code == 0
    assert record.category == "lifecycle"
    assert record.artifact_path == ""


def test_evidence_record_with_artifact() -> None:
    """``EvidenceRecord`` stores an artifact path when provided."""
    record = EvidenceRecord(
        command="hermes caduceus status",
        exit_code=0,
        category="lifecycle",
        artifact_path="/tmp/artifact.txt",
    )
    assert record.artifact_path == "/tmp/artifact.txt"


# ---------------------------------------------------------------------------
# HermesHostFixture initialisation
# ---------------------------------------------------------------------------


def test_fixture_init_empty_evidence() -> None:
    """A fresh fixture starts with an empty evidence list."""
    fixture = HermesHostFixture(
        hermes_home=Path("/tmp/test-home"),
        hermes_bin="/usr/local/bin/hermes",
        plugin_root=Path("/tmp/test-plugin"),
    )
    assert fixture.evidence == []


def test_fixture_init_stores_constructor_args() -> None:
    """Constructor arguments are stored as private attributes."""
    fixture = HermesHostFixture(
        hermes_home=Path("/tmp/alpha"),
        hermes_bin="/opt/hermes/bin/hermes",
        plugin_root=Path("/tmp/beta"),
    )
    assert fixture._hermes_home == Path("/tmp/alpha")
    assert fixture._hermes_bin == "/opt/hermes/bin/hermes"
    assert fixture._plugin_root == Path("/tmp/beta")


# ---------------------------------------------------------------------------
# HermesHostFixture install_plugin (stub — no real hermes binary needed)
# ---------------------------------------------------------------------------


def test_fixture_install_plugin_without_binary(tmp_path: Path) -> None:
    """``install_plugin`` returns an EvidenceRecord even when hermes is absent.

    Because the fixture uses ``subprocess.run`` with ``shell=False``, a
    missing binary raises ``FileNotFoundError`` which is caught by
    ``_run`` and recorded as exit code 127.
    """
    fixture = HermesHostFixture(
        hermes_home=tmp_path / ".hermes",
        hermes_bin="/nonexistent/hermes",
        plugin_root=tmp_path / "plugin",
    )
    record = fixture.install_plugin()
    assert isinstance(record, EvidenceRecord)
    assert "plugins install barkley-assistant/caduceus --enable" in record.command
    assert record.exit_code == 127
    assert record.category == "lifecycle"
    assert fixture.evidence == [record]


def test_fixture_evidence_appends_on_each_call(tmp_path: Path) -> None:
    """Each fixture method call appends one EvidenceRecord."""
    fixture = HermesHostFixture(
        hermes_home=tmp_path / ".hermes",
        hermes_bin="/nonexistent/hermes",
        plugin_root=tmp_path / "plugin",
    )
    r1 = fixture.install_plugin()
    r2 = fixture.setup()
    assert len(fixture.evidence) == 2
    assert fixture.evidence[0] is r1
    assert fixture.evidence[1] is r2


def test_fixture_teardown_removes_home(tmp_path: Path) -> None:
    """``teardown`` removes the temp HERMES_HOME and records prerequisite."""
    hermes_home = tmp_path / ".hermes"
    hermes_home.mkdir(parents=True, exist_ok=True)
    (hermes_home / "marker.txt").write_text("test")
    fixture = HermesHostFixture(
        hermes_home=hermes_home,
        hermes_bin="/nonexistent/hermes",
        plugin_root=tmp_path / "plugin",
    )
    fixture.teardown()
    assert not hermes_home.exists()
    # The prerequisite row is in evidence.
    prereq = fixture.evidence[-1]
    assert prereq.command == "gateway-prerequisite"
    assert prereq.exit_code == 0
    assert prereq.category == "prerequisite"
    assert prereq.artifact_path == ""


def test_fixture_teardown_idempotent(tmp_path: Path) -> None:
    """Calling ``teardown`` twice does not raise."""
    hermes_home = tmp_path / ".hermes"
    hermes_home.mkdir(parents=True, exist_ok=True)
    fixture = HermesHostFixture(
        hermes_home=hermes_home,
        hermes_bin="/nonexistent/hermes",
        plugin_root=tmp_path / "plugin",
    )
    fixture.teardown()
    fixture.teardown()  # must not raise


def test_fixture_run_returns_evidence_record(tmp_path: Path) -> None:
    """``_run`` returns an EvidenceRecord with captured output."""
    fixture = HermesHostFixture(
        hermes_home=tmp_path / ".hermes",
        hermes_bin="/bin/echo",
        plugin_root=tmp_path / "plugin",
    )
    record = fixture._run(["/bin/echo", "hello"], "echo_test")
    assert isinstance(record, EvidenceRecord)
    assert record.exit_code == 0
    assert record.category == "lifecycle"
    assert record.command == "/bin/echo hello"
    assert record.artifact_path != ""
    # Artifact files should exist
    stdout_path = tmp_path / ".hermes" / "echo_test-stdout.txt"
    assert stdout_path.exists()
    assert stdout_path.read_text(encoding="utf-8").strip() == "hello"
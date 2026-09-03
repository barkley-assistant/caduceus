"""Hermes wrapper pass-through CLI tests.

``hermes caduceus`` forwards ``queue``, ``worktree-gc``, and
``migrate-state`` verbatim to the ``caduceus`` binary, and forwards
``status --json``. The wrapper never re-encodes the flag contract —
the clap parser in the binary is the single source of truth. The
pass-through subparsers use a custom argparse parser so a
flags-first invocation (e.g. ``worktree-gc --older-than-days 7``)
parses under Hermes' strict top-level ``parse_args``.
"""

from __future__ import annotations

import subprocess
from typing import Any, Dict, List

import pytest

from tests.fixtures.fake_ctx import FakePluginContext


def _parse_and_dispatch(adapter, fake_ctx: FakePluginContext, *argv: str) -> Any:
    """Parse *argv* against the registered CLI and run the handler."""
    adapter.register(fake_ctx)
    parser = fake_ctx.cli_commands["caduceus"].parser
    args = parser.parse_args(list(argv))
    return args.func(args)


def _record_run(adapter, monkeypatch: pytest.MonkeyPatch) -> List[List[str]]:
    """Replace ``_run`` with a recorder that succeeds with empty output."""
    calls: List[List[str]] = []

    def fake_run(argv: list, **kwargs: Any) -> subprocess.CompletedProcess[str]:
        calls.append(list(argv))
        return subprocess.CompletedProcess(argv, 0, "stub", "")

    monkeypatch.setattr(adapter, "_run", fake_run)
    return calls


def test_cli_help_lists_full_command_set(adapter, fake_ctx: FakePluginContext) -> None:
    """``hermes caduceus --help`` lists every subcommand incl. pass-through."""
    adapter.register(fake_ctx)
    parser = fake_ctx.cli_commands["caduceus"].parser
    help_text = parser.format_help()
    for sub in (
        "setup",
        "doctor",
        "status",
        "cron-install",
        "cron-remove",
        "queue",
        "worktree-gc",
        "migrate-state",
    ):
        assert sub in help_text, f"missing subcommand {sub} in help"


def test_status_json_is_forwarded_to_binary(
    adapter, fake_ctx: FakePluginContext, install_with_fake_binary, monkeypatch
) -> None:
    """``status --json`` appends ``--json`` to the binary argv."""
    calls = _record_run(adapter, monkeypatch)
    rc = _parse_and_dispatch(adapter, fake_ctx, "status", "--json")
    assert rc == 0
    assert calls == [[str(install_with_fake_binary), "status", "--json"]]


def test_status_without_json_does_not_append_flag(
    adapter, fake_ctx: FakePluginContext, install_with_fake_binary, monkeypatch
) -> None:
    calls = _record_run(adapter, monkeypatch)
    rc = _parse_and_dispatch(adapter, fake_ctx, "status")
    assert rc == 0
    assert calls == [[str(install_with_fake_binary), "status"]]


def test_status_json_end_to_end_via_fake_binary(
    adapter, fake_ctx: FakePluginContext, install_with_fake_binary, capsys
) -> None:
    """The real ``_run`` path passes ``--json`` through to the binary."""
    rc = _parse_and_dispatch(adapter, fake_ctx, "status", "--json")
    captured = capsys.readouterr()
    assert rc == 0
    assert '"app_version"' in captured.out


def test_queue_passthrough_forwards_args_verbatim(
    adapter, fake_ctx: FakePluginContext, install_with_fake_binary, monkeypatch
) -> None:
    """``queue show --json`` reaches the binary as ``queue show --json``."""
    calls = _record_run(adapter, monkeypatch)
    rc = _parse_and_dispatch(adapter, fake_ctx, "queue", "show", "--json")
    assert rc == 0
    assert calls == [[str(install_with_fake_binary), "queue", "show", "--json"]]


def test_queue_remove_passthrough_forwards_flags(
    adapter, fake_ctx: FakePluginContext, install_with_fake_binary, monkeypatch
) -> None:
    calls = _record_run(adapter, monkeypatch)
    rc = _parse_and_dispatch(adapter, fake_ctx, "queue", "remove", "o/r#1", "--force")
    assert rc == 0
    assert calls == [
        [str(install_with_fake_binary), "queue", "remove", "o/r#1", "--force"]
    ]


def test_worktree_gc_passthrough_accepts_flags_first(
    adapter, fake_ctx: FakePluginContext, install_with_fake_binary, monkeypatch
) -> None:
    """Regression: a flags-first invocation must not trip argparse.

    Plain ``nargs=argparse.REMAINDER`` rejects
    ``worktree-gc --older-than-days 7 --dry-run`` with
    ``unrecognized arguments`` under Hermes' strict top-level parse;
    the pass-through parser must forward it verbatim instead.
    """
    calls = _record_run(adapter, monkeypatch)
    rc = _parse_and_dispatch(
        adapter, fake_ctx, "worktree-gc", "--older-than-days", "7", "--dry-run"
    )
    assert rc == 0
    assert calls == [
        [str(install_with_fake_binary), "worktree-gc", "--older-than-days", "7", "--dry-run"]
    ]


def test_migrate_state_passthrough_forwards_flags(
    adapter, fake_ctx: FakePluginContext, install_with_fake_binary, monkeypatch
) -> None:
    calls = _record_run(adapter, monkeypatch)
    rc = _parse_and_dispatch(adapter, fake_ctx, "migrate-state", "--to-sqlite", "--dry-run")
    assert rc == 0
    assert calls == [
        [str(install_with_fake_binary), "migrate-state", "--to-sqlite", "--dry-run"]
    ]


def test_migrate_state_passthrough_forwards_positional(
    adapter, fake_ctx: FakePluginContext, install_with_fake_binary, monkeypatch
) -> None:
    calls = _record_run(adapter, monkeypatch)
    rc = _parse_and_dispatch(adapter, fake_ctx, "migrate-state", "--from", "/tmp/legacy.json")
    assert rc == 0
    assert calls == [
        [str(install_with_fake_binary), "migrate-state", "--from", "/tmp/legacy.json"]
    ]


def test_passthrough_propagates_binary_exit_code(
    adapter, fake_ctx: FakePluginContext, install_with_fake_binary, monkeypatch
) -> None:
    def failing_run(argv: list, **kwargs: Any) -> subprocess.CompletedProcess[str]:
        return subprocess.CompletedProcess(argv, 7, "", "boom")

    monkeypatch.setattr(adapter, "_run", failing_run)
    rc = _parse_and_dispatch(adapter, fake_ctx, "queue", "remove", "o/r#1")
    assert rc == 7


def test_passthrough_missing_binary_returns_diagnostic(
    adapter, fake_ctx: FakePluginContext, capsys
) -> None:
    """Without an installed binary the wrapper returns 1 with a setup hint."""
    rc = _parse_and_dispatch(adapter, fake_ctx, "queue", "show")
    captured = capsys.readouterr()
    assert rc == 1
    assert "hermes caduceus setup" in captured.err
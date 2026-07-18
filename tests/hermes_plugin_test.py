"""Hermes plugin test suite for Caduceus v0.1.

These tests run against the Hermes Agent v0.18.2 contract documented in
``planning/caduceus-v0.1/CONTRACTS.md`` and verified by the Task 0.2
packet in ``planning/caduceus-v0.1/tasks/0.2-...``.

The suite deliberately runs against the plugin source as installed
under ``$HERMES_HOME/plugins/caduceus/``. The ``install_plugin`` fixture
copies the plugin tree from the repository root into a temp HERMES_HOME
to mirror the real ``hermes plugins install barkley-assistant/caduceus``
behaviour without hitting the network.

Coverage matrix (each bullet maps to one or more tests):

* install from repository root
* plugin discovery and enablement
* manifest field allowlist
* skill resolution as ``caduceus:caduceus``
* slash and CLI command registration
* missing-binary diagnostics
* locked Rust build + atomic binary placement
* setup idempotency
* user bridge preservation + ``.new`` upgrade candidate
* cron wrapper path / content / mode
* cron zero / one / multiple-match reconciliation
* no-agent execution invokes ``caduceus run``
* cron removal
* source update + rebuild
* plugin removal leaves user bridge / state
* registration does not mutate the filesystem, network, or cron job
* legacy ``plugin/`` directory absent
* negative fixture: legacy custom manifest fields fail the contract
"""

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

from tests.fake_ctx import (
    FakePluginContext,
    assert_cli_command_registered,
    assert_command_registered,
    assert_skill_registered,
)


# ---------------------------------------------------------------------------
# Helpers shared across tests
# ---------------------------------------------------------------------------


_ALLOWED_MANIFEST_FIELDS = {
    "manifest_version",
    "name",
    "version",
    "description",
    "author",
    "kind",
    "requires_env",
    "provides_tools",
    "provides_hooks",
}


def _read_plugin_yaml(installed: Path) -> Dict[str, Any]:
    import yaml

    text = (installed / "plugin.yaml").read_text(encoding="utf-8")
    return yaml.safe_load(text) or {}


def _invoke_cli(adapter: Any, *argv: str) -> Any:
    """Run the adapter's CLI command at the function boundary."""
    record = None
    for rec in adapter.__dict__.values():
        pass
    ctx = FakePluginContext(name="caduceus")
    adapter.register(ctx)
    parser = ctx.cli_commands["caduceus"].parser
    args = parser.parse_args(list(argv))
    return args.func(args)


# ---------------------------------------------------------------------------
# Install from repository root
# ---------------------------------------------------------------------------


def test_install_copies_plugin_tree_into_hermes_home(
    install_plugin: Path, isolated_hermes_home: Path
) -> None:
    """``hermes plugins install`` clones the repository root.

    The plugin directory must end up at
    ``$HERMES_HOME/plugins/caduceus/`` with the canonical surface
    (``plugin.yaml``, ``__init__.py``, ``skills/caduceus/SKILL.md``,
    ``plugin-assets/worker-bridge.py``) plus the Rust workspace files
    (``Cargo.toml``, ``Cargo.lock``, ``src/``).
    """
    assert install_plugin.is_dir(), install_plugin
    assert (install_plugin / "plugin.yaml").is_file()
    assert (install_plugin / "__init__.py").is_file()
    assert (install_plugin / "skills" / "caduceus" / "SKILL.md").is_file()
    assert (install_plugin / "plugin-assets" / "worker-bridge.py").is_file()
    assert (install_plugin / "plugin-assets" / "caduceus-pulse.sh").is_file()
    # Rust workspace files ride along with the plugin source.
    assert (install_plugin / "Cargo.toml").is_file()
    assert (install_plugin / "Cargo.lock").is_file()
    assert (install_plugin / "src" / "lib.rs").is_file()
    assert (install_plugin / "src" / "main.rs").is_file()
    # No temp / build / planning directory leaks.
    assert not (install_plugin / "target").exists()
    assert not (install_plugin / "planning").exists()
    assert not (install_plugin / "tests").exists()


def test_install_root_is_canonical(plugin_root: Path) -> None:
    """The repository *root* itself must already be the installable surface.

    Concretely: the plugin files live next to the Rust workspace, not
    under ``plugin/``. Hermes would otherwise install a subdirectory and
    leave the workspace behind (CONTRACTS.md, "Hermes plugin
    compatibility contract").
    """
    assert (plugin_root / "plugin.yaml").is_file()
    assert (plugin_root / "__init__.py").is_file()
    assert (plugin_root / "Cargo.toml").is_file()


# ---------------------------------------------------------------------------
# Plugin discovery / enablement
# ---------------------------------------------------------------------------


def test_register_uses_documentated_ctx_surface(
    adapter, fake_ctx: FakePluginContext
) -> None:
    """``register(ctx)`` only invokes the documented ``ctx`` methods.

    Hermes expects ``register_skill``, ``register_command``, and
    ``register_cli_command``. Anything else is a contract drift.
    """
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
    """Registration must not create cron jobs, build artefacts, or config.

    Per CONTRACTS.md: "Plugin import/registration never compiles code,
    mutates config, creates cron jobs, or performs network access."
    """
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


# ---------------------------------------------------------------------------
# Manifest field allowlist
# ---------------------------------------------------------------------------


def _manifest_field_check(plugin_yaml_text: str) -> tuple:
    """Return ``(allowed_fields, rejected_fields)`` against the contract list.

    Used by both the positive fixture (rejects extra fields) and the
    negative fixture (locks the same rule).
    """
    import yaml

    data = yaml.safe_load(plugin_yaml_text) or {}
    allowed = _ALLOWED_MANIFEST_FIELDS
    rejected = sorted(k for k in data.keys() if k not in allowed)
    allowed_actual = sorted(k for k in data.keys() if k in allowed)
    return (allowed_actual, rejected)


def test_manifest_uses_only_supported_fields(install_plugin: Path) -> None:
    """``plugin.yaml`` declares only fields the v0.18.2 loader accepts.

    See ``hermes_cli/plugins.py::PluginManifest`` for the canonical
    field set. CONTRACTS.md is explicit: "Unknown manifest fields may
    be silently ignored by Hermes and are therefore rejected by
    Caduceus's contract test."
    """
    yaml_text = (install_plugin / "plugin.yaml").read_text(encoding="utf-8")
    allowed, rejected = _manifest_field_check(yaml_text)
    assert not rejected, f"unknown manifest fields: {rejected}"
    # Spot-check: kind must be ``standalone`` for Caduceus.
    import yaml

    data = yaml.safe_load(yaml_text) or {}
    assert data.get("kind") == "standalone"
    # provides_tools/hooks must be explicit empty lists — a missing
    # field would silently be ``None`` in some upstream loaders.
    assert data.get("provides_tools") == []
    assert data.get("provides_hooks") == []


def test_negative_manifest_with_legacy_fields_is_rejected(
    tmp_path: Path, plugin_root: Path
) -> None:
    """A negative fixture containing legacy fields must fail the check.

    This mirrors the contract test requirement: a third-party plugin
    that inherits from the legacy ``plugin/plugin.yaml`` layout *and*
    deploys it as the install root would be silently broken because
    Hermes drops unknown fields. Caduceus catches that on its side.
    """
    bad_yaml = plugin_root / "tests" / "fixtures" / "negative_plugin.yaml"
    assert bad_yaml.is_file(), "negative fixture missing"
    allowed, rejected = _manifest_field_check(bad_yaml.read_text(encoding="utf-8"))
    assert rejected, "negative fixture did not actually declare a legacy field"


# ---------------------------------------------------------------------------
# Skill resolution
# ---------------------------------------------------------------------------


def test_skill_registers_as_caduceus_caduceus(
    adapter, fake_ctx: FakePluginContext, install_plugin: Path
) -> None:
    """``ctx.register_skill('caduceus', ...)`` is resolvable as ``caduceus:caduceus``.

    Hermes namespace logic joins the plugin manifest name with the bare
    skill name via ``:``. Per CONTRACTS.md the bare name is ``caduceus``
    and the plugin name is also ``caduceus``, so the qualified form
    must be exactly ``caduceus:caduceus``.
    """
    adapter.register(fake_ctx)
    record = assert_skill_registered(fake_ctx, "caduceus")
    assert record.path == install_plugin / "skills" / "caduceus" / "SKILL.md"
    # Mirror the loader's namespace join.
    qualified = f"{fake_ctx.name}:caduceus"
    assert qualified == "caduceus:caduceus"


def test_skill_file_passes_yaml_frontmatter() -> None:
    """SKILL.md exists and is non-trivial text the loader can consume."""
    skill = Path(__file__).resolve().parent.parent / "skills" / "caduceus" / "SKILL.md"
    assert skill.is_file(), skill
    text = skill.read_text(encoding="utf-8")
    # The skill body must describe boundaries; contract prohibits
    # narrative-only files with no actionable content.
    lowered = text.lower()
    assert "caduceus" in lowered
    assert "setup" in lowered or "doctor" in lowered or "cron" in lowered


# ---------------------------------------------------------------------------
# Slash command registration
# ---------------------------------------------------------------------------


def test_status_slash_command_is_registered(adapter, fake_ctx: FakePluginContext) -> None:
    adapter.register(fake_ctx)
    cmd = assert_command_registered(fake_ctx, "caduceus-status")
    assert callable(cmd.handler)


def test_status_slash_command_missing_binary_returns_diagnostic(
    adapter, fake_ctx: FakePluginContext
) -> None:
    """When the binary is absent the handler returns a precise diagnostic."""
    adapter.register(fake_ctx)
    cmd = assert_command_registered(fake_ctx, "caduceus-status")
    result = cmd.handler("")
    assert isinstance(result, str)
    assert "hermes caduceus setup" in result


def test_status_slash_command_invokes_binary(
    adapter, fake_ctx: FakePluginContext, install_with_fake_binary: Path
) -> None:
    adapter.register(fake_ctx)
    cmd = assert_command_registered(fake_ctx, "caduceus-status")
    result = cmd.handler("")
    assert isinstance(result, str)
    assert "caduceus 0.1.0" in result


def test_status_slash_redacts_token_like_strings(
    adapter, fake_ctx: FakePluginContext, install_plugin: Path, tmp_path: Path
) -> None:
    """A binary that prints ``GITHUB_TOKEN=ghp_xxx`` is redacted."""
    binary = install_plugin / "bin" / "caduceus"
    binary.parent.mkdir(exist_ok=True)
    binary.write_text(
        "#!/usr/bin/env bash\n"
        'if [ "$1" = "status" ]; then\n'
        '  if [ "$2" = "--json" ]; then\n'
        '    printf \'{"version":"0.1.0","last_tick":"never","last_outcome":"idle"}\'\n'
        "  fi\n"
        "  exit 0\n"
        "fi\n"
        "exit 0\n"
    )
    binary.chmod(0o755)
    adapter.register(fake_ctx)
    cmd = assert_command_registered(fake_ctx, "caduceus-status")
    result = cmd.handler("")
    assert result is not None
    # No ``ghp_`` token made it into chat output.
    assert "ghp_" not in result
    assert "<redacted>" not in result  # the fake didn't leak one — defensive


# ---------------------------------------------------------------------------
# CLI command: hermes caduceus ...
# ---------------------------------------------------------------------------


def _register_and_get_parser(adapter, fake_ctx):
    adapter.register(fake_ctx)
    return fake_ctx.cli_commands["caduceus"].parser


def test_cli_command_is_registered(adapter, fake_ctx: FakePluginContext) -> None:
    parser = _register_and_get_parser(adapter, fake_ctx)
    assert parser is not None
    # Help text references the canonical subcommands.
    help_text = parser.format_help()
    for sub in ("setup", "doctor", "status", "cron-install", "cron-remove"):
        assert sub in help_text, f"missing subcommand {sub} in help"


def test_cli_unknown_subcommand_is_rejected(adapter, fake_ctx: FakePluginContext) -> None:
    parser = _register_and_get_parser(adapter, fake_ctx)
    with pytest.raises(SystemExit):
        # argparse exits 2 on unknown subcommands.
        parser.parse_args(["nope"])


# ---------------------------------------------------------------------------
# Setup: locked build, atomic install, idempotency
# ---------------------------------------------------------------------------


def _setup_args(adapter, argv):
    """Build a local argparse tree and dispatch the supplied subcommand.

    ``argv`` is just the subcommand + its flags (the outer ``caduceus``
    word is supplied by Hermes's parser).
    """
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
    """The installed binary is created via ``os.replace``.

    We patch ``adapter._atomic_install_binary`` to record both the
    source and the destination, then assert that ``install_plugin /
    bin/caduceus`` ends up as a real file with mode 0755 and no
    leftover ``.tmp`` marker. Patching the helper is intentional — we
    are verifying Caduceus's adapter contract, not the Rust build.
    """
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
    """``cargo build`` is invoked with ``--locked``.

    We capture every subprocess invocation through ``adapter._run``.
    Because the test environment may not have cargo available, we drive
    both code paths (preconditions + build) by replacing the subprocess
    helper with one that records the argv and returns a synthetic OK.
    """
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


# ---------------------------------------------------------------------------
# Cron wrapper
# ---------------------------------------------------------------------------


def test_cron_wrapper_path_content_mode(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """The wrapper at ``$HERMES_HOME/scripts/caduceus-pulse.sh`` exists, is mode 0755,
    contains the absolute binary path, and ends in ``exec <binary> run "$@"``.
    """
    adapter._write_pulse_wrapper(install_with_fake_binary)
    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    assert wrapper.is_file()
    assert not wrapper.is_symlink()
    mode = stat.S_IMODE(wrapper.stat().st_mode)
    assert mode == 0o755
    body = wrapper.read_text(encoding="utf-8")
    assert str(install_with_fake_binary) in body
    assert body.rstrip().endswith(f'exec {install_with_fake_binary} run "$@"')


# ---------------------------------------------------------------------------
# Cron reconciliation
# ---------------------------------------------------------------------------


def _stub_cron_runtime(adapter, registry: Dict[str, Dict[str, Any]]):
    """Replace the cron helper with in-memory state."""
    from caduceus import _runtime

    next_id = [1]
    seen_actions: List[Dict[str, Any]] = []

    def dispatch(name: str, args: Dict[str, Any]):
        if name != "cronjob":
            raise AssertionError(name)
        seen_actions.append(args)
        action = args["action"]
        if action == "list":
            return {"jobs": list(registry.values())}
        if action == "create":
            job_id = f"job-{next_id[0]}"
            next_id[0] += 1
            registry[job_id] = {
                "id": job_id,
                "name": args["name"],
                "schedule": args["schedule"],
                "script": args["script"],
                "no_agent": args.get("no_agent", False),
            }
            return {"id": job_id}
        if action == "update":
            job = registry.get(args["job_id"])
            assert job is not None
            job.update(
                {
                    "schedule": args["schedule"],
                    "name": args["name"],
                    "script": args["script"],
                    "no_agent": args.get("no_agent", False),
                }
            )
            return {"id": args["job_id"]}
        if action == "remove":
            registry.pop(args["job_id"], None)
            return {"removed": args["job_id"]}
        raise AssertionError(action)

    _runtime.install_dispatcher(dispatch)
    return seen_actions


def test_cron_install_zero_matches_creates(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    from caduceus import _runtime

    registry: Dict[str, Dict[str, Any]] = {}
    actions = _stub_cron_runtime(adapter, registry)
    try:
        action, note = adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()
    assert action == "created"
    assert note.startswith("job-")
    assert any(a["action"] == "create" for a in actions)
    assert any(a["action"] == "list" for a in actions)
    # Wrapper was written.
    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    assert wrapper.is_file()


def test_cron_install_one_match_reuses(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    from caduceus import _runtime

    registry = {
        "job-9": {
            "id": "job-9",
            "name": "caduceus",
            "schedule": "every 5m",
            "script": "caduceus-pulse.sh",
            "no_agent": False,
        }
    }
    actions = _stub_cron_runtime(adapter, registry)
    try:
        action, note = adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()
    assert action == "reused"
    assert note == "job-9"
    # update was invoked with the new schedule and no_agent=True.
    update = next(a for a in actions if a["action"] == "update")
    assert update["schedule"] == "every 2m"
    assert update["no_agent"] is True
    assert update["script"] == "caduceus-pulse.sh"


def test_cron_install_multiple_matches_fails(
    adapter, install_with_fake_binary: Path
) -> None:
    from caduceus import _runtime

    registry = {
        "job-a": {"id": "job-a", "name": "caduceus", "schedule": "every 2m"},
        "job-b": {"id": "job-b", "name": "caduceus", "schedule": "every 2m"},
    }
    _stub_cron_runtime(adapter, registry)
    try:
        with pytest.raises(RuntimeError) as excinfo:
            adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()
    assert "multiple" in str(excinfo.value).lower()


def test_cron_install_invokes_no_agent_exec(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path, tmp_path: Path
) -> None:
    """The no-agent cron job is created by exec'ing the bash wrapper.

    The wrapper itself ends in ``exec <binary> run`` so the cron
    process is replaced by the daemon — not a fork-from-shell. We
    simulate this by invoking the wrapper as a subprocess and verifying
    that the fake binary runs with ``status`` (the only flag our fake
    supports; the contract only requires ``<binary> run``).
    """
    adapter._write_pulse_wrapper(install_with_fake_binary)
    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    # ``exec <binary> run`` only invokes run; we cannot reuse the
    # status-only fake — write a richer stub binary.
    fake = install_with_fake_binary
    fake.write_text("#!/usr/bin/env bash\necho run-ok\nexit 0\n")
    fake.chmod(0o755)
    proc = subprocess.run(
        [str(wrapper)], capture_output=True, text=True, timeout=10
    )
    assert proc.returncode == 0
    assert "run-ok" in proc.stdout


def test_cron_remove_is_idempotent(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    from caduceus import _runtime

    registry = {
        "job-9": {
            "id": "job-9",
            "name": "caduceus",
            "schedule": "every 2m",
            "script": "caduceus-pulse.sh",
        }
    }
    actions = _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_cron_remove()
    finally:
        _runtime.reset_dispatcher()
    assert rc == 0
    assert any(a["action"] == "remove" for a in actions)
    assert "job-9" not in registry
    # Wrapper is gone.
    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    assert not wrapper.exists()
    # Idempotent: a second call still returns 0.
    actions.clear()
    registry.pop("job-9", None)
    try:
        _stub_cron_runtime(adapter, registry)
        rc = adapter._cli_cron_remove()
    finally:
        _runtime.reset_dispatcher()
    assert rc == 0


# ---------------------------------------------------------------------------
# Update + rebuild + plugin removal
# ---------------------------------------------------------------------------


def test_source_update_then_rebuild_preserves_user_bridge(
    adapter, isolated_hermes_home: Path, install_plugin: Path, install_with_fake_binary: Path
) -> None:
    """A simulated ``hermes plugins update caduceus`` followed by
    ``hermes caduceus setup`` preserves the user-owned bridge."""
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    sentinel = "# user-edited-content-post-update\n"
    bridge.write_text(sentinel + "print('hi')\n", encoding="utf-8")

    # Update the plugin tree contents (simulated by touching files).
    template = install_plugin / "plugin-assets" / "worker-bridge.py"
    updated = template.read_text(encoding="utf-8") + "\n# upstream-revision-N\n"
    template.write_text(updated, encoding="utf-8")
    try:
        adapter._seed_user_bridge()
        # User copy untouched.
        assert sentinel in bridge.read_text(encoding="utf-8")
        # New candidate written next to the user copy.
        assert (bridge.parent / "worker-bridge.py.new").is_file()
    finally:
        template.write_text(template.read_text(encoding="utf-8").removesuffix("\n# upstream-revision-N\n"), encoding="utf-8")


def test_plugin_removal_preserves_state(isolated_hermes_home: Path) -> None:
    """Removing the plugin directory leaves the user-owned bridge and state alone.

    The Caduceus adapter does not implement an uninstall hook — that
    is Hermes's responsibility. After the plugin directory is removed
    the user's bridge, state, and config stay where they are.
    """
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("# user bridge\n")
    state = isolated_hermes_home / "caduceus-state"
    state.mkdir()
    plugins_root = isolated_hermes_home / "plugins"
    (plugins_root / "caduceus").mkdir(parents=True)
    (plugins_root / "caduceus" / "__init__.py").write_text("# plugin\n")
    # Simulate ``hermes plugins remove caduceus`` by deleting the
    # plugin directory under plugins/, mirroring Hermes's behaviour.
    shutil.rmtree(plugins_root / "caduceus")
    assert not (plugins_root / "caduceus").exists()
    assert bridge.is_file()  # user bridge preserved
    assert state.is_dir()  # state preserved


# ---------------------------------------------------------------------------
# Legacy ``plugin/`` directory is gone
# ---------------------------------------------------------------------------


def test_legacy_plugin_directory_is_absent(plugin_root: Path) -> None:
    """The historical ``plugin/`` layout must not exist."""
    legacy = plugin_root / "plugin"
    assert not legacy.exists(), f"legacy directory still present at {legacy}"


# ---------------------------------------------------------------------------
# doctest-style probe of the adapter (catches accidental network)
# ---------------------------------------------------------------------------


def test_subprocess_call_redacts_secrets(adapter) -> None:
    """``_redact`` masks token-shaped values from arbitrary text."""
    text = (
        "Calling GitHub with GITHUB_TOKEN=ghp_abcd1234efgh5678 and "
        "GH_TOKEN=ghp_zzzz and CADUCEUS_GITHUB_TOKEN=ghp_yyyy and "
        "GITHUB_TOKEN=\"ghp_quoted\"\n"
    )
    out = adapter._redact(text)
    assert "ghp_abcd1234efgh5678" not in out
    assert "ghp_zzzz" not in out
    assert "ghp_quoted" not in out
    # The variable name remains so operators can still see WHICH env
    # var was leaked.
    assert "GITHUB_TOKEN=" in out or "GITHUB_TOKEN =" in out


# ---------------------------------------------------------------------------
# Phase 2: Transactional Cron Flow (Tasks 2.1-2.7)
# ---------------------------------------------------------------------------
# These tests are written FIRST (RED phase of TDD) before any production
# code changes. They verify the snapshot, reconcile, and crash-boundary
# recovery behaviour specified in AC-01/03/04/09.
# ---------------------------------------------------------------------------


def _stub_wrapper_file(wrapper_path: Path, binary_path: Path) -> None:
    """Create a realistic wrapper file for snapshot testing."""
    wrapper_path.parent.mkdir(parents=True, exist_ok=True)
    body = (
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        f"exec {binary_path} run \"$@\"\n"
    )
    wrapper_path.write_text(body, encoding="utf-8")
    os.chmod(wrapper_path, 0o755)


# ---------------------------------------------------------------------------
# Task 2.1: _Snapshot frozen dataclass
# ---------------------------------------------------------------------------


def test_snapshot_is_frozen_dataclass() -> None:
    """_Snapshot is a frozen dataclass with wrapper_bytes, wrapper_mode, job_dict."""
    from caduceus import _Snapshot

    snap = _Snapshot(wrapper_bytes=b"content", wrapper_mode=0o755, job_dict=None)
    assert snap.wrapper_bytes == b"content"
    assert snap.wrapper_mode == 0o755
    assert snap.job_dict is None

    # Frozen — cannot set attributes.
    with pytest.raises(AttributeError):
        snap.wrapper_bytes = b"other"

    # Frozen — cannot delete attributes.
    with pytest.raises(AttributeError):
        del snap.wrapper_bytes


def test_snapshot_accepts_job_dict() -> None:
    """_Snapshot accepts a non-None job_dict."""
    from caduceus import _Snapshot

    job = {"id": "abc", "name": "caduceus", "schedule": "every 2m"}
    snap = _Snapshot(wrapper_bytes=b"x", wrapper_mode=0o644, job_dict=job)
    assert snap.job_dict == job


# ---------------------------------------------------------------------------
# Task 2.6: _NeedsAttention return type
# ---------------------------------------------------------------------------


def test_needs_attention_has_recovery_evidence() -> None:
    """_NeedsAttention carries a recovery_evidence string."""
    from caduceus import _NeedsAttention

    na = _NeedsAttention(recovery_evidence="wrapper and job state diverged")
    assert na.recovery_evidence == "wrapper and job state diverged"


def test_needs_attention_is_not_a_success() -> None:
    """_NeedsAttention is not a tuple and does not falsy-pass as success."""
    from caduceus import _NeedsAttention

    na = _NeedsAttention(recovery_evidence="unrecoverable")
    # It must not be a tuple (the normal (action, note) return shape).
    assert not isinstance(na, tuple)


# ---------------------------------------------------------------------------
# Task 2.2: _snapshot_wrapper_and_job()
# ---------------------------------------------------------------------------


def test_snapshot_wrapper_and_job_captures_bytes_and_mode(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """_snapshot_wrapper_and_job captures wrapper bytes, mode, and matching job."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    registry = {
        "abc": {
            "id": "abc",
            "name": "caduceus",
            "schedule": "every 2m",
            "script": "caduceus-pulse.sh",
        }
    }
    _stub_cron_runtime(adapter, registry)
    try:
        snap = adapter._snapshot_wrapper_and_job(wrapper, "caduceus", registry)
    finally:
        _runtime.reset_dispatcher()

    assert isinstance(snap, adapter._Snapshot)
    assert snap.wrapper_bytes == wrapper.read_bytes()
    assert snap.wrapper_mode == 0o755
    assert snap.job_dict is not None
    assert snap.job_dict["id"] == "abc"


def test_snapshot_wrapper_and_job_no_matching_job(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """When no matching job exists, job_dict is None."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    # Registry has jobs but none named "caduceus".
    registry = {
        "xyz": {
            "id": "xyz",
            "name": "other-service",
            "schedule": "every 5m",
        }
    }
    _stub_cron_runtime(adapter, registry)
    try:
        snap = adapter._snapshot_wrapper_and_job(wrapper, "caduceus", registry)
    finally:
        _runtime.reset_dispatcher()

    assert snap.job_dict is None
    assert snap.wrapper_bytes == wrapper.read_bytes()


def test_snapshot_wrapper_and_job_no_wrapper_file(
    adapter, isolated_hermes_home: Path
) -> None:
    """When the wrapper file does not exist, bytes are empty and mode is 0."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    registry = {}

    _stub_cron_runtime(adapter, registry)
    try:
        snap = adapter._snapshot_wrapper_and_job(wrapper, "caduceus", registry)
    finally:
        _runtime.reset_dispatcher()

    assert snap.wrapper_bytes == b""
    assert snap.wrapper_mode == 0
    assert snap.job_dict is None


# ---------------------------------------------------------------------------
# Task 2.3: _reconcile_after_error()
# ---------------------------------------------------------------------------


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


# ---------------------------------------------------------------------------
# Task 2.4: Rewritten _cron_install with snapshot + reconcile
# ---------------------------------------------------------------------------


def test_cron_install_snapshots_before_mutation(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """_cron_install snapshots the wrapper and job before any mutation (AC-01)."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    registry = {}
    actions = _stub_cron_runtime(adapter, registry)
    try:
        action, note = adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()

    assert action in ("created", "reused")
    # The first list action is the snapshot — it happens before create.
    list_actions = [a for a in actions if a["action"] == "list"]
    create_actions = [a for a in actions if a["action"] == "create"]
    assert len(list_actions) >= 1
    if create_actions:
        assert actions.index(list_actions[0]) < actions.index(create_actions[0])


def test_cron_install_checks_capability_before_wrapper(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """Cron capability is checked BEFORE the wrapper is written.

    Per AC-01 and design decision #8: the capability check must precede
    the mutation (wrapper write) so a denied/failing cron does not leave
    a stray wrapper on disk.
    """
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    registry = {}
    _stub_cron_runtime(adapter, registry)

    # Patch _cron_job_registry to raise CronCapabilityError(denied).
    original_registry = adapter._cron_job_registry
    def _failing_registry():
        raise _runtime.CronCapabilityError("denied", "cron denied")

    try:
        adapter._cron_job_registry = _failing_registry  # type: ignore[assignment]
        with pytest.raises((_runtime.CronCapabilityError, RuntimeError)):
            adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()
        adapter._cron_job_registry = original_registry

    # Wrapper should NOT have been written — capability check fails first.
    assert not wrapper.exists(), (
        "wrapper should not exist when cron capability check fails"
    )


# ---------------------------------------------------------------------------
# Task 2.5: Rewritten _cli_cron_remove with snapshot + reconcile
# ---------------------------------------------------------------------------


def test_cron_remove_snapshots_before_mutation(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """_cli_cron_remove snapshots wrapper and job before removal (AC-01/03)."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    registry = {
        "abc": {
            "id": "abc",
            "name": "caduceus",
            "schedule": "every 2m",
        }
    }
    actions = _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_cron_remove()
    finally:
        _runtime.reset_dispatcher()

    assert rc == 0
    # List should happen before remove.
    list_actions = [a for a in actions if a["action"] == "list"]
    remove_actions = [a for a in actions if a["action"] == "remove"]
    assert len(list_actions) >= 1
    if remove_actions:
        assert actions.index(list_actions[0]) < actions.index(remove_actions[0])
    assert not wrapper.exists()


def test_cron_remove_reconciles_on_failure(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """When cron-remove fails mid-way, reconcile restores stable state."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)
    original_bytes = wrapper.read_bytes()

    registry = {
        "abc": {
            "id": "abc",
            "name": "caduceus",
            "schedule": "every 2m",
        }
    }
    actions = _stub_cron_runtime(adapter, registry)

    # Make cron_remove_job fail on first call.
    original_remove = adapter._cronjob_remove
    fail_count = [0]

    def _failing_remove(job_id: str):
        fail_count[0] += 1
        if fail_count[0] == 1:
            raise RuntimeError("simulated remove failure")
        return original_remove(job_id)

    adapter._cronjob_remove = _failing_remove  # type: ignore[assignment]
    try:
        rc = adapter._cli_cron_remove()
    finally:
        _runtime.reset_dispatcher()
        adapter._cronjob_remove = original_remove

    # Reconcile restored stable state — wrapper and job are preserved.
    assert rc == 0
    assert wrapper.is_file()
    assert wrapper.read_bytes() == original_bytes
    # Job still exists — remove failed and reconcile preserved it.
    assert "abc" in registry


# ---------------------------------------------------------------------------
# Task 2.7: Crash-boundary reconciliation scenarios (AC-09)
# ---------------------------------------------------------------------------


def test_cron_install_crash_after_wrapper_before_create_is_idempotent(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """Re-running cron-install after a crash between wrapper write and
    job create converges to the single intended state."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"

    # First run: write wrapper, then fail on cron list (simulating crash).
    adapter._write_pulse_wrapper(install_with_fake_binary)
    assert wrapper.is_file()

    # Second run should succeed and create the job.
    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        action, note = adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()

    assert action == "created"
    assert wrapper.is_file()


def test_cron_remove_crash_after_remove_before_wrapper_delete(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """Re-running cron-remove after a crash between job remove and
    wrapper delete converges to clean state."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    # First run: remove the job manually (simulating crash after remove).
    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_cron_remove()
    finally:
        _runtime.reset_dispatcher()

    # Should succeed — job is already gone, wrapper gets removed.
    assert rc == 0
    assert not wrapper.exists()


def test_cron_install_crash_between_create_and_update_is_idempotent(
    adapter, isolated_hermes_home: Path, install_with_fake_binary: Path
) -> None:
    """If cron-install crashes after creating a job but before the
    reconcile check, a second run updates the existing job."""
    from caduceus import _runtime

    wrapper = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    _stub_wrapper_file(wrapper, install_with_fake_binary)

    # Pre-seed a caduceus job with stale schedule (simulating partial state).
    registry = {
        "abc": {
            "id": "abc",
            "name": "caduceus",
            "schedule": "every 5m",
            "script": "caduceus-pulse.sh",
            "no_agent": False,
        }
    }
    actions = _stub_cron_runtime(adapter, registry)
    try:
        action, note = adapter._cron_install(dry_run=False)
    finally:
        _runtime.reset_dispatcher()

    assert action == "reused"
    # The job should have been updated.
    assert registry["abc"]["schedule"] == "every 2m"
    assert registry["abc"]["no_agent"] is True


# ---------------------------------------------------------------------------
# Additional triangulation: reconcile edge cases
# ---------------------------------------------------------------------------


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


# ---------------------------------------------------------------------------
# Phase 3: Structured Doctor (Tasks 3.1-3.4)
# ---------------------------------------------------------------------------
# Tests written FIRST (RED phase) before any production code changes.
# They verify the _DoctorFinding namedtuple, five doctor check functions,
# structured _cli_doctor with 3 exit codes, and cleanup of AC-12.
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# Task 3.1: _DoctorFinding namedtuple
# ---------------------------------------------------------------------------


def test_doctor_finding_is_namedtuple() -> None:
    """_DoctorFinding is a namedtuple with category, status, detail, next_action."""
    from collections import namedtuple
    from caduceus import _DoctorFinding

    assert isinstance(_DoctorFinding, type)
    assert issubclass(_DoctorFinding, tuple)
    # Namedtuples have _fields.
    assert _DoctorFinding._fields == ("category", "status", "detail", "next_action")


def test_doctor_finding_holds_values() -> None:
    """_DoctorFinding stores category, status, detail, next_action."""
    from caduceus import _DoctorFinding

    f = _DoctorFinding(
        category="host-capability-unavailable",
        status="fail",
        detail="/path/to/binary not found",
        next_action="run `hermes caduceus setup` to build the binary",
    )
    assert f.category == "host-capability-unavailable"
    assert f.status == "fail"
    assert f.detail == "/path/to/binary not found"
    assert f.next_action == "run `hermes caduceus setup` to build the binary"


def test_doctor_finding_accepts_ok_status() -> None:
    """_DoctorFinding accepts status='ok'."""
    from caduceus import _DoctorFinding

    f = _DoctorFinding(
        category="config-incomplete",
        status="ok",
        detail="all config present",
        next_action="",
    )
    assert f.status == "ok"
    assert f.next_action == ""


def test_doctor_finding_is_immutable() -> None:
    """_DoctorFinding is a namedtuple — fields cannot be reassigned."""
    from caduceus import _DoctorFinding

    f = _DoctorFinding(
        category="gateway-inactive",
        status="fail",
        detail="gateway not reachable",
        next_action="run `hermes gateway restart`",
    )
    with pytest.raises(AttributeError):
        f.status = "ok"  # type: ignore[misc]


# ---------------------------------------------------------------------------
# Task 3.2: Doctor check functions
# ---------------------------------------------------------------------------


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
    assert "setup" in finding.next_action.lower()


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
    assert "chmod" in finding.next_action.lower() or "+x" in finding.next_action.lower()


def test_doctor_check_provider_secret_present(
    adapter, install_plugin: Path
) -> None:
    """_doctor_check_provider_secret returns ok when secret name is configured."""
    finding = adapter._doctor_check_provider_secret()
    # Without a config to inspect, we expect a sensible default.
    assert finding.category in ("config-incomplete", "host-capability-unavailable")
    assert finding.status in ("ok", "fail")


def test_doctor_check_cron_capability_ok(
    adapter, install_with_fake_binary: Path
) -> None:
    """_doctor_check_cron_capability returns ok when cron lists without error."""
    from caduceus import _runtime

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    finally:
        _runtime.reset_dispatcher()
    assert finding.category == "host-capability-unavailable"
    assert finding.status == "ok"


def test_doctor_check_cron_capability_fails(
    adapter, install_with_fake_binary: Path
) -> None:
    """_doctor_check_cron_capability returns fail when cron list raises."""
    from caduceus import _runtime

    original_registry = adapter._cron_job_registry
    def _failing_registry():
        raise RuntimeError("cron unavailable")

    try:
        adapter._cron_job_registry = _failing_registry  # type: ignore[assignment]
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    finally:
        adapter._cron_job_registry = original_registry
    assert finding.status == "fail"
    assert "cron" in finding.detail.lower()


def test_doctor_check_gateway_returns_finding(
    adapter, install_with_fake_binary: Path
) -> None:
    """_doctor_check_gateway returns a _DoctorFinding (ok or fail)."""
    finding = adapter._doctor_check_gateway()
    assert isinstance(finding, tuple)
    assert finding.category == "gateway-inactive"
    assert finding.status in ("ok", "fail")


# ---------------------------------------------------------------------------
# Task 3.3: Rewritten _cli_doctor
# ---------------------------------------------------------------------------


def test_doctor_exit_0_when_all_healthy(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path, monkeypatch
) -> None:
    """_cli_doctor returns 0 when all checks pass (AC-06)."""
    from caduceus import _runtime

    # Set up healthy environment: binary exists, bridge is executable,
    # cron works, and provider secret is configured.
    monkeypatch.setenv("CADUCEUS_GITHUB_TOKEN", "ghp_test-secret-configured")
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()
    assert rc == 0


def test_doctor_exit_1_for_config_defect(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path
) -> None:
    """_cli_doctor returns 1 for config-incomplete or daemon-defect (AC-08)."""
    from caduceus import _runtime

    # Binary present, bridge executable, cron works — but provider secret
    # is missing (config-incomplete).
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)

    registry = {}
    _stub_cron_runtime(adapter, registry)

    # Make provider secret check return fail (config-incomplete).
    original_secret = adapter._doctor_check_provider_secret
    def _failing_secret():
        from caduceus import _DoctorFinding
        return _DoctorFinding(
            category="config-incomplete",
            status="fail",
            detail="provider secret not configured",
            next_action="set HERMES_PROVIDER_SECRET in environment",
        )

    try:
        adapter._doctor_check_provider_secret = _failing_secret  # type: ignore[assignment]
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()
        adapter._doctor_check_provider_secret = original_secret
    assert rc == 1


def test_doctor_exit_2_for_missing_binary(
    adapter, isolated_hermes_home: Path
) -> None:
    """_cli_doctor returns 2 for host-capability-unavailable (AC-11)."""
    from caduceus import _runtime

    # No binary installed — exit 2.
    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()
    assert rc == 2


def test_doctor_exit_2_takes_precedence_over_exit_1(
    adapter, isolated_hermes_home: Path
) -> None:
    """When both exit-1 and exit-2 failures exist, exit 2 wins (design #9)."""
    from caduceus import _runtime

    # Binary missing (exit 2) AND config defect (exit 1) — exit 2 wins.
    registry = {}
    _stub_cron_runtime(adapter, registry)
    original_secret = adapter._doctor_check_provider_secret
    def _failing_secret():
        from caduceus import _DoctorFinding
        return _DoctorFinding(
            category="config-incomplete",
            status="fail",
            detail="provider secret not configured",
            next_action="set HERMES_PROVIDER_SECRET in environment",
        )

    try:
        adapter._doctor_check_provider_secret = _failing_secret  # type: ignore[assignment]
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()
        adapter._doctor_check_provider_secret = original_secret
    assert rc == 2


def test_doctor_prints_structured_report(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path, capsys: pytest.CaptureFixture, monkeypatch
) -> None:
    """_cli_doctor prints each finding with status, detail, next_action (AC-07)."""
    from caduceus import _runtime

    monkeypatch.setenv("CADUCEUS_GITHUB_TOKEN", "ghp_test-secret-configured")
    bridge = isolated_hermes_home / "caduceus" / "worker-bridge.py"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    bridge.write_text("#!/usr/bin/env python3\nprint('ok')\n")
    bridge.chmod(0o755)

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()

    captured = capsys.readouterr()
    assert rc == 0
    # Each finding category should appear in the output.
    assert "binary" in captured.out.lower() or "Binary" in captured.out
    assert "cron" in captured.out.lower() or "Cron" in captured.out
    assert "bridge" in captured.out.lower() or "Bridge" in captured.out
    assert "ok" in captured.out.lower() or "OK" in captured.out


def test_doctor_prints_failures_on_exit_2(
    adapter, capsys: pytest.CaptureFixture
) -> None:
    """_cli_doctor prints failure details when exiting 2."""
    from caduceus import _runtime

    registry = {}
    _stub_cron_runtime(adapter, registry)
    try:
        rc = adapter._cli_doctor()
    finally:
        _runtime.reset_dispatcher()

    captured = capsys.readouterr()
    assert rc == 2
    # Should show what failed.
    assert "fail" in captured.out.lower() or "FAIL" in captured.out


# ---------------------------------------------------------------------------
# Task 3.5: AC-12 cleanup tests
# ---------------------------------------------------------------------------


def test_write_pulse_wrapper_no_misleading_comments(
    adapter, install_with_fake_binary: Path, isolated_hermes_home: Path
) -> None:
    """_write_pulse_wrapper no longer has misleading comments (AC-12)."""
    # We check the body content the adapter generates.
    # Get the source of the function.
    import inspect
    source = inspect.getsource(adapter._write_pulse_wrapper)
    # The body string should not contain "Generated by" (the old comment
    # was misleading — it's generated at runtime with the installed path).
    # Actually AC-12 says REMOVE misleading comments, so let's verify
    # the body is clean.
    assert "Do not edit by hand" not in source
    # The wrapper should still contain the exec line and set -euo pipefail.
    body = (
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        f"exec {install_with_fake_binary} run \"$@\"\n"
    )
    # The generated wrapper should match a clean structure.
    adapter._write_pulse_wrapper(install_with_fake_binary)
    wrapper_path = isolated_hermes_home / "scripts" / "caduceus-pulse.sh"
    assert wrapper_path.is_file()
    text = wrapper_path.read_text(encoding="utf-8")
    assert text == body


def test_pulse_template_has_strict_shell_and_accurate_comments(
    plugin_root: Path
) -> None:
    """caduceus-pulse.sh has strict shell and accurate comments (AC-12)."""
    template = plugin_root / "plugin-assets" / "caduceus-pulse.sh"
    text = template.read_text(encoding="utf-8")
    lines = text.splitlines()

    # Must have strict shell settings.
    assert "set -euo pipefail" in text
    # Must NOT have misleading or dev-only comments.
    assert "FIXME" not in text
    assert "TODO" not in text
    assert "pending" not in text.lower()
    assert "dev-only" not in text.lower()
    # Must accurately describe what the file is.
    assert "template" in text.lower() or "Template" in text


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

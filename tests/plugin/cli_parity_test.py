"""Drift guard: the wrapper must cover the binary's full command set.

Issue #389: the wrapper registered only a subset of the binary's clap
subcommands, so operators hit argparse ``invalid choice`` errors. This
guard parses the clap ``Command`` enum straight from ``src/cli/mod.rs``
(hermetic: the CI python job has no Rust toolchain, so shelling out to
a built binary would silently never run) and asserts the wrapper's
argparse subcommand tree covers it exactly. A future binary subcommand
fails HERE instead of breaking operators at runtime.
"""

from __future__ import annotations

import argparse
import re
from pathlib import Path

from tests.fixtures.fake_ctx import FakePluginContext

# Subcommands that exist only in the Hermes wrapper, never in the
# binary. The coverage test ignores them on the wrapper side.
WRAPPER_ONLY = {"cron-install", "cron-remove"}

# Binary subcommands the wrapper intercepts with its own rich
# implementation instead of pass-through. Kept explicit so a variant
# moving between the two classes requires touching this file — the
# passthrough-set test fails otherwise.
INTERCEPTED = {"setup", "doctor", "status"}


def _binary_commands(repo_root: Path) -> set:
    """Return the binary's subcommand names from the clap source.

    clap's derive converts variant names to kebab-case subcommand
    names (``WorktreeGc`` -> ``worktree-gc``). The clap-internal
    ``help`` subcommand is deliberately not part of the enum and not
    part of the parity set: the wrapper's argparse provides its own
    ``--help`` surface.
    """
    src = (repo_root / "src" / "cli" / "mod.rs").read_text(encoding="utf-8")
    match = re.search(r"pub enum Command \{(.*?)\n\}", src, re.DOTALL)
    assert match is not None, "pub enum Command block not found in src/cli/mod.rs"
    variants = re.findall(r"^    ([A-Z][A-Za-z0-9]*)\b", match.group(1), re.MULTILINE)
    assert variants, "no Command variants parsed from src/cli/mod.rs"
    return {re.sub(r"(?<!^)(?=[A-Z])", "-", v).lower() for v in variants}


def _subparsers_action(adapter, fake_ctx: FakePluginContext):
    """Register the CLI and return the argparse subparsers action."""
    adapter.register(fake_ctx)
    parser = fake_ctx.cli_commands["caduceus"].parser
    for action in parser._actions:
        if isinstance(action, argparse._SubParsersAction):
            return action
    raise AssertionError("caduceus parser has no subparsers action")


def test_wrapper_covers_full_binary_command_set(
    adapter, fake_ctx: FakePluginContext, repo_root: Path
) -> None:
    """Every binary subcommand is registered by the wrapper (issue #389)."""
    binary = _binary_commands(repo_root)
    wrapper = set(_subparsers_action(adapter, fake_ctx).choices)
    missing = binary - wrapper
    extra = wrapper - binary - WRAPPER_ONLY
    assert not missing, (
        f"hermes caduceus is missing binary subcommands: {sorted(missing)}. "
        "Add a parser for it in _register_caduceus_cli (__init__.py) "
        "so operators do not hit argparse 'invalid choice'."
    )
    assert not extra, (
        f"hermes caduceus registers unknown subcommands: {sorted(extra)}. "
        "Binary commands must map 1:1; cron-install/cron-remove are the "
        "only wrapper-owned additions."
    )


def test_passthrough_set_equals_binary_minus_intercepted(
    adapter, fake_ctx: FakePluginContext, repo_root: Path
) -> None:
    """Every non-intercepted binary command is a REMAINDER passthrough.

    The guard compares ONLY the passthrough set: wrapper-owned
    (cron-*) and intercepted (setup/doctor/status) subcommands are
    excluded here and pinned by WRAPPER_ONLY / INTERCEPTED above.
    """
    subs = _subparsers_action(adapter, fake_ctx)
    passthrough = {
        name
        for name, sub in subs.choices.items()
        if isinstance(sub, adapter._PassthroughParser) and sub._passthrough_attr
    }
    expected = _binary_commands(repo_root) - INTERCEPTED
    assert passthrough == expected, (
        f"passthrough set drifted: missing={sorted(expected - passthrough)} "
        f"unexpected={sorted(passthrough - expected)}"
    )
"""Stand-in for Hermes's ``PluginContext`` used by the adapter tests.

The real context exposes ``register_skill``, ``register_command``,
``register_cli_command``, ``register_hook``, ``register_tool``,
``dispatch_tool``, ``manifest``, etc. The tests cover the surfaces
Caduceus uses; they do not pretend to be a Hermes loader.
"""

from __future__ import annotations

import inspect
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional


@dataclass
class _SkillRecord:
    path: Path
    description: str = ""


@dataclass
class _CommandRecord:
    name: str
    handler: Callable
    description: str
    args_hint: str = ""


@dataclass
class _CliCommandRecord:
    name: str
    help: str
    description: str
    setup_fn: Callable
    handler_fn: Optional[Callable]
    parser: Any = None


@dataclass
class FakePluginContext:
    name: str
    version: str = ""
    description: str = ""
    author: str = ""
    kind: str = "standalone"
    manifest: Dict[str, Any] = field(default_factory=dict)

    skills: Dict[str, _SkillRecord] = field(default_factory=dict)
    commands: Dict[str, _CommandRecord] = field(default_factory=dict)
    cli_commands: Dict[str, _CliCommandRecord] = field(default_factory=dict)
    dispatch_calls: List[Dict[str, Any]] = field(default_factory=list)
    timer_changes: Dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not self.manifest:
            self.manifest = {
                "name": self.name,
                "version": self.version,
                "description": self.description,
                "author": self.author,
                "kind": self.kind,
            }

    # -- registration surface ------------------------------------------------

    def register_skill(self, name: str, path: Path, description: str = "") -> None:
        self.skills[name] = _SkillRecord(path=Path(path), description=description)

    def register_command(
        self,
        name: str,
        handler: Callable,
        description: str = "",
        args_hint: str = "",
    ) -> None:
        self.commands[name] = _CommandRecord(
            name=name,
            handler=handler,
            description=description,
            args_hint=args_hint,
        )

    def register_cli_command(
        self,
        name: str,
        help: str,
        setup_fn: Callable,
        handler_fn: Optional[Callable] = None,
        description: str = "",
    ) -> None:
        record = _CliCommandRecord(
            name=name,
            help=help,
            description=description,
            setup_fn=setup_fn,
            handler_fn=handler_fn,
        )
        self.cli_commands[name] = record
        # Mimic Hermes: build an argparse subparser so the CLI can be
        # inspected programmatically.
        try:
            import argparse

            parser = argparse.ArgumentParser(prog=name)
            setup_fn(parser)
            record.parser = parser
        except Exception as exc:  # pragma: no cover - defensive
            raise AssertionError(f"CLI setup_fn raised: {exc}") from exc

    # -- dispatcher ----------------------------------------------------------

    def dispatch_tool(self, name: str, args: Dict[str, Any]) -> Any:
        self.dispatch_calls.append({"name": name, "args": dict(args)})
        return None

    # -- cron capability install ---------------------------------------------

    def install_cron_capability(self, category: str) -> None:
        """Install a cron dispatcher callable for the given *category*.

        Categories: well_formed, malformed, denied, timed_out, eof, crashed,
        duplicate, foreign_name_collision, absent.  Uses
        ``_runtime.install_dispatcher()`` — does NOT modify ``_runtime.py``.
        The caller is responsible for calling ``_runtime.reset_dispatcher()``
        after the test.
        """
        from caduceus import _runtime
        from tests.fixtures.capability_simulator import get_simulator

        fn = get_simulator(category)
        _runtime.install_dispatcher(fn)

    # -- inspection helpers --------------------------------------------------

    def parse_cli(self, argv: List[str]) -> Any:
        """Parse *argv* against the registered CLI subcommand parser.

        Returns the populated ``argparse.Namespace``. The CLI subparser
        tree built by the adapter must accept the canonical ``hermes
        caduceus <subcommand>`` invocation shape.
        """
        if not self.cli_commands:
            raise AssertionError("no CLI commands registered")
        record = next(iter(self.cli_commands.values()))
        if record.parser is None:
            raise AssertionError("CLI parser not built")
        return record.parser.parse_args(argv)


def assert_skill_registered(ctx: FakePluginContext, name: str) -> _SkillRecord:
    assert name in ctx.skills, f"skill {name!r} was not registered; saw {sorted(ctx.skills)}"
    return ctx.skills[name]


def assert_command_registered(ctx: FakePluginContext, name: str) -> _CommandRecord:
    assert name in ctx.commands, f"command {name!r} was not registered; saw {sorted(ctx.commands)}"
    return ctx.commands[name]


def assert_cli_command_registered(ctx: FakePluginContext, name: str):
    assert name in ctx.cli_commands, f"CLI command {name!r} was not registered; saw {sorted(ctx.cli_commands)}"
    return ctx.cli_commands[name]  # for type-checker

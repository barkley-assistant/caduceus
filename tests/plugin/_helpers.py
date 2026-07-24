"""Shared helpers for tests/plugin/."""

from __future__ import annotations

import os
import subprocess
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Callable, Dict, Iterator, List, Union


def _read_plugin_yaml(installed: Path) -> Dict[str, Any]:
    import yaml

    text = (installed / "plugin.yaml").read_text(encoding="utf-8")
    return yaml.safe_load(text) or {}



def _invoke_cli(adapter: Any, *argv: str) -> Any:
    """Run the adapter's CLI command at the function boundary."""
    ctx = FakePluginContext(name="caduceus")
    adapter.register(ctx)
    parser = ctx.cli_commands["caduceus"].parser
    args = parser.parse_args(list(argv))
    return args.func(args)



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



# Header emitted by ``hermes cron list``. Keep widths elastic so parser
# tests stay focused on structure rather than exact banner length.
_HERMES_CRON_LIST_HEADER = (
    "\u250c\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2510\n"
    "\u2502 Scheduled Jobs \u2502\n"
    "\u2514\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2518\n"
)


def _registry_to_table(registry: Dict[str, Dict[str, Any]]) -> str:
    """Render a fake ``hermes cron list --all`` table from *registry*."""
    lines = [_HERMES_CRON_LIST_HEADER]
    for job_id, job in registry.items():
        status = job.get("status", "active")
        lines.append(f"  {job_id} [{status}]")
        name = job.get("name", "")
        lines.append(f"    Name:      {name}")
        if "schedule" in job:
            lines.append(f"    Schedule:  {job['schedule']}")
        if "script" in job:
            lines.append(f"    Script:    {job['script']}")
        if "workdir" in job:
            lines.append(f"    Workdir:   {job['workdir']}")
        if "no_agent" in job:
            mode = "no-agent" if job["no_agent"] else "agent"
            lines.append(f"    Mode:      {mode}")
        lines.append("")
    return "\n".join(lines)


def _fake_subprocess_run(
    registry: Dict[str, Dict[str, Any]],
    next_id: List[int],
    seen_actions: List[Dict[str, Any]],
) -> Callable[..., subprocess.CompletedProcess[str]]:
    """Return a ``_subprocess_run`` replacement backed by *registry*."""

    def fake_run(argv: list, **kwargs: Any) -> subprocess.CompletedProcess[str]:
        op = argv[2] if len(argv) > 2 else "unknown"
        seen_actions.append({"action": op, "argv": list(argv)})
        if op == "list":
            return subprocess.CompletedProcess(argv, 0, _registry_to_table(registry), "")
        if op == "create":
            schedule = argv[3]
            name = argv[argv.index("--name") + 1]
            script = argv[argv.index("--script") + 1]
            no_agent = "--no-agent" in argv
            # Use hex ids so the table parser's block regex accepts them.
            job_id = f"{next_id[0]:012x}"
            next_id[0] += 1
            registry[job_id] = {
                "id": job_id,
                "name": name,
                "schedule": schedule,
                "script": script,
                "no_agent": no_agent,
            }
            return subprocess.CompletedProcess(argv, 0, f"Created cron job {job_id}\n", "")
        if op == "remove":
            job_id = argv[3]
            registry.pop(job_id, None)
            return subprocess.CompletedProcess(argv, 0, "", "")
        return subprocess.CompletedProcess(argv, 0, "", "")

    return fake_run


@contextmanager
def subprocess_run_recorder(
    scripted: Dict[str, Union[str, Exception, Callable[..., Any]]]
) -> Iterator[List[List[str]]]:
    """Monkeypatch ``caduceus._runtime._subprocess_run`` for a single test.

    *scripted* maps operation names (``list``, ``create``, ``remove``) to
    either:

    * a stdout string (used for a successful ``CompletedProcess``),
    * an ``Exception`` subclass instance to be raised, or
    * a callable ``fn(argv, kwargs) -> CompletedProcess`` for full control.

    Yields the list of argv lists passed to the fake runner. Hermes path
    resolution is short-circuited so tests do not require ``hermes`` on PATH.
    """
    from caduceus import _runtime

    calls: List[List[str]] = []
    saved_run = _runtime._subprocess_run
    saved_path = _runtime._HERMES_PATH

    def fake_run(argv: list, **kwargs: Any) -> subprocess.CompletedProcess[str]:
        calls.append(list(argv))
        op = argv[2] if len(argv) > 2 else "unknown"
        entry = scripted.get(op, scripted.get("default", ""))
        if callable(entry):
            return entry(argv, kwargs)
        if isinstance(entry, Exception):
            raise entry
        return subprocess.CompletedProcess(argv, 0, str(entry), "")

    _runtime._subprocess_run = fake_run
    _runtime._HERMES_PATH = "/usr/bin/hermes"
    try:
        yield calls
    finally:
        _runtime._subprocess_run = saved_run
        _runtime._HERMES_PATH = saved_path


def _stub_cron_runtime(adapter: Any, registry: Dict[str, Dict[str, Any]]) -> List[Dict[str, Any]]:
    """Replace the cron helper with an in-memory registry backed by a recorder.

    The ``adapter`` argument is accepted for backward compatibility but is
    not used; the runtime is patched directly via ``_runtime._subprocess_run``.
    """
    from caduceus import _runtime

    next_id = [1]
    seen_actions: List[Dict[str, Any]] = []
    _runtime._subprocess_run = _fake_subprocess_run(registry, next_id, seen_actions)
    _runtime._HERMES_PATH = "/usr/bin/hermes"
    return seen_actions

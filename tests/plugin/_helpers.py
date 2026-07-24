"""Shared helpers for tests/plugin/."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Any, Dict, List

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

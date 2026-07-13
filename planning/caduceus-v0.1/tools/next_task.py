#!/usr/bin/env python3
"""Return the single next eligible task or phase gate."""

from __future__ import annotations

import argparse
import json
import sys

from validate_plan import PlanError, ROOT, validate


def select_next(manifest: dict, progress: dict) -> dict:
    tasks = manifest["tasks"]
    states = progress["tasks"]
    gates = progress["phase_gates"]

    active_tasks = [t for t in tasks if states[t["id"]]["status"] == "in_progress"]
    if active_tasks:
        task = active_tasks[0]
        return _task_result(task, manifest, progress, resumed=True)

    for phase in manifest["phases"]:
        phase_id = phase["id"]
        gate = gates[str(phase_id)]
        if gate["status"] == "complete":
            continue
        if gate["status"] == "in_progress":
            return _gate_result(phase, resumed=True)

        phase_tasks = [t for t in tasks if t["execution_phase"] == phase_id]
        blocked = [t["id"] for t in phase_tasks if states[t["id"]]["status"] == "blocked"]
        pending = [t for t in phase_tasks if states[t["id"]]["status"] == "pending"]
        eligible = [
            task for task in pending
            if all(states[d]["status"] == "complete" for d in task["depends_on"])
        ]
        if eligible:
            return _task_result(eligible[0], manifest, progress, resumed=False)
        if pending:
            return {
                "kind": "blocked",
                "phase": phase_id,
                "reason": "no pending task has all dependencies complete",
                "pending": [t["id"] for t in pending],
                "blocked": blocked,
            }
        if blocked:
            return {
                "kind": "blocked",
                "phase": phase_id,
                "reason": "phase contains blocked tasks",
                "blocked": blocked,
            }
        return _gate_result(phase, resumed=False)

    return {"kind": "done", "plan_id": manifest["plan_id"]}


def _task_result(task: dict, manifest: dict, progress: dict, resumed: bool) -> dict:
    dependency_handoffs = []
    for dependency in task["depends_on"]:
        handoff = progress["tasks"][dependency].get("handoff")
        if handoff:
            dependency_handoffs.append(handoff)
    phase = manifest["phases"][task["execution_phase"]]
    return {
        "kind": "task",
        "resumed": resumed,
        "id": task["id"],
        "title": task["title"],
        "execution_phase": task["execution_phase"],
        "contracts": str(ROOT / manifest["contracts"]),
        "phase_spec": str(ROOT / phase["spec"]),
        "task_spec": str(ROOT / task["spec"]),
        "dependency_handoffs": [str(ROOT / p) for p in dependency_handoffs],
    }


def _gate_result(phase: dict, resumed: bool) -> dict:
    return {
        "kind": "phase_gate",
        "resumed": resumed,
        "execution_phase": phase["id"],
        "title": phase["title"],
        "phase_spec": str(ROOT / phase["spec"]),
        "handoff": str(ROOT / phase["gate_handoff"]),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--format", choices=("json", "path"), default="path")
    args = parser.parse_args()
    try:
        manifest, progress = validate()
        result = select_next(manifest, progress)
    except PlanError as exc:
        print(f"plan invalid: {exc}", file=sys.stderr)
        return 1
    if args.format == "json":
        print(json.dumps(result, indent=2))
    elif result["kind"] == "task":
        print(result["task_spec"])
    elif result["kind"] == "phase_gate":
        print(result["phase_spec"])
    else:
        print(json.dumps(result))
    return 2 if result["kind"] == "blocked" else 0


if __name__ == "__main__":
    raise SystemExit(main())

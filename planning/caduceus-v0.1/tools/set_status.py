#!/usr/bin/env python3
"""Guarded, atomic progress transitions for one-task agent loops."""

from __future__ import annotations

import argparse
import datetime as dt
import fcntl
import json
import os
import sys
from pathlib import Path

from next_task import select_next
from validate_plan import PlanError, ROOT, validate


def _save(path: Path, value: dict) -> None:
    tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    with tmp.open("w", encoding="utf-8") as handle:
        json.dump(value, handle, indent=2)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(tmp, path)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("work_item", help="task ID such as 2.3 or gate such as phase-02")
    parser.add_argument("status", choices=("pending", "in_progress", "complete", "blocked"))
    parser.add_argument("--handoff", help="path relative to the plan root")
    args = parser.parse_args()

    lock_path = ROOT / ".progress.lock"
    with lock_path.open("a+", encoding="utf-8") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            manifest, progress = validate()
            selected = select_next(manifest, progress)
            is_gate = args.work_item.startswith("phase-")
            if is_gate:
                phase_id = str(int(args.work_item.removeprefix("phase-")))
                state = progress["phase_gates"].get(phase_id)
                expected_kind = "phase_gate"
                expected_id = int(phase_id)
            else:
                state = progress["tasks"].get(args.work_item)
                expected_kind = "task"
                expected_id = args.work_item
            if state is None:
                raise PlanError(f"unknown work item {args.work_item}")

            current = state["status"]
            if args.status == "pending":
                if current != "blocked":
                    raise PlanError(f"cannot return {args.work_item} to pending from {current}")
                state["handoff"] = None
            elif args.status == "in_progress":
                if current not in {"pending", "blocked", "in_progress"}:
                    raise PlanError(f"cannot start {args.work_item} from {current}")
                if selected.get("kind") != expected_kind:
                    raise PlanError(f"next work item is {selected}")
                selected_id = selected.get("execution_phase") if is_gate else selected.get("id")
                if selected_id != expected_id:
                    raise PlanError(f"next work item is {selected_id}, not {expected_id}")
            elif args.status in {"complete", "blocked"}:
                if current != "in_progress":
                    raise PlanError(f"cannot finish {args.work_item} from {current}")
                if not args.handoff:
                    raise PlanError("complete/blocked transitions require --handoff")
                handoff = ROOT / args.handoff
                if not handoff.is_file():
                    raise PlanError(f"handoff does not exist: {handoff}")
                if args.status == "complete" and not is_gate:
                    task = next(task for task in manifest["tasks"] if task["id"] == args.work_item)
                    review = task.get("human_review")
                    if review and review.get("required"):
                        review_path = ROOT / str(review.get("handoff", ""))
                        if not review_path.is_file():
                            raise PlanError(
                                f"task {args.work_item} requires human-review artifact {review_path}"
                            )

            state["status"] = args.status
            state["updated_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
            if args.handoff:
                state["handoff"] = args.handoff
            _save(ROOT / "progress.json", progress)
        except PlanError as exc:
            print(f"status unchanged: {exc}", file=sys.stderr)
            return 1
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)
    print(f"{args.work_item}: {args.status}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

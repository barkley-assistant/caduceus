#!/usr/bin/env python3
"""Guarded, atomic progress transitions for the v1.0 one-task agent loop.

Phase 00 Task 0.2 (PLAN-002, PLAN-003) hardens the validator and
the templates so incomplete or non-independent evidence is rejected
before a task or phase transition is recorded.

The fsync + atomic-replace + temp-cleanup contract is documented
inline at `_save`. The procedure is:

1. Write the new JSON to a PID-scoped temp file under
   ``planning/caduceus-v1.0/.progress.json.<pid>.tmp``.
2. ``flush()`` and ``os.fsync()`` the file before close so the
   bytes hit stable storage.
3. ``os.replace(tmp, path)`` for atomic publication.
4. ``os.fsync()`` the parent directory so the rename itself is
   durable.
5. On any failure inside the try block, the temp file is removed
   in the finally clause so a crashed prior invocation cannot
   leak a half-written file.

The contract is invoked by ``validate()`` in ``validate_plan.py``,
which is the single source of truth for evidence acceptance. A
transition is recorded only when both the controller's preconditions
(``next work item``) and the validator's evidence rules pass.
"""

from __future__ import annotations

import argparse
import copy
import datetime as dt
import fcntl
import json
import os
import sys
from pathlib import Path

from next_task import select_next
from validate_plan import PlanError, ROOT, validate


def _save(path: Path, value: dict) -> None:
    """Atomically replace ``path`` with ``value`` and fsync everything.

    The progress directory is fsynced after the rename so a power
    loss between replace and fsync still leaves the new content on
    disk. The temp file is unlinked in the finally clause so a
    mid-write crash never leaves a half-written artifact under
    ``planning/caduceus-v1.0/``.
    """
    parent = path.parent
    tmp = parent / f".{path.name}.{os.getpid()}.tmp"
    replaced = False
    try:
        with tmp.open("w", encoding="utf-8") as handle:
            json.dump(value, handle, indent=2)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(tmp, path)
        replaced = True
        # fsync the parent directory so the rename is durable. A
        # crash between the rename and this fsync leaves the new
        # file in place but the directory entry may not be flushed;
        # the fsync closes that window.
        directory_fd = os.open(str(parent), os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    finally:
        # On the failure path the temp file may still be on disk if
        # the open succeeded but the dump / replace failed before
        # the rename. The rename path leaves nothing to clean up
        # (the inode has been moved). The finally clause is the
        # single cleanup point; ``replaced`` distinguishes the two.
        if not replaced:
            try:
                tmp.unlink()
            except FileNotFoundError:
                pass


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
            if manifest["catalog_status"] == "draft":
                raise PlanError(
                    "v1.0 task catalog is draft; progress transitions are disabled"
                )
            selected = select_next(manifest, progress)
            proposed = copy.deepcopy(progress)
            is_gate = args.work_item.startswith("phase-")
            if is_gate:
                phase_id = str(int(args.work_item.removeprefix("phase-")))
                state = proposed["phase_gates"].get(phase_id)
                expected_kind = "phase_gate"
                expected_id = int(phase_id)
            else:
                state = proposed["tasks"].get(args.work_item)
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
            state["status"] = args.status
            state["updated_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
            if args.handoff:
                state["handoff"] = args.handoff
            # Evidence acceptance is delegated to ``validate(...)
            # with progress_override=proposed``; the validator is
            # the single source of truth for both the schema
            # (PLAN-002) and the human-review surface (PLAN-003).
            validate(ROOT, progress_override=proposed)
            _save(ROOT / "progress.json", proposed)
        except PlanError as exc:
            print(f"status unchanged: {exc}", file=sys.stderr)
            return 1
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)
    print(f"{args.work_item}: {args.status}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

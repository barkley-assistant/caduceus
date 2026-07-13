#!/usr/bin/env python3
"""Validate the Caduceus v0.1 execution plan and mutable progress ledger."""

from __future__ import annotations

import hashlib
import json
import re
import sys
from pathlib import Path
from urllib.parse import unquote

ROOT = Path(__file__).resolve().parents[1]
ALLOWED_STATUS = {"pending", "in_progress", "complete", "blocked"}


class PlanError(RuntimeError):
    pass


def _load(path: Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise PlanError(f"cannot load {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise PlanError(f"{path} must contain a JSON object")
    return value


def _front_matter(packet: str, task_id: str) -> dict[str, str]:
    match = re.match(r"---\n(.+?)\n---\n", packet, re.DOTALL)
    if not match:
        raise PlanError(f"task {task_id} packet lacks front matter")
    fields: dict[str, str] = {}
    for line in match.group(1).splitlines():
        key, separator, value = line.partition(":")
        if separator:
            fields[key.strip()] = value.strip().strip('"')
    return fields


def _validate_local_links(root: Path, files: list[Path]) -> None:
    link_pattern = re.compile(r"(?<!!)\[[^\]]*\]\(([^)]+)\)")
    for source in files:
        for raw_target in link_pattern.findall(source.read_text(encoding="utf-8")):
            target = raw_target.strip().split("#", 1)[0]
            if not target or "://" in target or target.startswith(("mailto:", "#")):
                continue
            # Canonical plan links do not use optional Markdown link titles.
            target = unquote(target.strip("<>"))
            resolved = (source.parent / target).resolve()
            try:
                resolved.relative_to(root.resolve())
            except ValueError as exc:
                raise PlanError(f"link escapes plan root: {source} -> {raw_target}") from exc
            if not resolved.exists():
                raise PlanError(f"broken local link: {source} -> {raw_target}")


def validate(root: Path = ROOT) -> tuple[dict, dict]:
    manifest = _load(root / "task-manifest.json")
    progress = _load(root / "progress.json")

    if manifest.get("schema_version") != 1 or progress.get("schema_version") != 1:
        raise PlanError("unsupported manifest/progress schema version")
    if manifest.get("plan_id") != progress.get("plan_id"):
        raise PlanError("manifest and progress plan_id differ")

    contracts = root / str(manifest.get("contracts", ""))
    if not contracts.is_file():
        raise PlanError(f"contracts file missing: {contracts}")
    digest = hashlib.sha256(contracts.read_bytes()).hexdigest()
    if digest != manifest.get("contracts_sha256"):
        raise PlanError(
            "CONTRACTS.md drift detected; restore it or obtain an approved revision "
            "recorded in CONTRACT_REVISIONS.md before refreshing the digest "
            f"(expected {manifest.get('contracts_sha256')}, got {digest})"
        )

    revision_log = root / str(manifest.get("contract_revision_log", ""))
    if not revision_log.is_file():
        raise PlanError(f"contract revision log missing: {revision_log}")

    phases = manifest.get("phases")
    tasks = manifest.get("tasks")
    if not isinstance(phases, list) or not isinstance(tasks, list):
        raise PlanError("manifest phases/tasks must be arrays")
    if [p.get("id") for p in phases] != list(range(len(phases))):
        raise PlanError("phase IDs must be contiguous from zero")
    if len(phases) != 10:
        raise PlanError(f"expected 10 phases, found {len(phases)}")
    if len(tasks) != 46:
        raise PlanError(f"expected 46 tasks, found {len(tasks)}")

    by_id: dict[str, dict] = {}
    human_reviews: dict[str, tuple[bool, Path]] = {}
    for task in tasks:
        task_id = task.get("id")
        if not isinstance(task_id, str) or not re.fullmatch(r"[0-9]+\.[0-9]+", task_id):
            raise PlanError(f"invalid task ID: {task_id!r}")
        if task_id in by_id:
            raise PlanError(f"duplicate task ID: {task_id}")
        by_id[task_id] = task
        phase_id = task.get("execution_phase")
        if not isinstance(phase_id, int) or not 0 <= phase_id < len(phases):
            raise PlanError(f"task {task_id} has invalid execution phase")
        spec = root / str(task.get("spec", ""))
        if not spec.is_file():
            raise PlanError(f"task {task_id} spec missing: {spec}")
        packet = spec.read_text(encoding="utf-8")
        fields = _front_matter(packet, task_id)
        if fields.get("task_id") != task_id:
            raise PlanError(f"task {task_id} packet task_id does not match")
        if fields.get("title") != task.get("title"):
            raise PlanError(f"task {task_id} packet title does not match manifest")
        if fields.get("execution_phase") != str(phase_id):
            raise PlanError(f"task {task_id} packet execution phase does not match")
        phase_file = Path(fields.get("phase_file", ""))
        expected_phase_file = Path("../") / str(phases[phase_id].get("spec", ""))
        if phase_file != expected_phase_file:
            raise PlanError(f"task {task_id} packet phase_file does not match manifest")
        if f"# Task {task_id}: {task.get('title')}" not in packet:
            raise PlanError(f"task {task_id} packet heading does not match manifest")
        match = re.search(
            r"## Outcome and required behavior\n(.+?)\n## Execution boundaries",
            packet,
            re.DOTALL,
        )
        if not match or len(match.group(1).strip()) < 20:
            raise PlanError(f"task {task_id} has no substantive behavior contract")

        review = task.get("human_review")
        if review is not None:
            if not isinstance(review, dict) or not isinstance(review.get("required"), bool):
                raise PlanError(f"task {task_id} has invalid human_review metadata")
            handoff_value = review.get("handoff")
            if not isinstance(handoff_value, str) or not handoff_value:
                raise PlanError(f"task {task_id} human_review needs a handoff path")
            review_path = (root / handoff_value).resolve()
            try:
                review_path.relative_to(root.resolve())
            except ValueError as exc:
                raise PlanError(f"task {task_id} human-review path escapes plan root") from exc
            if review_path.parent != (root / "handoffs").resolve():
                raise PlanError(f"task {task_id} human-review artifact must be under handoffs/")
            human_reviews[task_id] = (review["required"], review_path)

    phase_files: list[Path] = []
    for phase in phases:
        phase_id = phase.get("id")
        if not all(isinstance(phase.get(field), str) and phase.get(field) for field in ("slug", "title", "spec", "gate_handoff")):
            raise PlanError(f"phase {phase_id} has incomplete metadata")
        spec = root / str(phase.get("spec", ""))
        if not spec.is_file():
            raise PlanError(f"phase {phase_id} spec missing: {spec}")
        phase_files.append(spec)
        phase_text = spec.read_text(encoding="utf-8")
        if not phase_text.startswith(f"# Phase {phase_id:02d}: {phase['title']}\n"):
            raise PlanError(f"phase {phase_id} heading does not match manifest")
        expected_task_ids = [
            task["id"] for task in tasks if task["execution_phase"] == phase_id
        ]
        if not expected_task_ids:
            raise PlanError(f"phase {phase_id} contains no tasks")
        listed_task_ids = re.findall(r"^- \[Task ([0-9]+\.[0-9]+):", phase_text, re.MULTILINE)
        if len(listed_task_ids) != len(set(listed_task_ids)):
            raise PlanError(f"phase {phase_id} lists a task more than once")
        if set(listed_task_ids) != set(expected_task_ids):
            raise PlanError(
                f"phase {phase_id} task list differs from manifest: "
                f"expected {expected_task_ids}, got {listed_task_ids}"
            )

    for task_id, task in by_id.items():
        dependencies = task.get("depends_on")
        if not isinstance(dependencies, list) or len(set(dependencies)) != len(dependencies):
            raise PlanError(f"task {task_id} dependencies must be a unique array")
        for dependency in dependencies:
            if dependency not in by_id:
                raise PlanError(f"task {task_id} has unknown dependency {dependency}")
            if by_id[dependency]["execution_phase"] > task["execution_phase"]:
                raise PlanError(
                    f"forward phase dependency: {dependency} -> {task_id}"
                )

    visiting: set[str] = set()
    visited: set[str] = set()

    def visit(task_id: str) -> None:
        if task_id in visiting:
            raise PlanError(f"dependency cycle contains task {task_id}")
        if task_id in visited:
            return
        visiting.add(task_id)
        for dependency in by_id[task_id]["depends_on"]:
            visit(dependency)
        visiting.remove(task_id)
        visited.add(task_id)

    for task_id in by_id:
        visit(task_id)

    task_progress = progress.get("tasks")
    gate_progress = progress.get("phase_gates")
    if not isinstance(task_progress, dict) or set(task_progress) != set(by_id):
        raise PlanError("progress task IDs do not exactly match the manifest")
    expected_gates = {str(p["id"]) for p in phases}
    if not isinstance(gate_progress, dict) or set(gate_progress) != expected_gates:
        raise PlanError("progress phase gates do not exactly match the manifest")

    active: list[str] = []
    for task_id, state in task_progress.items():
        if not isinstance(state, dict):
            raise PlanError(f"task {task_id} progress must be an object")
        status = state.get("status")
        if status not in ALLOWED_STATUS:
            raise PlanError(f"task {task_id} has invalid status {status!r}")
        if status == "in_progress":
            active.append(task_id)
        if status in {"in_progress", "complete"}:
            incomplete = [d for d in by_id[task_id]["depends_on"] if task_progress[d]["status"] != "complete"]
            if incomplete:
                raise PlanError(f"task {task_id} advanced before dependencies {incomplete}")
        if status == "complete":
            handoff = state.get("handoff")
            if not handoff or not (root / handoff).is_file():
                raise PlanError(f"complete task {task_id} lacks a handoff file")
            review = human_reviews.get(task_id)
            if review and review[0] and not review[1].is_file():
                raise PlanError(
                    f"complete task {task_id} lacks required human-review artifact {review[1]}"
                )

    first_open_phase: int | None = None
    for phase in phases:
        phase_id = phase["id"]
        gate = gate_progress[str(phase_id)]
        if not isinstance(gate, dict):
            raise PlanError(f"phase {phase_id} gate progress must be an object")
        status = gate.get("status")
        if status not in ALLOWED_STATUS:
            raise PlanError(f"phase {phase_id} has invalid gate status {status!r}")
        phase_task_ids = [t["id"] for t in tasks if t["execution_phase"] == phase_id]
        if status == "in_progress":
            active.append(f"phase-{phase_id:02d}")
        if status in {"in_progress", "complete"} and any(
            task_progress[t]["status"] != "complete" for t in phase_task_ids
        ):
            raise PlanError(f"phase {phase_id} gate advanced before all tasks completed")
        if status == "complete":
            handoff = gate.get("handoff")
            if not handoff or not (root / handoff).is_file():
                raise PlanError(f"complete phase {phase_id} gate lacks a handoff")
        elif first_open_phase is None:
            first_open_phase = phase_id
        elif any(task_progress[t]["status"] != "pending" for t in phase_task_ids):
            raise PlanError(f"phase {phase_id} work started before prior phase gate")

    if len(active) > 1:
        raise PlanError(f"more than one work item is in progress: {active}")

    archive = root / str(manifest.get("archive", ""))
    if not archive.is_file():
        raise PlanError(f"audit archive missing: {archive}")
    archive_digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    if archive_digest != manifest.get("archive_sha256"):
        raise PlanError("audit archive content changed")

    canonical_markdown = [
        root / "README.md",
        root / "CONTRACTS.md",
        revision_log,
        root / "AGENT_LOOP.md",
        root / "handoffs" / "TEMPLATE.md",
        root / "handoffs" / "HUMAN_REVIEW_TEMPLATE.md",
        *phase_files,
        *(root / task["spec"] for task in tasks),
    ]
    if any(not path.is_file() for path in canonical_markdown):
        raise PlanError("a canonical Markdown file is missing")
    _validate_local_links(root, canonical_markdown)
    return manifest, progress


def main() -> int:
    try:
        manifest, _ = validate()
    except PlanError as exc:
        print(f"plan invalid: {exc}", file=sys.stderr)
        return 1
    print(
        f"plan valid: {len(manifest['tasks'])} tasks, "
        f"{len(manifest['phases'])} phases, acyclic and phase-safe"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

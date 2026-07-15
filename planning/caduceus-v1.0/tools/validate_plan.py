#!/usr/bin/env python3
"""Validate the Caduceus v1.0 plan and mutable progress ledger."""

from __future__ import annotations

import hashlib
import json
import re
import sys
from pathlib import Path
from urllib.parse import unquote

ROOT = Path(__file__).resolve().parents[1]
ALLOWED_STATUS = {"pending", "in_progress", "complete", "blocked"}
ALLOWED_CATALOG_STATUS = {"draft", "active"}
PLACEHOLDERS = {
    "",
    "-",
    "n/a",
    "na",
    "none",
    "not applicable",
    "tbd",
    "todo",
    "placeholder",
}
FAILED_EVIDENCE = {"deferred", "stubbed", "failed", "contradicted"}


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


def _acceptance_ids(value: object, pattern: str, owner: str) -> list[str]:
    if not isinstance(value, list) or not value:
        raise PlanError(f"active {owner} needs a non-empty acceptance_ids array")
    if len(value) != len(set(value)):
        raise PlanError(f"{owner} acceptance IDs must be unique")
    for acceptance_id in value:
        if not isinstance(acceptance_id, str) or not re.fullmatch(pattern, acceptance_id):
            raise PlanError(f"{owner} has invalid acceptance ID {acceptance_id!r}")
    return value


def _meaningful(value: str, *, generic: set[str] | None = None) -> bool:
    normalized = " ".join(value.strip().casefold().split())
    if re.fullmatch(r"<[^>]+>|\[[^]]+\]|\{[^}]+\}", normalized):
        return False
    return len(normalized) >= 3 and normalized not in PLACEHOLDERS | (generic or set())


def _validate_pass_evidence(path: Path, acceptance_ids: list[str], owner: str) -> None:
    text = path.read_text(encoding="utf-8")
    for acceptance_id in acceptance_ids:
        rows = []
        for line in text.splitlines():
            cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
            if cells and cells[0] == acceptance_id:
                rows.append(cells)
        if len(rows) != 1 or len(rows[0]) != 5:
            raise PlanError(
                f"complete {owner} needs one five-column evidence row for "
                f"{acceptance_id}"
            )
        _, status, procedure, result, artifact = rows[0]
        if status.casefold() not in {"pass", "passed"}:
            raise PlanError(f"complete {owner} has non-PASS evidence for {acceptance_id}")
        values = (procedure, result, artifact)
        if any(
            set(re.findall(r"[a-z]+", value.casefold())) & FAILED_EVIDENCE
            for value in values
        ):
            raise PlanError(f"complete {owner} has invalid evidence for {acceptance_id}")
        if not _meaningful(procedure):
            raise PlanError(f"{acceptance_id} lacks a meaningful command or procedure")
        if not _meaningful(result, generic={"pass", "passed", "success"}):
            raise PlanError(f"{acceptance_id} lacks a meaningful result")
        if not _meaningful(artifact, generic={"pass", "passed", "success"}):
            raise PlanError(f"{acceptance_id} lacks a durable artifact or test reference")


def _validate_human_review(path: Path, task_id: str, implementation_handoff: str) -> None:
    text = path.read_text(encoding="utf-8")
    def field(label: str) -> str:
        match = re.search(rf"(?im)^- {re.escape(label)}:\s*(.*?)\s*$", text)
        return match.group(1).strip() if match else ""

    actor = field("Implementation actor")
    reviewer = field("Reviewer name / handle")
    commit = field("Reviewed commit")
    provenance = field("External approval provenance")
    declared_handoff = field("Implementation handoff").strip("`")
    decision = field("Decision").casefold()
    if not _meaningful(actor) or not _meaningful(reviewer):
        raise PlanError(f"task {task_id} human review lacks actor or reviewer")
    def normalize(value: str) -> str:
        return " ".join(value.casefold().split()).lstrip("@")
    if normalize(actor) == normalize(reviewer):
        raise PlanError(f"task {task_id} implementation actor cannot review own work")
    if not re.fullmatch(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})", commit):
        raise PlanError(f"task {task_id} human review has invalid reviewed commit")
    if set(commit) == {"0"}:
        raise PlanError(f"task {task_id} human review uses an all-zero commit")
    provenance_pattern = r"https://\S+|(?:PR|pull request)\s*#\d+"
    if not _meaningful(provenance) or not re.fullmatch(
        provenance_pattern, provenance, re.IGNORECASE
    ):
        raise PlanError(f"task {task_id} human review lacks approval provenance")
    if decision not in {"approved", "approved with notes"}:
        raise PlanError(f"task {task_id} human review is not approved")
    if declared_handoff != implementation_handoff:
        raise PlanError(
            f"task {task_id} human review names the wrong implementation handoff"
        )


def _validate_blocker_handoff(path: Path, owner: str) -> None:
    text = path.read_text(encoding="utf-8")
    match = re.search(r"(?im)^- Blocker evidence \(blocked only\):\s*(.*?)\s*$", text)
    if not match or not _meaningful(match.group(1)):
        raise PlanError(f"blocked {owner} lacks meaningful blocker evidence")


def _handoff_path(root: Path, value: object, owner: str) -> Path:
    if not isinstance(value, str) or not value:
        raise PlanError(f"{owner} lacks a handoff path")
    path = (root / value).resolve()
    if path.parent != (root / "handoffs").resolve() or not path.is_file():
        raise PlanError(f"{owner} has invalid handoff path: {value}")
    return path


def _tree_digest(root: Path) -> str:
    if not root.is_dir():
        raise PlanError(f"sealed tree missing: {root}")
    digest = hashlib.sha256()
    files = []
    for path in root.rglob("*"):
        relative = path.relative_to(root)
        if "__pycache__" in relative.parts or path.suffix == ".pyc":
            continue
        if path.name == ".progress.lock" or not path.is_file():
            continue
        files.append((relative.as_posix(), path))
    for relative, path in sorted(files):
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def _validate_catalog_shape(
    catalog_status: str, phases: list[dict], tasks: list[dict]
) -> None:
    if catalog_status != "active":
        return
    for phase in phases:
        phase_id = phase.get("id")
        if not any(task.get("execution_phase") == phase_id for task in tasks):
            raise PlanError(f"active phase {phase_id} has no tasks")


def _validate_local_links(root: Path, files: list[Path]) -> None:
    link_pattern = re.compile(r"(?<!!)\[[^\]]*\]\(([^)]+)\)")
    for source in files:
        for raw_target in link_pattern.findall(source.read_text(encoding="utf-8")):
            target = raw_target.strip().split("#", 1)[0]
            if not target or "://" in target or target.startswith(("mailto:", "#")):
                continue
            target = unquote(target.strip("<>"))
            resolved = (source.parent / target).resolve()
            try:
                resolved.relative_to(root.resolve())
            except ValueError as exc:
                raise PlanError(f"link escapes plan root: {source} -> {raw_target}") from exc
            if not resolved.exists():
                raise PlanError(f"broken local link: {source} -> {raw_target}")


def validate(root: Path = ROOT, progress_override: dict | None = None) -> tuple[dict, dict]:
    manifest = _load(root / "task-manifest.json")
    progress = progress_override if progress_override is not None else _load(
        root / "progress.json"
    )
    if not isinstance(progress, dict):
        raise PlanError("progress override must be an object")

    if manifest.get("schema_version") != 1 or progress.get("schema_version") != 1:
        raise PlanError("unsupported manifest/progress schema version")
    if manifest.get("plan_id") != progress.get("plan_id"):
        raise PlanError("manifest and progress plan_id differ")
    catalog_status = manifest.get("catalog_status")
    if catalog_status not in ALLOWED_CATALOG_STATUS:
        raise PlanError("manifest catalog_status must be draft or active")

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

    v01_tree = (root / str(manifest.get("v01_tree", ""))).resolve()
    expected_v01_digest = manifest.get("v01_tree_sha256")
    if not isinstance(expected_v01_digest, str) or not expected_v01_digest:
        raise PlanError("manifest lacks v01_tree_sha256")
    actual_v01_digest = _tree_digest(v01_tree)
    if actual_v01_digest != expected_v01_digest:
        raise PlanError(
            "sealed v0.1 planning tree changed "
            f"(expected {expected_v01_digest}, got {actual_v01_digest})"
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
    _validate_catalog_shape(catalog_status, phases, tasks)

    by_id: dict[str, dict] = {}
    human_reviews: dict[str, tuple[bool, Path]] = {}
    task_acceptance: dict[str, list[str]] = {}
    expected_task_specs: set[Path] = set()
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
        expected_task_specs.add(spec.resolve())
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
        expected_dependencies = ", ".join(task.get("depends_on", []))
        if fields.get("depends_on") != expected_dependencies:
            raise PlanError(f"task {task_id} packet dependencies do not match")
        if fields.get("owns") != task.get("owns"):
            raise PlanError(f"task {task_id} packet owned surfaces do not match")
        if fields.get("primary_tests") != task.get("primary_tests"):
            raise PlanError(f"task {task_id} packet primary tests do not match")
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
        if catalog_status == "active":
            acceptance_ids = _acceptance_ids(
                task.get("acceptance_ids"),
                rf"{re.escape(task_id)}-AC-[0-9]{{2}}",
                f"task {task_id}",
            )
            acceptance_section = packet.partition("## Acceptance checks\n")[2].partition(
                "\n## Handoff expectations"
            )[0]
            packet_acceptance = re.findall(
                r"\*\*([0-9]+\.[0-9]+-AC-[0-9]{2})\*\*",
                acceptance_section,
            )
            all_packet_acceptance = re.findall(
                r"\*\*([0-9]+\.[0-9]+-AC-[0-9]{2})\*\*",
                packet,
            )
            if all_packet_acceptance != packet_acceptance:
                raise PlanError(
                    f"task {task_id} has acceptance IDs outside its "
                    "Acceptance checks section"
                )
            if packet_acceptance != acceptance_ids:
                raise PlanError(
                    f"task {task_id} acceptance section differs from manifest: "
                    f"expected {acceptance_ids}, got {packet_acceptance}"
                )
            if len(packet_acceptance) != len(set(packet_acceptance)):
                raise PlanError(f"task {task_id} repeats an acceptance ID")
            task_acceptance[task_id] = acceptance_ids

        review = task.get("human_review")
        if review is not None:
            if not isinstance(review, dict) or not isinstance(
                review.get("required"), bool
            ):
                raise PlanError(f"task {task_id} has invalid human_review metadata")
            handoff_value = review.get("handoff")
            if not isinstance(handoff_value, str) or not handoff_value:
                raise PlanError(f"task {task_id} human_review needs a handoff path")
            review_path = (root / handoff_value).resolve()
            try:
                review_path.relative_to(root.resolve())
            except ValueError as exc:
                raise PlanError(
                    f"task {task_id} human-review path escapes plan root"
                ) from exc
            if review_path.parent != (root / "handoffs").resolve():
                raise PlanError(
                    f"task {task_id} human-review artifact must be under handoffs/"
                )
            human_reviews[task_id] = (review["required"], review_path)

    actual_task_specs = {path.resolve() for path in (root / "tasks").glob("*.md")}
    if actual_task_specs != expected_task_specs:
        missing = sorted(str(path) for path in expected_task_specs - actual_task_specs)
        orphaned = sorted(str(path) for path in actual_task_specs - expected_task_specs)
        raise PlanError(
            f"task packet set differs from manifest: missing={missing}, "
            f"orphaned={orphaned}"
        )

    phase_files: list[Path] = []
    phase_acceptance: dict[int, list[str]] = {}
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
        if catalog_status == "active":
            acceptance_ids = _acceptance_ids(
                phase.get("acceptance_ids"),
                rf"PHASE-{phase_id:02d}-AC-[0-9]{{2}}",
                f"phase {phase_id}",
            )
            for acceptance_id in acceptance_ids:
                if acceptance_id not in phase_text:
                    raise PlanError(
                        f"phase {phase_id} spec omits acceptance ID {acceptance_id}"
                    )
            phase_acceptance[phase_id] = acceptance_ids
        listed_task_ids = re.findall(r"^- \[Task ([0-9]+\.[0-9]+):", phase_text, re.MULTILINE)
        if len(listed_task_ids) != len(set(listed_task_ids)):
            raise PlanError(f"phase {phase_id} lists a task more than once")
        if listed_task_ids != expected_task_ids:
            raise PlanError(
                f"phase {phase_id} task order differs from manifest: "
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

    requirement_ids = set(re.findall(
        r"^### ([A-Z]+-[0-9]{3}) —", contracts.read_text(encoding="utf-8"),
        re.MULTILINE,
    ))
    requirement_map = manifest.get("requirement_map")
    if not isinstance(requirement_map, dict) or set(requirement_map) != requirement_ids:
        raise PlanError("manifest requirement map does not exactly cover the contract")
    all_acceptance = {
        acceptance_id
        for acceptance_ids in task_acceptance.values()
        for acceptance_id in acceptance_ids
    }
    for requirement_id, mapped in requirement_map.items():
        if not isinstance(mapped, list) or not mapped:
            raise PlanError(f"requirement {requirement_id} has no acceptance mapping")
        if any(value not in all_acceptance for value in mapped):
            raise PlanError(f"requirement {requirement_id} maps to an unknown acceptance ID")

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
            handoff_path = _handoff_path(root, handoff, f"complete task {task_id}")
            _validate_pass_evidence(
                handoff_path, task_acceptance.get(task_id, []), f"task {task_id}"
            )
            review = human_reviews.get(task_id)
            if review and review[0] and not review[1].is_file():
                raise PlanError(
                    f"complete task {task_id} lacks required human-review artifact {review[1]}"
                )
            if review and review[0]:
                _validate_human_review(review[1], task_id, handoff)
        elif status == "blocked":
            handoff = state.get("handoff")
            handoff_path = _handoff_path(root, handoff, f"blocked task {task_id}")
            _validate_blocker_handoff(handoff_path, f"task {task_id}")

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
            handoff_path = _handoff_path(root, handoff, f"complete phase {phase_id}")
            _validate_pass_evidence(
                handoff_path,
                phase_acceptance.get(phase_id, []),
                f"phase {phase_id}",
            )
        elif status == "blocked":
            handoff = gate.get("handoff")
            handoff_path = _handoff_path(root, handoff, f"blocked phase {phase_id}")
            _validate_blocker_handoff(handoff_path, f"phase {phase_id}")
        elif first_open_phase is None:
            first_open_phase = phase_id
        elif any(task_progress[t]["status"] != "pending" for t in phase_task_ids):
            raise PlanError(f"phase {phase_id} work started before prior phase gate")

    if len(active) > 1:
        raise PlanError(f"more than one work item is in progress: {active}")

    if catalog_status == "draft":
        advanced_tasks = [
            task_id for task_id, state in task_progress.items()
            if state["status"] != "pending"
        ]
        advanced_gates = [
            phase_id for phase_id, state in gate_progress.items()
            if state["status"] != "pending"
        ]
        if advanced_tasks or advanced_gates:
            raise PlanError(
                "draft catalog cannot contain progress transitions: "
                f"tasks={advanced_tasks}, phase_gates={advanced_gates}"
            )

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
        f"plan valid ({manifest['catalog_status']} catalog): "
        f"{len(manifest['tasks'])} tasks, {len(manifest['phases'])} phases, "
        "acyclic and phase-safe"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

"""Negative-test driver for set_status evidence enforcement.

Each test exercises one PLAN-002 / PLAN-003 / atomic-save
requirement. The driver does not mutate the live progress ledger
under planning/caduceus-v1.0; it copies the planning tree into a
sandbox, mutates only the sandbox, and asserts the validator and
set_status.py both reject the bad handoff.

The sandbox rewrites progress.json so the task under test is the
one the controller would select next (0.1 is the canonical choice;
it is the only task with no dependencies and is the first item
returned by next_task.py).
"""
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path("/home/agent/projects/barkley-assistant/caduceus")
PLAN = REPO / "planning" / "caduceus-v1.0"

failures: list[str] = []


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, **kwargs)


def sandbox() -> Path:
    """Copy the planning tree into a writable temp directory.

    The v0.1 sealed archive must be reachable as
    ``<sibling-of-plan>/caduceus-v0.1`` (the manifest's
    ``v01_tree: ../caduceus-v0.1`` is relative to the plan root),
    so the layout is::

        <tmp>/plan          <- sandbox of caduceus-v1.0
        <tmp>/caduceus-v0.1 <- copy of the sealed archive

    The validator's ``_tree_digest`` will then hash the sealed
    tree and the test runs the full evidence check rather than
    the short-circuit "sealed tree missing" error.
    """
    tmp = Path(tempfile.mkdtemp(prefix="set-status-neg-"))
    shutil.copytree(PLAN, tmp / "plan")
    shutil.copytree(REPO / "planning" / "caduceus-v0.1", tmp / "caduceus-v0.1")
    return tmp


def claim_task_in_progress(plan_root: Path, task_id: str) -> str:
    """Mark all of phase 0 pending, claim ``task_id``, and leave it
    in_progress so the AC-03 tests can target a known task ID.

    The validator enforces one evidence row per acceptance_id;
    by claiming the same task explicitly each AC-03 test can
    author a complete row set for the four 0.2 acceptance IDs.
    """
    progress_path = plan_root / "progress.json"
    progress = json.loads(progress_path.read_text(encoding="utf-8"))
    for state in progress["tasks"].values():
        state["status"] = "pending"
        state["handoff"] = None
    for state in progress["phase_gates"].values():
        state["status"] = "pending"
        state["handoff"] = None
    progress_path.write_text(json.dumps(progress, indent=2) + "\n", encoding="utf-8")
    proc = run(
        ["python3", str(plan_root / "tools" / "set_status.py"), task_id, "in_progress"],
        cwd=str(plan_root / "tools"),
    )
    if proc.returncode != 0:
        raise RuntimeError(f"sandbox claim failed: {proc.stderr}")
    return task_id


def claim_first_pending(plan_root: Path) -> str:
    """Reset the sandbox progress.json to all-pending, then claim 0.1.

    Each sandbox test starts from a clean slate so the test
    always exercises task 0.1's acceptance evidence rules, not
    whatever happens to be the live next-task in the repository
    progress.json we copied.
    """
    return claim_task_in_progress(plan_root, "0.1")


def write_handoff(handoff: Path, acceptance_id: str, *, status: str = "PASS",
                  procedure: str = "echo ok", result: str = "ok",
                  artifact: str = "sandbox artifact",
                  forbidden: str | None = None) -> None:
    if forbidden is not None:
        procedure += f" {forbidden}"
    handoff.parent.mkdir(parents=True, exist_ok=True)
    handoff.write_text(
        f"""# Handoff

- Work item: 0.1
- Outcome: complete
- Files changed: none
- Public signatures/contracts used: none
- State/schema effects: none
- Tests added or changed: none
- Commands run: sandbox
- Results: sandbox
- Forbidden-side-effect checks: sandbox
- Residual risks: none
- Blocker evidence (blocked only): n/a

## Acceptance evidence

| Acceptance ID | Status | Command or procedure | Result | Artifact |
|---|---|---|---|---|
| {acceptance_id} | {status} | {procedure} | {result} | {artifact} |
""",
        encoding="utf-8",
    )


def run_set_status(plan_root: Path, task_id: str, handoff_rel: str) -> subprocess.CompletedProcess:
    return run(
        [
            "python3",
            str(plan_root / "tools" / "set_status.py"),
            task_id,
            "complete",
            "--handoff",
            handoff_rel,
        ],
        cwd=str(plan_root / "tools"),
    )


def expect_reject(test_name: str, plan_root: Path, task_id: str,
                  handoff: Path, *, expected_substrings: list[str]) -> None:
    proc = run_set_status(plan_root, task_id, str(handoff.relative_to(plan_root)))
    if proc.returncode == 0:
        failures.append(f"{test_name}: was accepted")
        return
    if not all(s in proc.stderr for s in expected_substrings):
        failures.append(
            f"{test_name}: rejection reason did not include {expected_substrings!r}; "
            f"got: {proc.stderr.strip()}"
        )
        return
    print(f"{test_name} OK: {proc.stderr.strip()}")


# ---------------------------------------------------------------------------
# AC-01: Reject incomplete or adverse completion evidence.
# ---------------------------------------------------------------------------

def test_ac01_rejects_failed_evidence() -> None:
    tmp = sandbox()
    plan_root = tmp / "plan"
    task_id = claim_first_pending(plan_root)
    handoff = plan_root / "handoffs" / f"{task_id}.md"
    write_handoff(handoff, f"{task_id}-AC-01", procedure="echo ok",
                  result="ok", artifact=str(handoff.relative_to(plan_root)),
                  forbidden="failed")
    expect_reject("AC-01 (failed)", plan_root, task_id, handoff,
                  expected_substrings=["invalid evidence"])
    shutil.rmtree(tmp, ignore_errors=True)


def test_ac01_rejects_deferred_evidence() -> None:
    tmp = sandbox()
    plan_root = tmp / "plan"
    task_id = claim_first_pending(plan_root)
    handoff = plan_root / "handoffs" / f"{task_id}.md"
    write_handoff(handoff, f"{task_id}-AC-01", procedure="echo ok",
                  result="ok", artifact=str(handoff.relative_to(plan_root)),
                  forbidden="deferred")
    expect_reject("AC-01 (deferred)", plan_root, task_id, handoff,
                  expected_substrings=["invalid evidence"])
    shutil.rmtree(tmp, ignore_errors=True)


def test_ac01_rejects_stubbed_evidence() -> None:
    tmp = sandbox()
    plan_root = tmp / "plan"
    task_id = claim_first_pending(plan_root)
    handoff = plan_root / "handoffs" / f"{task_id}.md"
    write_handoff(handoff, f"{task_id}-AC-01", procedure="echo ok",
                  result="ok", artifact=str(handoff.relative_to(plan_root)),
                  forbidden="stubbed")
    expect_reject("AC-01 (stubbed)", plan_root, task_id, handoff,
                  expected_substrings=["invalid evidence"])
    shutil.rmtree(tmp, ignore_errors=True)


def test_ac01_rejects_contradicted_evidence() -> None:
    tmp = sandbox()
    plan_root = tmp / "plan"
    task_id = claim_first_pending(plan_root)
    handoff = plan_root / "handoffs" / f"{task_id}.md"
    write_handoff(handoff, f"{task_id}-AC-01", procedure="echo ok",
                  result="ok", artifact=str(handoff.relative_to(plan_root)),
                  forbidden="contradicted")
    expect_reject("AC-01 (contradicted)", plan_root, task_id, handoff,
                  expected_substrings=["invalid evidence"])
    shutil.rmtree(tmp, ignore_errors=True)


# ---------------------------------------------------------------------------
# AC-02: Require a meaningful procedure, result, and artifact.
# ---------------------------------------------------------------------------

def test_ac02_rejects_dash_procedure() -> None:
    tmp = sandbox()
    plan_root = tmp / "plan"
    task_id = claim_first_pending(plan_root)
    handoff = plan_root / "handoffs" / f"{task_id}.md"
    write_handoff(handoff, f"{task_id}-AC-01", procedure="-",
                  result="the validator printed a complaint about a dash",
                  artifact=str(handoff.relative_to(plan_root)))
    expect_reject("AC-02 (procedure -)", plan_root, task_id, handoff,
                  expected_substrings=["lacks a meaningful command"])
    shutil.rmtree(tmp, ignore_errors=True)


def test_ac02_rejects_na_artifact() -> None:
    tmp = sandbox()
    plan_root = tmp / "plan"
    task_id = claim_first_pending(plan_root)
    handoff = plan_root / "handoffs" / f"{task_id}.md"
    write_handoff(handoff, f"{task_id}-AC-01", procedure="echo ok",
                  result="the validator printed a complaint about an n/a artifact",
                  artifact="n/a")
    expect_reject("AC-02 (artifact n/a)", plan_root, task_id, handoff,
                  expected_substrings=["lacks a durable artifact"])
    shutil.rmtree(tmp, ignore_errors=True)


def test_ac02_rejects_tbd_result() -> None:
    tmp = sandbox()
    plan_root = tmp / "plan"
    task_id = claim_first_pending(plan_root)
    handoff = plan_root / "handoffs" / f"{task_id}.md"
    write_handoff(handoff, f"{task_id}-AC-01", procedure="echo ok",
                  result="TBD", artifact=str(handoff.relative_to(plan_root)))
    expect_reject("AC-02 (result TBD)", plan_root, task_id, handoff,
                  expected_substrings=["lacks a meaningful result"])
    shutil.rmtree(tmp, ignore_errors=True)


# ---------------------------------------------------------------------------
# AC-03: Reject invalid or non-independent review evidence.
# ---------------------------------------------------------------------------
# AC-03 tests the human-review surface directly, without going
# through the full ``set_status.py`` machinery. The validator's
# ``_validate_human_review`` is the single source of truth for
# PLAN-003; we synthesize the artifact and handoff in a sandbox
# and call ``validate()`` with a progress override that marks the
# task complete with the synthesized handoff path.


def _force_human_review_required(plan_root: Path, task_id: str) -> None:
    """Toggle the task's human_review.required flag in the manifest."""
    manifest_path = plan_root / "task-manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    for task in manifest["tasks"]:
        if task["id"] == task_id:
            task["human_review"] = {
                "required": True,
                "handoff": f"handoffs/{task_id}.review.md",
            }
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")


def _write_review(plan_root: Path, task_id: str, *,
                  actor: str = "agent-one", reviewer: str = "agent-two",
                  commit: str = "0123456789abcdef0123456789abcdef01234567",
                  provenance: str = "https://example.invalid/pr/1") -> None:
    review = plan_root / "handoffs" / f"{task_id}.review.md"
    review.parent.mkdir(parents=True, exist_ok=True)
    review.write_text(f"""# Human review
- Implementation actor: {actor}
- Reviewer name / handle: {reviewer}
- Review date: 2026-07-15
- Reviewed commit: {commit}
- External approval provenance: {provenance}
- Implementation handoff: handoffs/{task_id}.md
## Acceptance checklist
- [x] ok
## Decision
- Decision: approved
## Notes
""", encoding="utf-8")


# Task 0.2 has 4 acceptance IDs; the AC-03 tests must author all
# four rows so the validator's evidence loop reaches the
# human-review path.
_02_ACCEPTANCE_IDS = ["0.2-AC-01", "0.2-AC-02", "0.2-AC-03", "0.2-AC-04"]


def _write_complete_handoff_for_02(plan_root: Path, *, target_id: str) -> Path:
    """Write a 0.2 handoff whose rows are valid except the ``target_id``
    row carries a forged value the validator must reject for a
    different reason than the row count.
    """
    handoff = plan_root / "handoffs" / "0.2.md"
    rows = []
    for ac in _02_ACCEPTANCE_IDS:
        if ac == target_id:
            rows.append((ac, "PASS", "echo ok", "ok", "handoffs/0.2.md"))
        else:
            rows.append((
                ac, "PASS",
                "echo ok",
                "validator should accept this row",
                "handoffs/0.2.md",
            ))
    table = "\n".join(
        f"| {r[0]} | {r[1]} | {r[2]} | {r[3]} | {r[4]} |"
        for r in rows
    )
    handoff.parent.mkdir(parents=True, exist_ok=True)
    handoff.write_text(f"""# Handoff

- Work item: 0.2
- Outcome: complete
- Files changed: none
- Public signatures/contracts used: none
- State/schema effects: none
- Tests added or changed: none
- Commands run: sandbox
- Results: sandbox
- Forbidden-side-effect checks: sandbox
- Residual risks: none
- Blocker evidence (blocked only): n/a

## Acceptance evidence

| Acceptance ID | Status | Command or procedure | Result | Artifact |
|---|---|---|---|---|
{table}
""", encoding="utf-8")
    return handoff


def _mutate_row(handoff: Path, target_id: str, *, column: int, value: str) -> None:
    """Replace one cell of the ``target_id`` row with ``value``."""
    text = handoff.read_text(encoding="utf-8")
    for line in text.splitlines():
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if cells and cells[0] == target_id:
            new_cells = list(cells)
            new_cells[column] = value
            new_line = "| " + " | ".join(new_cells) + " |"
            text = text.replace(line, new_line, 1)
            handoff.write_text(text, encoding="utf-8")
            return
    raise AssertionError(f"row {target_id} not found in {handoff}")


def _validate_sandbox(sandbox_root: Path) -> tuple[bool, str]:
    """Run ``validate(plan_root)`` against a sandbox and return
    ``(ok, message)``. ``ok`` is False if the validator raised a
    PlanError; ``message`` is the error text.
    """
    sys.path.insert(0, str(sandbox_root / "tools"))
    try:
        # Re-import to pick up the sandbox copy.
        for mod in list(sys.modules):
            if mod == "validate_plan":
                del sys.modules[mod]
        sys.path.insert(0, str(sandbox_root / "tools"))
        import validate_plan as vp
        try:
            vp.validate(sandbox_root)
            return True, ""
        except vp.PlanError as exc:
            return False, str(exc)
    finally:
        # Restore the live validator import path.
        sys.path.pop(0)
        sys.path.pop(0)


def _set_task_complete_with_handoff(sandbox_root: Path, task_id: str,
                                    handoff_rel: str) -> None:
    """Mark the task complete in the sandbox's progress.json so the
    validator's evidence loop runs against the synthesized handoff.
    """
    progress_path = sandbox_root / "progress.json"
    progress = json.loads(progress_path.read_text(encoding="utf-8"))
    progress["tasks"][task_id]["status"] = "complete"
    progress["tasks"][task_id]["handoff"] = handoff_rel
    progress_path.write_text(json.dumps(progress, indent=2) + "\n", encoding="utf-8")


def test_ac03_rejects_same_actor_and_reviewer() -> None:
    tmp = sandbox()
    plan_root = tmp / "plan"
    sandbox_root = plan_root
    _force_human_review_required(plan_root, "0.2")
    handoff = _write_complete_handoff_for_02(plan_root, target_id="0.2-AC-04")
    _mutate_row(handoff, "0.2-AC-04", column=3,
                value="validator should reach the human-review path")
    _write_review(plan_root, "0.2", actor="agent-one", reviewer="agent-one")
    _set_task_complete_with_handoff(plan_root, "0.2", "handoffs/0.2.md")
    ok, msg = _validate_sandbox(sandbox_root)
    if ok or "cannot review own work" not in msg:
        failures.append(f"AC-03 (same actor/reviewer): ok={ok} msg={msg!r}")
    else:
        print(f"AC-03 (same actor/reviewer) OK: {msg}")
    shutil.rmtree(tmp, ignore_errors=True)


def test_ac03_rejects_all_zero_commit() -> None:
    tmp = sandbox()
    plan_root = tmp / "plan"
    sandbox_root = plan_root
    _force_human_review_required(plan_root, "0.2")
    handoff = _write_complete_handoff_for_02(plan_root, target_id="0.2-AC-04")
    _mutate_row(handoff, "0.2-AC-04", column=3,
                value="validator should reach the human-review path")
    _write_review(plan_root, "0.2",
                  commit="0000000000000000000000000000000000000000")
    _set_task_complete_with_handoff(plan_root, "0.2", "handoffs/0.2.md")
    ok, msg = _validate_sandbox(sandbox_root)
    if ok or "all-zero commit" not in msg:
        failures.append(f"AC-03 (all-zero commit): ok={ok} msg={msg!r}")
    else:
        print(f"AC-03 (all-zero commit) OK: {msg}")
    shutil.rmtree(tmp, ignore_errors=True)


def test_ac03_rejects_missing_provenance() -> None:
    tmp = sandbox()
    plan_root = tmp / "plan"
    sandbox_root = plan_root
    _force_human_review_required(plan_root, "0.2")
    handoff = _write_complete_handoff_for_02(plan_root, target_id="0.2-AC-04")
    _mutate_row(handoff, "0.2-AC-04", column=3,
                value="validator should reach the human-review path")
    _write_review(plan_root, "0.2", provenance="n/a")
    _set_task_complete_with_handoff(plan_root, "0.2", "handoffs/0.2.md")
    ok, msg = _validate_sandbox(sandbox_root)
    if ok or "lacks approval provenance" not in msg:
        failures.append(f"AC-03 (missing provenance): ok={ok} msg={msg!r}")
    else:
        print(f"AC-03 (missing provenance) OK: {msg}")
    shutil.rmtree(tmp, ignore_errors=True)


# ---------------------------------------------------------------------------
# AC-04: Atomic replacement cleans up the temp file on failure and fsyncs
# the directory before and after the rename.
# ---------------------------------------------------------------------------

def test_ac04_cleans_up_temp_on_failure() -> None:
    """Force the json.dump to fail by passing a non-serializable value.

    The replacement must not have happened and the temp file must
    not be left behind.
    """
    import importlib.util
    spec = importlib.util.spec_from_file_location(
        "set_status_module",
        str(PLAN / "tools" / "set_status.py"),
    )
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)

    progress_path = PLAN / "progress.json"
    pre_bytes = progress_path.read_bytes()

    bad = {"schema_version": 1, "oops": object()}  # not JSON-serializable
    try:
        module._save(progress_path, bad)
        failures.append("AC-04: _save did not raise on non-serializable value")
    except TypeError as exc:
        post_bytes = progress_path.read_bytes()
        if post_bytes != pre_bytes:
            failures.append("AC-04: progress.json mutated on failure path")
        else:
            print(f"AC-04 (TypeError) OK: {exc}")
    leftovers = list(progress_path.parent.glob(f".{progress_path.name}.*.tmp"))
    if leftovers:
        failures.append(f"AC-04: temp file left behind: {leftovers}")
    else:
        print("AC-04 (no temp leftovers) OK")


def test_ac04_fsync_directory_on_success() -> None:
    """A successful _save must not leave a temp file behind."""
    import importlib.util
    spec = importlib.util.spec_from_file_location(
        "set_status_module",
        str(PLAN / "tools" / "set_status.py"),
    )
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)

    progress_path = PLAN / "progress.json"
    pre = json.loads(progress_path.read_text(encoding="utf-8"))
    module._save(progress_path, pre)
    leftovers = list(progress_path.parent.glob(f".{progress_path.name}.*.tmp"))
    if leftovers:
        failures.append(f"AC-04: temp file left on success path: {leftovers}")
    else:
        print("AC-04 (success path) OK")
    post = json.loads(progress_path.read_text(encoding="utf-8"))
    if pre != post:
        failures.append("AC-04: progress.json differs after round-trip")


def main() -> int:
    test_ac01_rejects_failed_evidence()
    test_ac01_rejects_deferred_evidence()
    test_ac01_rejects_stubbed_evidence()
    test_ac01_rejects_contradicted_evidence()
    test_ac02_rejects_dash_procedure()
    test_ac02_rejects_na_artifact()
    test_ac02_rejects_tbd_result()
    test_ac03_rejects_same_actor_and_reviewer()
    test_ac03_rejects_all_zero_commit()
    test_ac03_rejects_missing_provenance()
    test_ac04_cleans_up_temp_on_failure()
    test_ac04_fsync_directory_on_success()
    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        return 1
    print(f"PASS: all 12 negative tests accepted by the hardened controller")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

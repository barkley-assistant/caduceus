"""Self-tests for the deterministic review worker harness (caduceus
#327, plan T4).

The harness is the spawned-process seam for PR-review runs; these
tests drive it as a subprocess exactly the way the daemon (or the
bridge) would: environment in, exit code + result file out. Coverage:

* well-ordered prompt -> section-order assertion passes, schema
  version read from §2 and injected into the result document;
* reordered prompt -> harness fails (the determinism guarantee);
* missing section -> harness fails;
* every FAKE_REVIEW_RESULT value produces the expected exit code and
  on-disk shape (pass / fail / inconsistent / malformed / oversized);
* oversized writes a file exceeding MAX_REVIEW_RESULT_FILE_BYTES;
* prompt file is read, never modified.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
HARNESS = REPO_ROOT / "tests" / "fixtures" / "review_harness.py"

MAX_REVIEW_RESULT_FILE_BYTES = 4 << 20  # mirror of worker_contract.rs


def render_prompt(*sections: str) -> str:
    return "# caduceus review worker prompt\n\n" + "\n\n".join(sections) + "\n"


def full_prompt(schema_version: int = 1) -> str:
    return render_prompt(
        "## 1. Daemon instructions and review policy\n\nbehave",
        f"## 2. Output schema (ReviewResult v{schema_version})\n\nshape",
        "## 3. Pull request metadata (untrusted)\n\n```text\nmeta\n```",
        "## 4. Review diff over merge base (untrusted)\n\n```diff\n--- a\n+++ b\n```",
        "## 5. Repository context (untrusted)\n\n```text\nctx\n```",
        "## 6. PR discussion (untrusted)\n\n```text\nnone\n```",
    )


def run_harness(
    tmp_path: Path,
    prompt_text: str,
    fake_result: str | None = None,
    extra_env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[bytes]:
    prompt_path = tmp_path / "worker-prompt.md"
    prompt_path.write_text(prompt_text, encoding="utf-8")

    env = {
        **os.environ,
        "CADUCEUS_WORKTREE_PATH": str(tmp_path),
    }
    env.pop("CADUCEUS_RESULT_PATH", None)
    env.pop("CADUCEUS_PROMPT_PATH", None)
    if fake_result is not None:
        env["FAKE_REVIEW_RESULT"] = fake_result
    if extra_env:
        env.update(extra_env)

    return subprocess.run(
        [sys.executable, str(HARNESS)],
        env=env,
        capture_output=True,
        cwd=tmp_path,
        timeout=30,
    )


def read_result(tmp_path: Path) -> dict:
    return json.loads(
        (tmp_path / "worker-result.json").read_text(encoding="utf-8")
    )


# ---------------------------------------------------------------------------
# Determinism: section ordering + schema-version injection
# ---------------------------------------------------------------------------


def test_pass_result_injects_prompt_schema_version(tmp_path):
    proc = run_harness(tmp_path, full_prompt(schema_version=7), "pass")
    assert proc.returncode == 0, proc.stderr
    result = read_result(tmp_path)
    assert result["schema_version"] == 7
    assert result["status"] == "success"
    assert result["review"]["verdict"] == "pass"
    assert result["review"]["findings"] == []


def test_fail_result_is_verdict_consistent(tmp_path):
    proc = run_harness(tmp_path, full_prompt(), "fail")
    assert proc.returncode == 0, proc.stderr
    result = read_result(tmp_path)
    assert result["review"]["verdict"] == "fail"
    severities = [f["severity"] for f in result["review"]["findings"]]
    assert severities == ["blocking"], "FAIL must carry >=1 blocking finding"


def test_reordered_prompt_fails(tmp_path):
    reordered = render_prompt(
        "## 2. Output schema (ReviewResult v1)\n\nshape",
        "## 1. Daemon instructions and review policy\n\nbehave",
        "## 3. Pull request metadata (untrusted)\n\n```text\nmeta\n```",
        "## 4. Review diff over merge base (untrusted)\n\n```diff\n```",
        "## 5. Repository context (untrusted)\n\n```text\n```",
        "## 6. PR discussion (untrusted)\n\n```text\n```",
    )
    proc = run_harness(tmp_path, reordered, "pass")
    assert proc.returncode != 0
    assert "section order violated" in proc.stderr.decode()
    assert not (tmp_path / "worker-result.json").exists()


def test_missing_section_fails(tmp_path):
    proc = run_harness(tmp_path, full_prompt().replace("## 6. PR discussion (untrusted)\n\n```text\nnone\n```", ""), "pass")
    assert proc.returncode != 0
    assert "missing prompt section" in proc.stderr.decode()


def test_missing_schema_version_fails(tmp_path):
    # Keep the §2 heading prefix (so the section-order check passes)
    # but drop the version digit, isolating the schema-version read.
    prompt = full_prompt().replace("ReviewResult v1)", "ReviewResult v)")
    proc = run_harness(tmp_path, prompt, "pass")
    assert proc.returncode != 0
    assert "schema version not found" in proc.stderr.decode()


def test_harness_never_modifies_prompt(tmp_path):
    original = full_prompt()
    run_harness(tmp_path, original, "pass")
    assert (tmp_path / "worker-prompt.md").read_text(encoding="utf-8") == original


# ---------------------------------------------------------------------------
# Scripted result matrix
# ---------------------------------------------------------------------------


def test_inconsistent_result_is_fail_with_zero_blocking(tmp_path):
    proc = run_harness(tmp_path, full_prompt(), "inconsistent")
    assert proc.returncode == 0, proc.stderr
    result = read_result(tmp_path)
    assert result["review"]["verdict"] == "fail"
    assert result["review"]["findings"] == []


def test_malformed_result_writes_invalid_json_and_exits_two(tmp_path):
    proc = run_harness(tmp_path, full_prompt(), "malformed")
    assert proc.returncode == 2
    raw = (tmp_path / "worker-result.json").read_text(encoding="utf-8")
    with pytest.raises(json.JSONDecodeError):
        json.loads(raw)


def test_oversized_result_exceeds_daemon_read_cap(tmp_path):
    proc = run_harness(tmp_path, full_prompt(), "oversized")
    assert proc.returncode == 0, proc.stderr
    size = (tmp_path / "worker-result.json").stat().st_size
    assert size > MAX_REVIEW_RESULT_FILE_BYTES


def test_unknown_script_kind_fails(tmp_path):
    proc = run_harness(tmp_path, full_prompt(), "nonsense")
    assert proc.returncode != 0
    assert "unknown FAKE_REVIEW_RESULT" in proc.stderr.decode()


def test_result_path_override_is_honoured(tmp_path):
    out = tmp_path / "custom-result.json"
    proc = run_harness(
        tmp_path,
        full_prompt(),
        "pass",
        extra_env={"CADUCEUS_RESULT_PATH": str(out)},
    )
    assert proc.returncode == 0, proc.stderr
    assert json.loads(out.read_text(encoding="utf-8"))["review"]["verdict"] == "pass"


def test_missing_worktree_env_fails(tmp_path):
    env = {k: v for k, v in os.environ.items() if not k.startswith("CADUCEUS_")}
    proc = subprocess.run(
        [sys.executable, str(HARNESS)],
        env=env,
        capture_output=True,
        cwd=tmp_path,
        timeout=30,
    )
    assert proc.returncode != 0
    assert "CADUCEUS_WORKTREE_PATH" in proc.stderr.decode()

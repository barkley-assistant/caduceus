"""``hermes caduceus logs`` wrapper tests (issue #416).

The wrapper-owned ``logs`` subcommand surfaces the three diagnostic
files the daemon writes under the state dir — ``processor.log`` (the
JSON-lines daemon log), ``runs/<run_id>.log`` (a worker transcript),
and ``doctor.json`` (the stored readiness report) — without the
operator needing to know the layout by heart. These tests pin the
tail/follow defaults, the missing-file diagnostics and exit codes, the
redaction of credential-shaped lines, and the ``--json`` envelope.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from tests.fixtures.fake_ctx import FakePluginContext

RUN_ID = "01M0SCW70B1MQK81Y5NV36N1P4"
DOCTOR_REPORT = {"schema_version": 1, "verdict": "OK", "checks": []}


@pytest.fixture
def state_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Point the adapter's state-dir resolution at a temp directory."""
    directory = tmp_path / "caduceus-state"
    directory.mkdir()
    monkeypatch.setenv("CADUCEUS_STATE_DIR", str(directory))
    return directory


def _run_logs(adapter, fake_ctx: FakePluginContext, *argv: str) -> int:
    """Parse ``hermes caduceus logs <argv>`` and run the handler."""
    adapter.register(fake_ctx)
    parser = fake_ctx.cli_commands["caduceus"].parser
    args = parser.parse_args(["logs", *argv])
    return args.func(args)


def _write_processor_log(state_dir: Path, count: int = 60) -> Path:
    path = state_dir / "processor.log"
    path.write_text("".join(f"line {i}\n" for i in range(count)), encoding="utf-8")
    return path


def _write_transcript(state_dir: Path, body: str, run_id: str = RUN_ID) -> Path:
    runs = state_dir / "runs"
    runs.mkdir(exist_ok=True)
    path = runs / f"{run_id}.log"
    path.write_text(body, encoding="utf-8")
    return path


def _write_doctor_report(state_dir: Path, report: dict) -> Path:
    path = state_dir / "doctor.json"
    path.write_text(json.dumps(report), encoding="utf-8")
    return path


def test_logs_subcommand_exposes_its_flags(
    adapter, fake_ctx: FakePluginContext, capsys: pytest.CaptureFixture
) -> None:
    """``logs`` is registered and advertises the full flag contract."""
    adapter.register(fake_ctx)
    parser = fake_ctx.cli_commands["caduceus"].parser
    assert "logs" in parser.format_help()
    with pytest.raises(SystemExit) as exc:
        parser.parse_args(["logs", "--help"])
    assert exc.value.code == 0
    help_text = capsys.readouterr().out
    for flag in ("--follow", "--tail", "--run", "--doctor", "--json"):
        assert flag in help_text, f"missing {flag} in logs --help"


def test_logs_default_tails_processor_log(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    """The default invocation prints the last 50 processor.log lines."""
    _write_processor_log(state_dir, 60)
    rc = _run_logs(adapter, fake_ctx)
    captured = capsys.readouterr()
    lines = captured.out.splitlines()
    assert rc == 0
    assert captured.err == ""
    assert len(lines) == 50
    assert lines[0] == "line 10"
    assert lines[-1] == "line 59"


def test_logs_tail_n_prints_the_last_n_lines(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    _write_processor_log(state_dir, 60)
    rc = _run_logs(adapter, fake_ctx, "--tail", "10")
    captured = capsys.readouterr()
    lines = captured.out.splitlines()
    assert rc == 0
    assert len(lines) == 10
    assert lines[0] == "line 50"


def test_logs_tail_zero_prints_the_whole_file(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    _write_processor_log(state_dir, 60)
    rc = _run_logs(adapter, fake_ctx, "--tail", "0")
    captured = capsys.readouterr()
    assert rc == 0
    assert len(captured.out.splitlines()) == 60


def test_logs_run_prints_the_transcript(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    _write_transcript(state_dir, "worker said hello\nworker said bye\n")
    rc = _run_logs(adapter, fake_ctx, "--run", RUN_ID)
    captured = capsys.readouterr()
    assert rc == 0
    assert captured.out.splitlines() == ["worker said hello", "worker said bye"]
    assert captured.err == ""


def test_logs_run_honours_tail(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    _write_transcript(state_dir, "first\nsecond\nthird\n")
    rc = _run_logs(adapter, fake_ctx, "--run", RUN_ID, "--tail", "1")
    captured = capsys.readouterr()
    assert rc == 0
    assert captured.out.splitlines() == ["third"]


def test_logs_run_missing_transcript_errors(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    (state_dir / "runs").mkdir()
    rc = _run_logs(adapter, fake_ctx, "--run", "nope")
    captured = capsys.readouterr()
    assert rc == 1
    assert captured.out == ""
    assert "no transcript for run 'nope'" in captured.err


def test_logs_run_invalid_id_is_rejected(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    rc = _run_logs(adapter, fake_ctx, "--run", "../etc")
    captured = capsys.readouterr()
    assert rc == 2
    assert "invalid run id" in captured.err


def test_logs_doctor_pretty_prints_the_report(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    _write_doctor_report(state_dir, DOCTOR_REPORT)
    rc = _run_logs(adapter, fake_ctx, "--doctor")
    captured = capsys.readouterr()
    assert rc == 0
    assert json.loads(captured.out) == DOCTOR_REPORT
    assert captured.out.startswith("{\n")  # indented, not the raw one-liner
    assert '\n  "verdict"' in captured.out


def test_logs_doctor_missing_report_errors(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    rc = _run_logs(adapter, fake_ctx, "--doctor")
    captured = capsys.readouterr()
    assert rc == 1
    assert captured.out == ""
    assert "no doctor report at" in captured.err
    assert "hermes caduceus doctor" in captured.err


def test_logs_doctor_invalid_json_errors(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    (state_dir / "doctor.json").write_text("{not json", encoding="utf-8")
    rc = _run_logs(adapter, fake_ctx, "--doctor")
    captured = capsys.readouterr()
    assert rc == 1
    assert captured.out == ""
    assert "not valid JSON" in captured.err


def test_logs_missing_state_dir_errors(
    adapter,
    fake_ctx: FakePluginContext,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture,
) -> None:
    monkeypatch.setenv("CADUCEUS_STATE_DIR", str(tmp_path / "absent"))
    rc = _run_logs(adapter, fake_ctx)
    captured = capsys.readouterr()
    assert rc == 1
    assert "state dir not found" in captured.err
    assert "hermes caduceus setup" in captured.err


def test_logs_missing_processor_log_errors(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    rc = _run_logs(adapter, fake_ctx)
    captured = capsys.readouterr()
    assert rc == 1
    assert captured.out == ""
    assert "no daemon log at" in captured.err


def test_logs_redacts_credentials_in_log_bodies(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    (state_dir / "processor.log").write_text(
        "starting\nGITHUB_TOKEN=ghp_secret123\n", encoding="utf-8"
    )
    _write_transcript(state_dir, "GH_TOKEN=ghp_other456\n")
    rc = _run_logs(adapter, fake_ctx)
    captured = capsys.readouterr()
    assert rc == 0
    assert "GITHUB_TOKEN=<redacted>" in captured.out
    assert "ghp_secret123" not in captured.out
    rc = _run_logs(adapter, fake_ctx, "--run", RUN_ID)
    captured = capsys.readouterr()
    assert rc == 0
    assert "GH_TOKEN=<redacted>" in captured.out
    assert "ghp_other456" not in captured.out


def test_logs_json_envelope_for_streams(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    _write_processor_log(state_dir, 3)
    rc = _run_logs(adapter, fake_ctx, "--tail", "2", "--json")
    captured = capsys.readouterr()
    assert rc == 0
    envelope = json.loads(captured.out)
    assert envelope["source"] == "processor"
    assert envelope["path"].endswith("processor.log")
    assert envelope["lines"] == ["line 1", "line 2"]


def test_logs_json_envelope_for_run_and_doctor(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    _write_transcript(state_dir, "hello\n")
    _write_doctor_report(state_dir, DOCTOR_REPORT)
    rc = _run_logs(adapter, fake_ctx, "--run", RUN_ID, "--json")
    captured = capsys.readouterr()
    assert rc == 0
    envelope = json.loads(captured.out)
    assert envelope["source"] == f"run:{RUN_ID}"
    assert envelope["lines"] == ["hello"]
    rc = _run_logs(adapter, fake_ctx, "--doctor", "--json")
    captured = capsys.readouterr()
    assert rc == 0
    envelope = json.loads(captured.out)
    assert envelope["source"] == "doctor"
    assert envelope["report"] == DOCTOR_REPORT


def test_logs_run_and_doctor_are_mutually_exclusive(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    rc = _run_logs(adapter, fake_ctx, "--run", RUN_ID, "--doctor")
    captured = capsys.readouterr()
    assert rc == 2
    assert captured.out == ""
    assert "mutually exclusive" in captured.err


def test_logs_json_and_follow_are_rejected(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    rc = _run_logs(adapter, fake_ctx, "--json", "--follow")
    captured = capsys.readouterr()
    assert rc == 2
    assert "--json cannot be combined with --follow" in captured.err


def test_logs_negative_tail_is_rejected(
    adapter, fake_ctx: FakePluginContext, state_dir: Path, capsys: pytest.CaptureFixture
) -> None:
    rc = _run_logs(adapter, fake_ctx, "--tail", "-1")
    captured = capsys.readouterr()
    assert rc == 2
    assert "--tail must be >= 0" in captured.err


def test_logs_follow_seeds_then_streams_appended_lines(
    adapter,
    fake_ctx: FakePluginContext,
    state_dir: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture,
) -> None:
    """``--follow`` prints the seed tail, streams appends, exits 0 on Ctrl-C.

    The poll sleep is stubbed so the test never blocks: the first poll
    appends a line (which the loop must pick up), the second raises
    ``KeyboardInterrupt`` — the same signal Ctrl-C delivers to the
    operator's shell.
    """
    log = state_dir / "processor.log"
    log.write_text("seed 1\nseed 2\nseed 3\n", encoding="utf-8")
    polls = {"count": 0}

    def fake_sleep(seconds: float) -> None:
        polls["count"] += 1
        if polls["count"] == 1:
            with log.open("a", encoding="utf-8") as handle:
                handle.write("appended\n")
            return
        raise KeyboardInterrupt

    monkeypatch.setattr(adapter.time, "sleep", fake_sleep)
    rc = _run_logs(adapter, fake_ctx, "--follow", "--tail", "2")
    captured = capsys.readouterr()
    assert rc == 0
    assert captured.out.splitlines()[:3] == ["seed 2", "seed 3", "appended"]
    # Ctrl-C leaves the operator's prompt on a clean line.
    assert captured.out.endswith("appended\n\n")


def test_logs_derives_the_state_dir_from_hermes_home(
    adapter,
    fake_ctx: FakePluginContext,
    isolated_hermes_home: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture,
) -> None:
    """Without ``CADUCEUS_STATE_DIR`` the plugin uses ``$HERMES_HOME``."""
    monkeypatch.delenv("CADUCEUS_STATE_DIR", raising=False)
    state_dir = isolated_hermes_home / "caduceus-state"
    state_dir.mkdir()
    (state_dir / "processor.log").write_text("from hermes home\n", encoding="utf-8")
    rc = _run_logs(adapter, fake_ctx)
    captured = capsys.readouterr()
    assert rc == 0
    assert captured.out.splitlines() == ["from hermes home"]

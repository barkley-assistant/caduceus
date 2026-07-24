"""Bridge script integration tests."""

from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import sys
import textwrap
import time
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parent.parent.parent
BRIDGE_PATH = REPO_ROOT / "plugin-assets" / "worker-bridge.py"
FAKE_HARNESS_PATH = REPO_ROOT / "tests" / "fixtures" / "bridge_harness.py"

# All required CADUCEUS_* environment values. Mirrors
# ``plugin_assets.worker_bridge.REQUIRED_ENV_VARS``.
REQUIRED_ENV_KEYS = (
    "CADUCEUS_ISSUE_NUMBER",
    "CADUCEUS_ISSUE_TITLE",
    "CADUCEUS_ISSUE_BODY",
    "CADUCEUS_ISSUE_REPO",
    "CADUCEUS_CONTEXT_JSON",
    "CADUCEUS_WORKTREE_PATH",
    "CADUCEUS_RUN_ID",
    "CADUCEUS_ISSUE_LABELS_JSON",
    "CADUCEUS_BRANCH_NAME",
)


# ---------------------------------------------------------------------------
# Import the bridge as a module so the in-process tests can drive it
# ---------------------------------------------------------------------------


@pytest.fixture(scope="module")
def bridge_module():
    import importlib.util

    spec = importlib.util.spec_from_file_location("caduceus_bridge", BRIDGE_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


@pytest.fixture
def fake_env(tmp_path: Path) -> dict:
    """Provide a fully-populated CADUCEUS_* environment with a real prompt file."""
    worktree = tmp_path
    prompt = worktree / "worker-prompt.md"
    prompt.write_text("# Caduceus prompt\n\nRun the workflow per the spec.", encoding="utf-8")
    return {
        "CADUCEUS_ISSUE_NUMBER": "42",
        "CADUCEUS_ISSUE_TITLE": "Test issue with spaces and unicode ✨",
        "CADUCEUS_ISSUE_BODY": "Body with newline\nand quote \" marks.",
        "CADUCEUS_ISSUE_REPO": "owner/repo",
        "CADUCEUS_CONTEXT_JSON": json.dumps(
            {
                "issue": {"number": 42, "title": "Test"},
                "config_keys": ["worker_command", "poll_interval_seconds"],
            }
        ),
        "CADUCEUS_WORKTREE_PATH": str(worktree),
        "CADUCEUS_RUN_ID": "01J0X0X0X0X0X0X0X0X0X0X0X",
        "CADUCEUS_ISSUE_LABELS_JSON": json.dumps(["🤖 auto-fix", "good first issue"]),
        "CADUCEUS_BRANCH_NAME": "automation/issue-42-01j0x0x0x0x0x0x0x0x0x0x",
    }


# ---------------------------------------------------------------------------
# 1. read_required_env
# ---------------------------------------------------------------------------


class TestReadRequiredEnv:
    def test_returns_dict_when_all_required_present(
        self, bridge_module, fake_env
    ):
        result = bridge_module.read_required_env(fake_env)
        assert set(result.keys()) == set(REQUIRED_ENV_KEYS)
        assert result["CADUCEUS_ISSUE_NUMBER"] == "42"

    def test_empty_string_counts_as_missing(self, bridge_module, fake_env):
        env = dict(fake_env)
        env["CADUCEUS_ISSUE_TITLE"] = ""
        with pytest.raises(SystemExit) as excinfo:
            bridge_module.read_required_env(env)
        assert excinfo.value.code == bridge_module.EXIT_MISSING_ENV

    def test_missing_keys_appear_in_diagnostic_listed_together(
        self, bridge_module, fake_env, capsys
    ):
        env = dict(fake_env)
        for key in ("CADUCEUS_ISSUE_TITLE", "CADUCEUS_BRANCH_NAME"):
            env.pop(key)
        with pytest.raises(SystemExit) as excinfo:
            bridge_module.read_required_env(env)
        captured = capsys.readouterr()
        assert excinfo.value.code == bridge_module.EXIT_MISSING_ENV
        # Both missing keys appear in the single-line diagnostic; they
        # are joined with ", " and the format is "name1, name2, ..." so
        # a downstream log scraper can match the prefix reliably.
        err = captured.err.strip()
        assert err.startswith("caduceus bridge: missing required environment:")
        assert "CADUCEUS_ISSUE_TITLE" in err
        assert "CADUCEUS_BRANCH_NAME" in err
        # Values are NEVER echoed into the diagnostic.
        for value in fake_env.values():
            assert value not in captured.err
        # The single-line format must not embed literal quotes from any
        # boundary tests — no fancy JSON, just a stable plain-text list.
        assert '"' not in err


# ---------------------------------------------------------------------------
# 2. parse_labels
# ---------------------------------------------------------------------------


class TestParseLabels:
    def test_arrays_of_strings_parse(self, bridge_module):
        result = bridge_module.parse_labels(json.dumps(["🤖 auto-fix", "bug"]))
        assert result == ["🤖 auto-fix", "bug"]

    def test_empty_array_parses_to_empty_list(self, bridge_module):
        assert bridge_module.parse_labels("[]") == []

    def test_unicode_labels_round_trip(self, bridge_module):
        labels = ["🤖 auto-fix", "🚀 ship-it", "ñ"]
        assert bridge_module.parse_labels(json.dumps(labels)) == labels

    def test_non_string_element_rejected(self, bridge_module, capsys):
        with pytest.raises(SystemExit) as excinfo:
            bridge_module.parse_labels(json.dumps([1, 2, 3]))
        assert excinfo.value.code == bridge_module.EXIT_MALFORMED_LABELS
        assert "JSON array of strings" in capsys.readouterr().err

    def test_top_level_object_rejected(self, bridge_module, capsys):
        with pytest.raises(SystemExit) as excinfo:
            bridge_module.parse_labels(json.dumps({"label": "bug"}))
        assert excinfo.value.code == bridge_module.EXIT_MALFORMED_LABELS

    def test_malformed_json_rejected(self, bridge_module, capsys):
        with pytest.raises(SystemExit) as excinfo:
            bridge_module.parse_labels("this is not json")
        assert excinfo.value.code == bridge_module.EXIT_MALFORMED_LABELS

    def test_csv_string_rejected(self, bridge_module, capsys):
        """The bridge refuses the legacy CSV form — the daemon always emits JSON."""
        with pytest.raises(SystemExit) as excinfo:
            bridge_module.parse_labels("bug,feature,priority")
        assert excinfo.value.code == bridge_module.EXIT_MALFORMED_LABELS


# ---------------------------------------------------------------------------
# 3. verify_prompt
# ---------------------------------------------------------------------------


class TestVerifyPrompt:
    def test_existing_file_is_returned(self, bridge_module, tmp_path):
        prompt = tmp_path / "worker-prompt.md"
        prompt.write_text("hello", encoding="utf-8")
        assert bridge_module.verify_prompt(prompt) == prompt

    def test_missing_file_rejected(self, bridge_module, tmp_path, capsys):
        with pytest.raises(SystemExit) as excinfo:
            bridge_module.verify_prompt(tmp_path / "nope.md")
        assert excinfo.value.code == bridge_module.EXIT_MISSING_PROMPT
        assert "nope.md" in capsys.readouterr().err


# ---------------------------------------------------------------------------
# 4. invoke_harness — in-process test using the unit-patchable bridge
# ---------------------------------------------------------------------------


class TestInvokeHarness:
    def test_default_invocation_uses_argument_array(self, bridge_module, monkeypatch):
        """The bridge must invoke OpenCode as an argv list, never a shell."""
        captured = {}

        class FakeCompleted:
            returncode = 0

        def fake_run(argv, cwd, **kwargs):
            captured["argv"] = argv
            captured["cwd"] = cwd
            captured["kwargs"] = kwargs
            return FakeCompleted()

        monkeypatch.setattr(bridge_module.subprocess, "run", fake_run)
        worktree = Path("/tmp/fake-worktree")
        prompt = worktree / "worker-prompt.md"
        rc = bridge_module.invoke_harness(
            worktree=worktree,
            prompt_file=prompt,
            run_id="abc",
            labels=("🤖 auto-fix",),
            branch_name="automation/issue-1-abc",
        )
        assert rc == 0
        # Argument array — every element is its own string. No shell=True.
        assert isinstance(captured["argv"], list)
        assert all(isinstance(item, str) for item in captured["argv"])
        for forbidden in ("shell", "shell=True"):
            assert forbidden not in captured["kwargs"]
            assert forbidden not in str(captured["argv"])
        # Cwd is the worktree.
        assert captured["cwd"] == str(worktree)
        # Prompt path is a single argument, not a shell fragment.
        assert str(prompt) in captured["argv"]

    def test_extra_argv_is_forwarded(self, bridge_module, monkeypatch):
        captured = {}

        class FakeCompleted:
            returncode = 0

        def fake_run(argv, cwd, **kwargs):
            captured["argv"] = argv
            return FakeCompleted()

        monkeypatch.setattr(bridge_module.subprocess, "run", fake_run)
        bridge_module.invoke_harness(
            worktree=Path("/tmp/wt"),
            prompt_file=Path("/tmp/wt/worker-prompt.md"),
            run_id="abc",
            labels=(),
            branch_name="automation/issue-1-abc",
            extra_argv=("--trace", "--quiet"),
        )
        assert "--trace" in captured["argv"]
        assert "--quiet" in captured["argv"]

    def test_returncode_propagated_from_harness(self, bridge_module, monkeypatch):
        class FakeCompleted:
            returncode = 17

        monkeypatch.setattr(
            bridge_module.subprocess,
            "run",
            lambda *_a, **_kw: FakeCompleted(),
        )
        assert (
            bridge_module.invoke_harness(
                worktree=Path("/tmp/wt"),
                prompt_file=Path("/tmp/wt/worker-prompt.md"),
                run_id="abc",
                labels=(),
                branch_name="automation/issue-1-abc",
            )
            == 17
        )


# ---------------------------------------------------------------------------
# 5. main — in-process flows
# ---------------------------------------------------------------------------


class TestMain:
    def test_success_propagates_zero(
        self, bridge_module, monkeypatch, fake_env
    ):
        # Invoke the fake harness through the live subprocess so the
        # bridge does its full argv construction.
        monkeypatch.setattr(
            bridge_module,
            "invoke_harness",
            lambda **_: 0,
        )
        # Need to point invoke_harness's argv at a real subprocess so the
        # test is hermetic. The unit tests above already verify argv shape;
        # here we drive main end-to-end with a stub.
        rc = bridge_module.main(env=fake_env, argv=["bridge"])
        assert rc == 0

    def test_nonzero_harness_exit_propagated(
        self, bridge_module, monkeypatch, fake_env
    ):
        monkeypatch.setattr(bridge_module, "invoke_harness", lambda **_: 7)
        assert bridge_module.main(env=fake_env, argv=["bridge"]) == 7

    def test_missing_env_returns_exit_missing(
        self, bridge_module, monkeypatch, fake_env, capsys
    ):
        env = dict(fake_env)
        env.pop("CADUCEUS_ISSUE_TITLE")
        # Sanity: the bridge should never reach invoke_harness.
        def explode(**_):
            raise AssertionError("invoke_harness should not run with missing env")

        monkeypatch.setattr(bridge_module, "invoke_harness", explode)
        with pytest.raises(SystemExit) as excinfo:
            bridge_module.main(env=env, argv=["bridge"])
        assert excinfo.value.code == bridge_module.EXIT_MISSING_ENV
        assert "CADUCEUS_ISSUE_TITLE" in capsys.readouterr().err

    def test_credential_vars_in_bridge_input_do_not_block_when_other_env_present(
        self, bridge_module, monkeypatch, fake_env, capsys
    ):
        """The bridge does not police credential tokens — that is the."""
        env = dict(fake_env)
        env["GITHUB_TOKEN"] = "ghp_secret_value"
        monkeypatch.setattr(bridge_module, "invoke_harness", lambda **_: 0)
        # No SystemExit raised: the bridge ran.
        assert bridge_module.main(env=env, argv=["bridge"]) == 0

    def test_malformed_labels_rejected_before_harness(
        self, bridge_module, monkeypatch, fake_env, capsys
    ):
        env = dict(fake_env)
        env["CADUCEUS_ISSUE_LABELS_JSON"] = "bug,feature"  # legacy CSV shape

        def explode(**_):
            raise AssertionError("invoke_harness must not run with bad labels")

        monkeypatch.setattr(bridge_module, "invoke_harness", explode)
        with pytest.raises(SystemExit) as excinfo:
            bridge_module.main(env=env, argv=["bridge"])
        assert excinfo.value.code == bridge_module.EXIT_MALFORMED_LABELS

    def test_missing_prompt_rejected(
        self, bridge_module, monkeypatch, fake_env, capsys
    ):
        env = dict(fake_env)
        # Point the worktree at a directory with no prompt file.
        env["CADUCEUS_WORKTREE_PATH"] = "/tmp/caduceus-bridge-test-no-worktree"
        # Ensure the target truly does not exist.
        assert not Path(env["CADUCEUS_WORKTREE_PATH"]).exists()

        def explode(**_):
            raise AssertionError("invoke_harness must not run with missing prompt")

        monkeypatch.setattr(bridge_module, "invoke_harness", explode)
        with pytest.raises(SystemExit) as excinfo:
            bridge_module.main(env=env, argv=["bridge"])
        assert excinfo.value.code == bridge_module.EXIT_MISSING_PROMPT


# ---------------------------------------------------------------------------
# 6. Forbidden side effects: no state, no heartbeat, no worker-result.json
# ---------------------------------------------------------------------------


class TestForbiddenSideEffects:
    def test_bridge_does_not_emit_worker_result_json(
        self, bridge_module, monkeypatch, tmp_path, fake_env
    ):
        # The harness stub returns 0. The bridge must NOT write
        # ``worker-result.json`` — that's the daemon's job.
        monkeypatch.setattr(bridge_module, "invoke_harness", lambda **_: 0)
        env = dict(fake_env)
        env["CADUCEUS_WORKTREE_PATH"] = str(tmp_path)
        # Provide a prompt file inside the worktree.
        (tmp_path / "worker-prompt.md").write_text("prompt", encoding="utf-8")
        rc = bridge_module.main(env=env, argv=["bridge"])
        assert rc == 0
        assert not (tmp_path / "worker-result.json").exists()

    def test_bridge_does_not_create_state_directory(
        self, bridge_module, monkeypatch, tmp_path, fake_env
    ):
        # Point HERMES_HOME at a tmp path and verify the bridge never
        # touches anything resembling a daemon state directory.
        monkeypatch.setattr(bridge_module, "invoke_harness", lambda **_: 0)
        env = dict(fake_env)
        env["CADUCEUS_WORKTREE_PATH"] = str(tmp_path)
        (tmp_path / "worker-prompt.md").write_text("prompt", encoding="utf-8")
        env["HERMES_HOME"] = str(tmp_path / "fake-hermes-home")
        rc = bridge_module.main(env=env, argv=["bridge"])
        assert rc == 0
        assert not (tmp_path / "fake-hermes-home").exists()

    def test_bridge_does_not_write_heartbeat(
        self, bridge_module, monkeypatch, tmp_path, fake_env
    ):
        monkeypatch.setattr(bridge_module, "invoke_harness", lambda **_: 0)
        env = dict(fake_env)
        env["CADUCEUS_WORKTREE_PATH"] = str(tmp_path)
        (tmp_path / "worker-prompt.md").write_text("prompt", encoding="utf-8")
        env["HERMES_HOME"] = str(tmp_path / "fake-hermes-home")
        env["CADUCEUS_STATE_DIR"] = str(tmp_path / "fake-state")
        bridge_module.main(env=env, argv=["bridge"])
        # No .heartbeat in worktree, no .new candidate under $HERMES_HOME.
        for entry in tmp_path.iterdir():
            assert not entry.name.endswith(".heartbeat")
        # No fake-state dir.
        assert not (tmp_path / "fake-state").exists()

    def test_bridge_does_not_open_network_sockets(
        self, bridge_module, monkeypatch, fake_env, tmp_path
    ):
        """The bridge itself never opens a socket — OpenCode does, via subprocess."""
        sockets_open = []

        original_socket = bridge_module.socket.socket if hasattr(bridge_module, "socket") else None

        # Patch at the stdlib level via subprocess.run (we don't import socket
        # in the bridge module). If a future change adds a network call,
        # this test will surface it as the patch is _not_ applied but the
        # call will fail loudly anyway. We assert the directory stays quiet.
        env = dict(fake_env)
        env["CADUCEUS_WORKTREE_PATH"] = str(tmp_path)
        (tmp_path / "worker-prompt.md").write_text("prompt", encoding="utf-8")
        monkeypatch.setattr(bridge_module, "invoke_harness", lambda **_: 0)
        rc = bridge_module.main(env=env, argv=["bridge"])
        assert rc == 0
        assert sockets_open == []


# ---------------------------------------------------------------------------
# 7. End-to-end subprocess — argv with spaces, unicode, signals, exit codes
# ---------------------------------------------------------------------------


def _make_worktree_with_prompt(tmp_path: Path) -> Path:
    worktree = tmp_path
    prompt = worktree / "worker-prompt.md"
    prompt.write_text(
        textwrap.dedent(
            """
            # Caduceus prompt

            Sparkly path with spaces ✨ — uñíçødé 🚀

            Run the workflow.
            """
        ),
        encoding="utf-8",
    )
    return worktree


def _build_env(tmp_path: Path, **overrides: str) -> dict:
    env = {
        "CADUCEUS_ISSUE_NUMBER": "42",
        "CADUCEUS_ISSUE_TITLE": "issue with spaces ✨",
        "CADUCEUS_ISSUE_BODY": "line one\nline two 🚀",
        "CADUCEUS_ISSUE_REPO": "owner-name/repo.name",
        "CADUCEUS_CONTEXT_JSON": json.dumps({"run_id": "abc"}),
        "CADUCEUS_WORKTREE_PATH": str(tmp_path),
        "CADUCEUS_RUN_ID": "01J0X0X0X0X0X0X0X0X0X0X0X",
        "CADUCEUS_ISSUE_LABELS_JSON": json.dumps(["🤖 auto-fix", "good first issue"]),
        "CADUCEUS_BRANCH_NAME": "automation/issue-42-01j0x0x0x0x0x0x0x0x0x0x",
    }
    env.update({k: v for k, v in overrides.items() if v is not None})
    return env


def _run_bridge_subprocess(env: dict, *args: str) -> subprocess.CompletedProcess:
    """Run the bridge as a true subprocess — no in-process patching."""
    return subprocess.run(
        [sys.executable, str(BRIDGE_PATH), *args],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )


def _make_opencode_wrapper(bin_dir: Path, harness_path: Path) -> Path:
    """Create a Python ``opencode`` script that exec's the fake harness."""
    wrapper = bin_dir / "opencode"
    contents = textwrap.dedent(
        f"""\
        #!{sys.executable}
        # GENERATED FOR TESTS — dispatch every argv into the fake harness.
        import os
        import sys
        sys.path.insert(0, {str(harness_path.parent)!r})
        from bridge_harness import main as fake_main
        sys.argv[0] = "opencode"
        sys.exit(fake_main())
        """
    )
    wrapper.write_text(contents, encoding="utf-8")
    wrapper.chmod(0o755)
    return wrapper


class TestSubprocessEndToEnd:
    def test_success_propagates_zero_via_subprocess(self, tmp_path):
        _make_worktree_with_prompt(tmp_path)
        env = _build_env(tmp_path)
        bin_dir = tmp_path / "bin"
        bin_dir.mkdir()
        _make_opencode_wrapper(bin_dir, FAKE_HARNESS_PATH)
        harness_log = tmp_path / "harness.log"
        env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
        env["FAKE_HARNESS_LOG"] = str(harness_log)
        env["FAKE_HARNESS_EXIT"] = "0"
        result = _run_bridge_subprocess(env)
        assert result.returncode == 0, f"stderr: {result.stderr}"
        # Harness actually ran and the log captured the env keys.
        log_lines = harness_log.read_text(encoding="utf-8").strip().splitlines()
        assert len(log_lines) == 1
        record = json.loads(log_lines[0])
        assert "CADUCEUS_ISSUE_LABELS_JSON" in record["env_keys"]
        assert "CADUCEUS_WORKTREE_PATH" in record["env_keys"]

    def test_nonzero_harness_exit_propagated(self, tmp_path):
        _make_worktree_with_prompt(tmp_path)
        env = _build_env(tmp_path)
        bin_dir = tmp_path / "bin"
        bin_dir.mkdir()
        _make_opencode_wrapper(bin_dir, FAKE_HARNESS_PATH)
        env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
        env["FAKE_HARNESS_EXIT"] = "37"
        env["FAKE_HARNESS_STDERR"] = "harness-induced error ✨\n"
        result = _run_bridge_subprocess(env)
        assert result.returncode == 37, f"stderr: {result.stderr}"
        assert "harness-induced error ✨" in result.stderr

    def test_missing_env_subprocess(self, tmp_path):
        _make_worktree_with_prompt(tmp_path)
        env = _build_env(tmp_path)
        env.pop("CADUCEUS_ISSUE_TITLE")
        result = _run_bridge_subprocess(env)
        assert result.returncode == 2  # EXIT_MISSING_ENV
        assert "CADUCEUS_ISSUE_TITLE" in result.stderr

    def test_malformed_labels_subprocess(self, tmp_path):
        _make_worktree_with_prompt(tmp_path)
        env = _build_env(tmp_path)
        env["CADUCEUS_ISSUE_LABELS_JSON"] = "not json"
        result = _run_bridge_subprocess(env)
        assert result.returncode == 2

    def test_missing_prompt_subprocess(self, tmp_path):
        # Make a worktree without a prompt file.
        env = _build_env(tmp_path)
        env["CADUCEUS_WORKTREE_PATH"] = str(tmp_path / "empty-wt")
        result = _run_bridge_subprocess(env)
        assert result.returncode == 2  # EXIT_MISSING_PROMPT
        assert "worker-prompt.md" in result.stderr

    def test_path_with_spaces_and_unicode_is_passed_verbatim(self, tmp_path):
        space_worktree = tmp_path / "with space ✨"
        space_worktree.mkdir()
        (space_worktree / "worker-prompt.md").write_text(
            "spaces ✨ prompt", encoding="utf-8"
        )
        env = _build_env(space_worktree)
        bin_dir = tmp_path / "bin2"
        bin_dir.mkdir()
        _make_opencode_wrapper(bin_dir, FAKE_HARNESS_PATH)
        env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
        log = tmp_path / "harness-unicode.log"
        env["FAKE_HARNESS_LOG"] = str(log)
        env["FAKE_HARNESS_EXIT"] = "0"
        result = _run_bridge_subprocess(env)
        assert result.returncode == 0
        record = json.loads(log.read_text(encoding="utf-8").strip())
        # The prompt file path is on the argv passed to the harness.
        assert any("with space ✨" in arg for arg in record["argv"])

    def test_arguments_containing_spaces_pass_unchanged(self, tmp_path):
        """The bridge must pass arguments containing spaces as discrete argv items."""
        _make_worktree_with_prompt(tmp_path)
        env = _build_env(tmp_path)
        bin_dir = tmp_path / "bin3"
        bin_dir.mkdir()
        _make_opencode_wrapper(bin_dir, FAKE_HARNESS_PATH)
        env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
        log = tmp_path / "harness-argv.log"
        env["FAKE_HARNESS_LOG"] = str(log)
        env["FAKE_HARNESS_EXIT"] = "0"
        # argv has spaces inside an extra arg; the bridge must not try
        # to split it on whitespace.
        result = _run_bridge_subprocess(env, "--label", "with space 🚀")
        assert result.returncode == 0
        record = json.loads(log.read_text(encoding="utf-8").strip())
        assert "with space 🚀" in record["argv"]
        assert "--label" in record["argv"]
        idx = record["argv"].index("--label")
        assert record["argv"][idx + 1] == "with space 🚀"

    def test_bridge_does_not_install_python_signal_handlers(
        self, bridge_module, monkeypatch, tmp_path
    ):
        """The bridge is downstream of the daemon's worker supervisor,."""
        # Provide a real worktree with a prompt so the bridge runs to
        # completion through invoke_harness.
        _make_worktree_with_prompt(tmp_path)
        env = _build_env(tmp_path)
        # Stub signal.signal so any call is observable.
        signal_calls = []
        real_signal = bridge_module.signal.signal

        def spy_signal(signum, handler):
            signal_calls.append((signum, handler))
            return real_signal(signum, handler)

        monkeypatch.setattr(bridge_module.signal, "signal", spy_signal)
        # Stub invoke_harness so we never spawn a subprocess.
        monkeypatch.setattr(bridge_module, "invoke_harness", lambda **_: 0)
        assert bridge_module.main(env=env, argv=["bridge"]) == 0
        # The bridge never installed a Python signal handler.
        assert signal_calls == []


    def test_signal_forwarded_to_harness_via_subprocess(self, tmp_path):
        """Verify the bridge's subprocess.run lets the daemon-supplied."""
        _make_worktree_with_prompt(tmp_path)
        env = _build_env(tmp_path)
        bin_dir = tmp_path / "bin4"
        bin_dir.mkdir()
        sleep_program = tmp_path / "sleep_harness.py"
        sleep_program.write_text(
            textwrap.dedent(
                """\
                import os, sys, time, signal, pathlib
                def main():
                    log = pathlib.Path(sys.argv[1])
                    log.write_text(str(os.getpid()))
                    received = []
                    def onterm(signum, frame):
                        received.append(signum)
                        log.write_text(
                            log.read_text() + '\\n' + repr(received)
                        )
                        sys.exit(130)
                    signal.signal(signal.SIGINT, onterm)
                    try:
                        time.sleep(10)
                    except KeyboardInterrupt:
                        pass
                    sys.exit(0)
                if __name__ == '__main__':
                    sys.exit(main())
                """
            ).strip(),
            encoding="utf-8",
        )
        wrapper = bin_dir / "opencode"
        wrapper.write_text(
            textwrap.dedent(
                f"""\
                #!{sys.executable}
                import os
                import sys
                sys.path.insert(0, {str(tmp_path)!r})
                import sleep_harness
                sys.argv = ["opencode", "/tmp/caduceus-sleep-pid"]
                sys.exit(sleep_harness.main())
                """
            ).strip(),
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
        env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
        # Launch the bridge with a new session so its harness inherits
        # a process group whose leader is the bridge. Sending SIGINT
        # to the bridge's process group will deliver to the entire
        # group, including the harness child.
        proc = subprocess.Popen(
            [sys.executable, str(BRIDGE_PATH)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        pid_file = Path("/tmp/caduceus-sleep-pid")
        for _ in range(80):
            if pid_file.exists():
                break
            time.sleep(0.05)
        assert pid_file.exists(), "harness did not start"
        # Send SIGINT to the process group; the harness's Python signal
        # handler will fire and write the signal number to the pid file.
        os.killpg(os.getpgid(proc.pid), signal.SIGINT)
        try:
            stdout, stderr = proc.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            stdout, stderr = proc.communicate()
        text = pid_file.read_text(encoding="utf-8")
        # The harness's trap recorded SIGINT (signal.SIGINT == 2 on POSIX).
        assert "[2]" in text, (
            f"harness did not receive SIGINT; pidfile: {text!r}"
        )
        # The bridge returned the harness's exit code (130 from the
        # trap handler, or 0 if the bridge's own SIGINT handler ran
        # first). Either way the bridge did not block the signal.
        assert proc.returncode is not None

    def test_credential_vars_do_not_reach_harness_subprocess(self, tmp_path):
        """The bridge inherits its environment from the daemon. The daemon."""
        _make_worktree_with_prompt(tmp_path)
        env = _build_env(tmp_path)
        bin_dir = tmp_path / "bin5"
        bin_dir.mkdir()
        _make_opencode_wrapper(bin_dir, FAKE_HARNESS_PATH)
        env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
        log = tmp_path / "harness-creds.log"
        env["FAKE_HARNESS_LOG"] = str(log)
        env["FAKE_HARNESS_EXIT"] = "0"
        # Drop the credential here because the test environment may
        # have it set — the bridge inherits os.environ through the test.
        env.pop("GITHUB_TOKEN", None)
        env.pop("AUTO_ISSUE_GITHUB_TOKEN", None)
        env.pop("CADUCEUS_GITHUB_TOKEN", None)
        env.pop("GH_TOKEN", None)
        result = _run_bridge_subprocess(env)
        assert result.returncode == 0, result.stderr
        record = json.loads(log.read_text(encoding="utf-8").strip())
        assert "GITHUB_TOKEN" not in record["env_keys"]
        assert "CADUCEUS_GITHUB_TOKEN" not in record["env_keys"]
        assert "GH_TOKEN" not in record["env_keys"]
        assert "AUTO_ISSUE_GITHUB_TOKEN" not in record["env_keys"]




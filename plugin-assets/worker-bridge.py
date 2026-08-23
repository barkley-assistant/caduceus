#!/usr/bin/env python3
"""Caduceus harness bridge — canonical reference implementation.

This file is the parent runner that ``hermes caduceus setup`` seeds at
``$HERMES_HOME/caduceus/worker-bridge.py``. A user-owned
``$HERMES_HOME/caduceus/harness.py`` can provide the small ``run_task(ctx)``
hook; the parent validates the daemon inputs, loads that hook, runs its argv,
and synthesizes a result when the argv did not write one. If no hook exists,
the bridge preserves the legacy full-script path.

The bridge never touches the daemon state directory, never reads or writes a
heartbeat, and never claims, queues, or finalizes anything. All of that work
lives in the Rust core.

Credential hygiene
------------------

The bridge never holds a ``GITHUB_TOKEN`` / ``CADUCEUS_GITHUB_TOKEN`` /
``GH_TOKEN`` / ``AUTO_ISSUE_GITHUB_TOKEN`` value in its own environment
because the daemon strips them before launch. The bridge does *not*
re-check the parent environment for these tokens; doing so would
incorrectly refuse launches in any operator environment that keeps
such tokens in their shell (which is the common pattern). The daemon's
``DENIED_ENV_VARS`` is the only source of truth for credential hygiene,
and it runs **before** the bridge starts.

Harness selection
-----------------

The reference hook returns OpenCode with the gentle-orchestrator agent. To
swap harnesses, copy ``plugin-assets/caduceus_harness.py.example`` to
``$HERMES_HOME/caduceus/harness.py`` and edit its ``run_task`` function.
Existing user-owned full bridge scripts remain valid when the hook is absent.

Forbidden side effects
----------------------

* No writes under ``$HERMES_HOME/caduceus-state`` (the state directory).
* No ``<state_dir>/runs/*.heartbeat`` or ``<worktree>/.heartbeat``
  creation — heartbeats are owned by the Rust supervisor.
* No daemon config / queue / state mutations.
* No ``<worktree>/worker-result.json`` — the daemon reads that file
  after the worker exits and uses it for finalization.

If this file is edited to violate any of these contracts, the
``tests/bridge_test.py`` suite will fail.

Forwarding signals
------------------

Subprocesses inherit the daemon's signal plan already — Caduceus puts
the worker in a new Unix session and forwards SIGINT/SIGTERM/timeout to
the whole process group. The bridge therefore does not trap signals of
its own; raising ``KeyboardInterrupt`` or letting the harness die on a
delivered signal is the correct behavior. The Python test suite pins
this explicitly (``test_signal_is_forwarded_to_harness``).
"""

import importlib.util
import json
import os
import signal  # noqa: F401 - preserved as a public compatibility symbol
import subprocess
import sys
import unicodedata
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from types import MappingProxyType

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

#: Required ``CADUCEUS_*`` environment variables. The daemon exports every
#: one of these for the worker; a missing entry means the daemon is not
#: talking to a current bridge.
REQUIRED_ENV_VARS: tuple[str, ...] = (
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

#: File names inside the worktree the daemon prepares. The bridge never
#: reads ``worker-result.json`` (the daemon reads it after the worker
#: exits) — but it does verify the prompt is on disk before exec.
PROMPT_FILE_NAME = "worker-prompt.md"

#: Exit codes that the bridge maps onto the daemon's worker interface.
EXIT_OK = 0
EXIT_MISSING_ENV = 2
EXIT_MALFORMED_LABELS = 2
EXIT_MISSING_PROMPT = 2
EXIT_HARNESS_NOT_FOUND = 127
EXIT_HARNESS_UNREACHABLE = 126
EXIT_HARNESS_CRASH = 125

# Result-file limits mirror ``src/worker/worker_contract.rs``.
MAX_RESULT_FILE_BYTES = 1 << 20
MAX_SUMMARY_BYTES = 64 * 1024
MAX_PULL_REQUEST_TITLE_CHARS = 256

#: Patterns the bridge uses. The daemon is the source of truth for
#: credential hygiene (see ``DENIED_ENV_VARS`` in the Rust core); the
#: bridge does **not** re-check the parent environment for credential
#: tokens, otherwise any operator with a ``GITHUB_TOKEN`` in their
#: shell would see the bridge refuse to start. The constants below are
#: kept as documentation only.
_DOCUMENTED_DENIED_VARS = frozenset(
    {
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "CADUCEUS_GITHUB_TOKEN",
        "AUTO_ISSUE_GITHUB_TOKEN",
    }
)
del _DOCUMENTED_DENIED_VARS  # documented above; not enforced here


# ---------------------------------------------------------------------------
# Parent runner types and helpers
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Issue:
    """Issue details exposed to a user harness through :class:`Ctx`."""

    repo: str
    number: int
    title: str
    body: str


@dataclass(frozen=True)
class Ctx:
    """The explicit worker context passed to ``run_task``."""

    worktree: Path
    prompt: Path
    run_id: str
    branch: str
    labels: list[str]
    issue: Issue
    context_json: str
    dry_run: bool
    env: Mapping[str, str]


def _hermes_home(env: Mapping[str, str] | None = None) -> Path:
    """Resolve ``HERMES_HOME`` without consulting a fallback first."""
    source = os.environ if env is None else env
    raw = source.get("HERMES_HOME")
    if raw:
        return Path(raw).expanduser().resolve()
    return Path("~/.hermes").expanduser().resolve()


def _hook_path(env: Mapping[str, str] | None = None) -> Path | None:
    """Return the opt-in user hook path when it is a regular file."""
    path = _hermes_home(env) / "caduceus" / "harness.py"
    return path if path.is_file() else None


def _legacy_bridge_path(env: Mapping[str, str] | None = None) -> Path:
    """Return the user-owned full bridge path used by the legacy path."""
    return _hermes_home(env) / "caduceus" / "worker-bridge.py"


def _build_ctx(env: Mapping[str, str]) -> Ctx:
    """Validate the daemon environment and construct the hook context."""
    values = read_required_env(env)
    labels = parse_labels(values["CADUCEUS_ISSUE_LABELS_JSON"])
    worktree = resolve_worktree(env).expanduser().resolve()
    prompt = verify_prompt(worktree / PROMPT_FILE_NAME)
    try:
        issue_number = int(values["CADUCEUS_ISSUE_NUMBER"])
    except ValueError as exc:
        print(
            "caduceus bridge: invalid CADUCEUS_ISSUE_NUMBER: "
            f"{exc}",
            file=sys.stderr,
        )
        sys.exit(EXIT_MISSING_ENV)
    dry_run = env.get("CADUCEUS_DRY_RUN", "").lower() in {
        "1",
        "true",
        "yes",
        "on",
    }
    return Ctx(
        worktree=worktree,
        prompt=prompt,
        run_id=values["CADUCEUS_RUN_ID"],
        branch=values["CADUCEUS_BRANCH_NAME"],
        labels=labels,
        issue=Issue(
            repo=values["CADUCEUS_ISSUE_REPO"],
            number=issue_number,
            title=values["CADUCEUS_ISSUE_TITLE"],
            body=values["CADUCEUS_ISSUE_BODY"],
        ),
        context_json=values["CADUCEUS_CONTEXT_JSON"],
        dry_run=dry_run,
        env=MappingProxyType(dict(env)),
    )


def _load_hook(hook_path: Path) -> Callable[[Ctx], Sequence[str]]:
    """Load and validate ``run_task`` from a user-owned hook module."""
    spec = importlib.util.spec_from_file_location("caduceus_harness", hook_path)
    if spec is None or spec.loader is None:
        raise ImportError(f"unable to load harness module from {hook_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    hook = module.run_task
    if not callable(hook):
        raise TypeError("run_task is not callable")
    return hook


def _exception_line(prefix: str, exc: Exception) -> str:
    """Format a one-line diagnostic without allowing hook output to break it."""
    message = " ".join(str(exc).split()) or "no message"
    return f"caduceus bridge: {prefix}: {type(exc).__name__}: {message}\n"


def _invoke_hook(
    hook: Callable[[Ctx], Sequence[str]], ctx: Ctx
) -> Sequence[str] | int:
    """Invoke a hook, mapping hook and contract errors to exit 125."""
    try:
        argv = hook(ctx)
        if not isinstance(argv, list) or not all(
            isinstance(argument, str) for argument in argv
        ):
            raise TypeError("run_task must return list[str]")
        if not argv:
            raise ValueError("run_task returned an empty argv")
        return argv
    except Exception as exc:  # noqa: BLE001 - hook failures map to exit 125
        sys.stderr.write(_exception_line("harness hook crashed", exc))
        return EXIT_HARNESS_CRASH


def _execute_argv(argv: Sequence[str], ctx: Ctx) -> tuple[int, str, str]:
    """Execute hook argv and return its exit code and captured output."""
    try:
        completed = subprocess.run(
            argv,
            cwd=str(ctx.worktree),
            env=ctx.env,
            capture_output=True,
            text=True,
            check=False,
        )
        return completed.returncode, completed.stdout or "", completed.stderr or ""
    except FileNotFoundError as exc:
        program = exc.filename or argv[0]
        return (
            EXIT_HARNESS_NOT_FOUND,
            "",
            f"caduceus bridge: harness executable not found: {program}\n",
        )
    except OSError as exc:
        return (
            EXIT_HARNESS_UNREACHABLE,
            "",
            f"caduceus bridge: unable to start harness: {exc}\n",
        )


def _clean_terminal_text(value: str) -> str:
    """Remove NUL and control characters while retaining newlines."""
    return "".join(
        character
        for character in value
        if character == "\n" or unicodedata.category(character) != "Cc"
    )


def _clamp_utf8(value: str, limit: int) -> str:
    """Clamp a string by UTF-8 byte length without splitting a code point."""
    encoded = value.encode("utf-8")
    if len(encoded) <= limit:
        return value
    return encoded[:limit].decode("utf-8", errors="ignore")


def truncate_pull_request_title(title: str) -> str:
    """Port Rust's char-counted title truncation exactly."""
    if len(title) <= MAX_PULL_REQUEST_TITLE_CHARS:
        return title
    return title[: MAX_PULL_REQUEST_TITLE_CHARS - 1] + "…"


def _result_path(worktree: Path) -> Path:
    """Return the daemon-consumed result path for a worktree."""
    return worktree / "worker-result.json"


def _synthesize_worker_result(
    worktree: Path, returncode: int, stdout: str, stderr: str
) -> None:
    """Atomically synthesize a schema-shaped result when none was written.

    A sibling temporary file plus ``os.replace`` keeps the daemon from seeing
    a partially written JSON document. Direct writes are an explicit bypass;
    an existing result file is therefore left untouched.
    """
    result_path = _result_path(worktree)
    if result_path.exists():
        return

    combined = _clean_terminal_text((stdout or "") + (stderr or ""))
    lines = [line.strip() for line in combined.split("\n") if line.strip()]
    placeholder = "Harness produced no terminal output."
    first_line = lines[0] if lines else placeholder
    last_line = lines[-1] if lines else placeholder
    summary = _clamp_utf8(combined, MAX_SUMMARY_BYTES) or placeholder
    payload = {
        "status": "success" if returncode == EXIT_OK else "failure",
        "summary": summary,
        "commit_message": last_line,
        "pull_request_title": truncate_pull_request_title(first_line),
        "artifacts": {},
        "investigation": False,
    }
    temporary_path = result_path.with_name(result_path.name + ".tmp")
    temporary_path.write_text(
        json.dumps(payload, ensure_ascii=False),
        encoding="utf-8",
    )
    os.replace(temporary_path, result_path)


# ---------------------------------------------------------------------------
# Legacy harness shim — preserved for existing full bridge copies
# ---------------------------------------------------------------------------


def invoke_harness(
    worktree: Path,
    prompt_file: Path,
    run_id: str,
    labels: Sequence[str],
    branch_name: str,
    extra_argv: Sequence[str] = (),
) -> int:
    """Run the configured AI harness inside *worktree* and return its exit code.

    Default implementation: OpenCode with the ``gentle-orchestrator``
    agent. The harness is responsible for writing
    ``<worktree>/worker-result.json`` describing what it did. Caduceus
    reads that file after this function returns and translates its
    status into ``Phase`` transitions; the bridge never inspects it.

    To swap harnesses:

    * Edit this function in the user-owned copy at
      ``$HERMES_HOME/caduceus/worker-bridge.py``.
    * Keep the same signature. The daemon's worker supervisor reads
      ``labels`` so the agent can branch on ticket type; ``branch_name``
      is the daemon-owned expected branch.
    * Add ``extra_argv`` to your invocation so test fixtures can pass
      arguments through without touching your CLI shape.

    The harness is launched with ``subprocess.run`` as an argument array —
    never a shell string — and inherits the bridge's environment after
    the daemon's allowlist. The reference harness invocation uses
    Unicode-safe commands (``opencode run --agent gentle-orchestrator
    -f <prompt>``) and passes the prompt path as a separate argument so
    paths containing spaces and Unicode characters reach the harness
    verbatim.
    """
    argv: list[str] = [
        "opencode",
        "run",
        "--agent",
        "gentle-orchestrator",
        "-f",
        str(prompt_file),
    ]
    argv.extend(extra_argv)
    argv.append("--")
    argv.append("Run the workflow per the attached prompt file.")
    completed = subprocess.run(argv, cwd=str(worktree), check=False)
    return completed.returncode


# ---------------------------------------------------------------------------
# Validation helpers — exported so the test suite can exercise them
# ---------------------------------------------------------------------------


def read_required_env(env: Mapping[str, str]) -> dict:
    """Return a new dict containing every required ``CADUCEUS_*`` value.

    Raises ``SystemExit(EXIT_MISSING_ENV)`` with a one-line stderr
    diagnostic naming each missing key. The error message never embeds
    the values (no echo of titles, bodies, or tokens).
    """
    missing = [name for name in REQUIRED_ENV_VARS if not env.get(name)]
    if missing:
        print(
            "caduceus bridge: missing required environment: "
            + ", ".join(missing),
            file=sys.stderr,
        )
        sys.exit(EXIT_MISSING_ENV)
    return {name: env[name] for name in REQUIRED_ENV_VARS}


def parse_labels(raw: str) -> list[str]:
    """Parse the JSON-encoded labels array.

    The daemon emits ``CADUCEUS_ISSUE_LABELS_JSON`` as a UTF-8 JSON array
    of strings. Anything else — a non-string element, a top-level object,
    a bare string of CSV labels — is a configuration error and the
    bridge exits with ``EXIT_MALFORMED_LABELS``.
    """
    try:
        decoded = json.loads(raw)
    except json.JSONDecodeError as exc:
        print(
            f"caduceus bridge: invalid CADUCEUS_ISSUE_LABELS_JSON: {exc}",
            file=sys.stderr,
        )
        sys.exit(EXIT_MALFORMED_LABELS)
    if not isinstance(decoded, list) or not all(
        isinstance(item, str) for item in decoded
    ):
        print(
            "caduceus bridge: CADUCEUS_ISSUE_LABELS_JSON must be a JSON "
            "array of strings",
            file=sys.stderr,
        )
        sys.exit(EXIT_MALFORMED_LABELS)
    return decoded


def verify_prompt(path: Path) -> Path:
    """Ensure the rendered prompt file is a regular file we can pass to the harness.

    The bridge refuses to launch a harness with a missing or unparseable
    prompt because a partial prompt almost always means the daemon's
    finalization step ran with stale state.
    """
    if not path.is_file():
        print(
            f"caduceus bridge: prompt file missing: {path}",
            file=sys.stderr,
        )
        sys.exit(EXIT_MISSING_PROMPT)
    return path


def resolve_worktree(env: Mapping[str, str]) -> Path:
    """Return the validated worktree path, raising on unset values."""
    raw = env.get("CADUCEUS_WORKTREE_PATH")
    if not raw:
        print(
            "caduceus bridge: missing required environment: CADUCEUS_WORKTREE_PATH",
            file=sys.stderr,
        )
        sys.exit(EXIT_MISSING_ENV)
    return Path(raw)


# ---------------------------------------------------------------------------
# Bridge entry point
# ---------------------------------------------------------------------------


def main(
    env: Mapping[str, str] | None = None,
    argv: Sequence[str] | None = None,
) -> int:
    """Bridge entry point.

    Both parameters default to the live process environment / ``sys.argv``
    so ``python -m`` and direct ``python worker-bridge.py`` invocations
    behave identically. The test suite calls :func:`main` with explicit
    arguments for both the hook and legacy paths.
    """
    env = os.environ if env is None else env
    argv = sys.argv if argv is None else argv

    ctx = _build_ctx(env)
    hook_path = _hook_path(env)
    if hook_path is not None:
        try:
            hook = _load_hook(hook_path)
        except Exception as exc:  # noqa: BLE001 - load failures are exit 125
            sys.stderr.write(_exception_line("harness hook crashed", exc))
            return EXIT_HARNESS_CRASH

        hook_result = _invoke_hook(hook, ctx)
        if isinstance(hook_result, int):
            return hook_result
        returncode, stdout, stderr = _execute_argv(hook_result, ctx)
        if stdout:
            sys.stdout.write(stdout)
        if stderr:
            sys.stderr.write(stderr)
        if not _result_path(ctx.worktree).exists():
            _synthesize_worker_result(ctx.worktree, returncode, stdout, stderr)
        return returncode

    # A current parent runner may have a separate user-owned legacy bridge.
    # Execute it as a full script, never import it into this process. When no
    # separate copy exists, retain the direct legacy invocation so an existing
    # 311-line installation and the historical test seam remain unchanged.
    legacy_path = _legacy_bridge_path(env)
    current_path = Path(__file__).resolve()
    if legacy_path.is_file() and legacy_path.resolve() != current_path:
        command = [sys.executable, str(legacy_path), *argv[1:]]
        try:
            completed = subprocess.run(command, env=dict(env), check=False)
            return completed.returncode
        except FileNotFoundError as exc:
            print(
                f"caduceus bridge: harness executable not found: {exc.filename}",
                file=sys.stderr,
            )
            return EXIT_HARNESS_NOT_FOUND
        except OSError as exc:
            print(
                f"caduceus bridge: unable to start harness: {exc}",
                file=sys.stderr,
            )
            return EXIT_HARNESS_UNREACHABLE

    try:
        return invoke_harness(
            worktree=ctx.worktree,
            prompt_file=ctx.prompt,
            run_id=ctx.run_id,
            labels=ctx.labels,
            branch_name=ctx.branch,
            extra_argv=tuple(argv[1:]),  # tests pass extra args after the script path
        )
    except FileNotFoundError as exc:
        print(
            f"caduceus bridge: harness executable not found: {exc.filename}",
            file=sys.stderr,
        )
        return EXIT_HARNESS_NOT_FOUND
    except OSError as exc:
        print(
            f"caduceus bridge: unable to start harness: {exc}",
            file=sys.stderr,
        )
        return EXIT_HARNESS_UNREACHABLE


if __name__ == "__main__":
    sys.exit(main())

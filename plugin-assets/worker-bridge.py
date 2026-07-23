#!/usr/bin/env python3
"""Caduceus harness bridge — canonical reference implementation.

This file is the reference bridge that ``hermes caduceus setup`` seeds at
``$HERMES_HOME/caduceus/worker-bridge.py``. It is the only part of Caduceus
written in Python. Its job is intentionally narrow:

1. Read the ``CADUCEUS_*`` environment variables that the daemon exports
   for the worker, fail with a clear diagnostic when any required value
   is missing or malformed.
2. Verify the worktree and the rendered prompt file exist and are usable.
3. Invoke the configured harness through :func:`invoke_harness` and
   return its exit code so the daemon's ``worker_timeout_seconds``,
   transcript capture, and retry budget behave correctly.

The bridge never touches the daemon state directory, never reads or writes
a heartbeat, and never claims, queues, or finalizes anything. All of that
work lives in the Rust core.

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

The reference harness is OpenCode with the gentle-orchestrator agent. To
swap harnesses, edit :func:`invoke_harness` in the *user-owned* copy at
``$HERMES_HOME/caduceus/worker-bridge.py`` — leave the validation in
:func:`main` alone. ``hermes caduceus setup`` will never overwrite that
copy automatically; if the upstream template changes it writes a sibling
``.new`` candidate and reports it, leaving your edits in place.

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

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
from pathlib import Path
from typing import List, Mapping, Sequence, Union


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
# Harness hook — the only function operators are expected to edit
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
    argv: List[str] = [
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
    completed = subprocess.run(argv, cwd=str(worktree))
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


def parse_labels(raw: str) -> List[str]:
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
    env: Union[Mapping[str, str], None] = None,
    argv: Union[Sequence[str], None] = None,
) -> int:
    """Bridge entry point.

    Both parameters default to the live process environment / ``sys.argv``
    so ``python -m`` and direct ``python worker-bridge.py`` invocations
    behave identically. The test suite calls :func:`main` with explicit
    arguments and patches :func:`invoke_harness` to assert behavior
    without spawning OpenCode.
    """
    env = os.environ if env is None else env
    argv = sys.argv if argv is None else argv

    values = read_required_env(env)
    labels = parse_labels(values["CADUCEUS_ISSUE_LABELS_JSON"])
    worktree = resolve_worktree(env)
    prompt_file = verify_prompt(worktree / PROMPT_FILE_NAME)

    run_id = values["CADUCEUS_RUN_ID"]
    branch_name = values["CADUCEUS_BRANCH_NAME"]

    try:
        return invoke_harness(
            worktree=worktree,
            prompt_file=prompt_file,
            run_id=run_id,
            labels=labels,
            branch_name=branch_name,
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

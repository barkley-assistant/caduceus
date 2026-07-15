#!/usr/bin/env python3
"""Deterministic test harness for ``tests/bridge_test.py``.

This script replaces OpenCode in the bridge's subprocess call. The test
suite points the ``PATH`` at a ``bin/`` directory where a shell-script
``opencode`` launches ``bridge_harness.py`` and forwards the bridge's
argv. The harness's behavior is selected by environment variables so
each test case can configure it without editing the file:

* ``FAKE_HARNESS_STDOUT`` — string to write to stdout before exiting.
* ``FAKE_HARNESS_STDERR`` — string to write to stderr before exiting.
* ``FAKE_HARNESS_EXIT``   — integer exit code (default ``0``).
* ``FAKE_HARNESS_SLEEP``  — when set to ``"1"``, the script sleeps
  ``FAKE_HARNESS_DELAY`` seconds instead of exiting immediately.
* ``FAKE_HARNESS_LOG``    — when set, append ``argv\\tenv_keys\\tcwd\\n``
  to this path so tests can assert the harness was invoked with the
  expected argument array.

The harness **never** writes a ``worker-result.json`` file — that's the
daemon's concern, not the bridge's. The bridge test suite patches
:func:`invoke_harness` so the real OpenCode path is exercised
separately, and the subprocess tests use this fixture so the harness's
exit code, stdout, stderr, and signal handling are all observable.
"""

from __future__ import annotations

import json
import os
import sys
import time


def main() -> int:
    log_path = os.environ.get("FAKE_HARNESS_LOG")
    if log_path:
        record = {
            "argv": list(sys.argv[1:]),
            "cwd": os.getcwd(),
            "env_keys": sorted(k for k in os.environ if k.startswith("CADUCEUS_")),
            "worktree_prompt_path": os.path.join(
                os.getcwd(), "worker-prompt.md"
            ),
        }
        with open(log_path, "a", encoding="utf-8") as fp:
            fp.write(json.dumps(record, ensure_ascii=False))
            fp.write("\n")

    if os.environ.get("FAKE_HARNESS_SLEEP") == "1":
        try:
            delay = float(os.environ.get("FAKE_HARNESS_DELAY", "5"))
        except ValueError:
            delay = 5.0
        time.sleep(delay)

    stdout = os.environ.get("FAKE_HARNESS_STDOUT", "")
    stderr = os.environ.get("FAKE_HARNESS_STDERR", "")
    if stdout:
        sys.stdout.write(stdout)
        sys.stdout.flush()
    if stderr:
        sys.stderr.write(stderr)
        sys.stderr.flush()

    try:
        exit_code = int(os.environ.get("FAKE_HARNESS_EXIT", "0"))
    except ValueError:
        exit_code = 0

    return exit_code


if __name__ == "__main__":
    sys.exit(main())


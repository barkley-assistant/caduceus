"""Caduceus v0.1 — Hermes plugin adapter (stdlib-only).

This module is the Hermes-facing surface for Caduceus. It exposes a single
``register(ctx)`` entry point. Importing this module is intentionally
side-effect-free: the adapter never compiles code, mutates configuration,
creates cron jobs, or performs network access during registration. All
side effects are performed only when the operator invokes an explicit
subcommand of ``hermes caduceus`` or the ``/caduceus-status`` slash command.

The adapter stays inside the Python standard library so Hermes can discover
and enable it before any Rust binary has been built. The reference bridge
template lives under ``plugin-assets/`` and is *not* imported here.

Three registrations are wired up at ``register`` time, per the Hermes
plugin compatibility contract in ``planning/caduceus-v0.1/CONTRACTS.md``:

1. ``ctx.register_skill("caduceus", <root>/skills/caduceus/SKILL.md, ...)``,
   resolvable as ``caduceus:caduceus``.
2. ``ctx.register_command("caduceus-status", handler, ...)`` for the
   ``/caduceus-status`` slash command. The handler invokes
   ``<root>/bin/caduceus status --json`` with an argument array, a short
   timeout, and bounded output. If the binary is missing it returns a
   chat-safe diagnostic explaining how to run ``hermes caduceus setup``.
3. ``ctx.register_cli_command(name="caduceus", ...)`` for the
   ``hermes caduceus <subcommand>`` family, with subcommands
   ``setup``, ``doctor``, ``status``, ``cron-install``, and ``cron-remove``.
"""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import sys
from pathlib import Path
from typing import Any, Callable, Dict, Optional


# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

PLUGIN_NAME = "caduceus"
SKILL_BARE_NAME = "caduceus"
SKILL_QUALIFIED_NAME = f"{PLUGIN_NAME}:{SKILL_BARE_NAME}"

# Bounded process limits for every subprocess the adapter spawns. Caduceus
# honors ``CONTRACTS.md``'s "subprocess calls use argument arrays, bounded
# output, timeouts, and redacted errors" requirement.
SUBPROCESS_TIMEOUT_SECONDS = 15
SUBPROCESS_OUTPUT_BYTES = 32 * 1024
SUBPROCESS_BUILD_TIMEOUT_SECONDS = 600


def _plugin_root() -> Path:
    """Resolve the repository root.

    Hermes installs the plugin by cloning the repository root into
    ``~/.hermes/plugins/caduceus/`` (per the contract), so this file is
    always two parents deep from the plugin root in production. The
    discovery also falls back to ``HERMES_CADUCEUS_ROOT`` so the adapter
    is testable from a working copy without changing directories.
    """
    env_root = os.environ.get("HERMES_CADUCEUS_ROOT")
    if env_root:
        return Path(env_root).resolve()
    return Path(__file__).resolve().parent


def _skill_path() -> Path:
    return _plugin_root() / "skills" / "caduceus" / "SKILL.md"


def _bridge_template_path() -> Path:
    return _plugin_root() / "plugin-assets" / "worker-bridge.py"


def _pulse_template_path() -> Path:
    return _plugin_root() / "plugin-assets" / "caduceus-pulse.sh"


def _binary_path() -> Path:
    """Return the absolute path to the installed ``caduceus`` binary.

    The path is computed from the plugin root, not derived from PATH, so a
    malicious shadow cannot impersonate the daemon. Setup builds this file
    under ``<plugin>/bin/caduceus``.
    """
    return _plugin_root() / "bin" / "caduceus"


# ---------------------------------------------------------------------------
# Subprocess helpers
# ---------------------------------------------------------------------------


def _redact(stderr: str) -> str:
    """Best-effort redaction of obvious credential tokens in stderr.

    The daemon itself redacts tokens before logging; this helper is the
    adapter's last-mile filter so accidental leaks never reach chat output.
    """
    if not stderr:
        return ""
    redacted = stderr
    for needle in ("GITHUB_TOKEN", "CADUCEUS_GITHUB_TOKEN", "GH_TOKEN"):
        if needle in redacted:
            # Replace any value that follows the variable name up to a
            # newline / space / shell-meta boundary. Handles both
            # bare values (``VAR=secret``) and quoted values
            # (``VAR="secret"`` / ``VAR='secret'``). Non-greedy, single
            # match per needle — the goal is damage control, not a
            # full scanner.
            import re

            redacted = re.sub(
                rf"({re.escape(needle)}\s*=\s*)([\"']?[^\s'\"`]+[\"']?)",
                lambda m: f"{m.group(1)}<redacted>",
                redacted,
            )
    return redacted


def _run(
    argv: list,
    *,
    cwd: Optional[Path] = None,
    timeout: int = SUBPROCESS_TIMEOUT_SECONDS,
) -> "subprocess.CompletedProcess[str]":
    """Run *argv* with bounded output, timeout, and text mode.

    The adapter never uses ``shell=True``. Errors are caught and re-raised
    as ``RuntimeError`` with a redacted message so the caller can present
    a chat-friendly diagnostic without leaking secrets.
    """
    try:
        return subprocess.run(
            argv,
            cwd=str(cwd) if cwd else None,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(
            f"timeout after {timeout}s running {shlex.join(argv)}"
        ) from exc
    except FileNotFoundError as exc:
        raise RuntimeError(
            f"command not found: {argv[0]}"
        ) from exc
    except OSError as exc:
        raise RuntimeError(f"failed to spawn {argv[0]!r}: {exc}") from exc


def _truncate(text: str, limit: int = SUBPROCESS_OUTPUT_BYTES) -> str:
    if text is None:
        return ""
    if len(text) <= limit:
        return text
    return text[:limit] + f"\n…<truncated {len(text) - limit} bytes>"


# ---------------------------------------------------------------------------
# Registration
# ---------------------------------------------------------------------------


def register(ctx: Any) -> None:
    """Register Caduceus with the supplied plugin context.

    Importing this module and calling ``register`` performs three actions
    and nothing else. It does not touch the filesystem outside the
    standard plugin registration paths, never compiles Rust, never opens
    a network socket, and never invokes a tool that mutates user config.
    """
    skill = _skill_path()
    if skill.is_file():
        ctx.register_skill(SKILL_BARE_NAME, skill)
    # Always register the slash command and the CLI command. Their
    # handlers may print "run setup" diagnostics when the binary is
    # missing, but registration itself is unconditional.
    ctx.register_command(
        "caduceus-status",
        handler=_handle_caduceus_status,
        description="Show Caduceus daemon state.",
        args_hint="",
    )
    ctx.register_cli_command(
        name=PLUGIN_NAME,
        help="Caduceus v0.1 lifecycle (setup, doctor, status, cron).",
        setup_fn=_register_caduceus_cli,
        handler_fn=_caduceus_cli_command,
        description=(
            "Manage the Caduceus daemon: install the binary, install or "
            "remove the two-minute cron job, and inspect daemon state."
        ),
    )
    # Wire the cronjob bridge to ``ctx.dispatch_tool("cronjob", ...)`` if
    # the ctx exposes it. The adapter never *creates* cron jobs at
    # register time — it only stores the dispatcher so that explicit
    # operator invocations of ``hermes caduceus cron-install`` can route
    # through Hermes's documented cronjob tool surface.
    dispatcher = getattr(ctx, "dispatch_tool", None)
    if callable(dispatcher):
        from . import _runtime  # late import; avoids stdlib pollution

        _runtime.install_dispatcher(dispatcher)


# ---------------------------------------------------------------------------
# Slash command: /caduceus-status
# ---------------------------------------------------------------------------


def _handle_caduceus_status(raw_args: str) -> Optional[str]:
    """``/caduceus-status`` handler.

    Forwards to ``<root>/bin/caduceus status --json`` with a short timeout
    and bounded output. Returns a chat-safe diagnostic when the binary is
    not built yet.
    """
    binary = _binary_path()
    if not binary.is_file():
        return (
            "Caduceus is installed but the binary has not been built. "
            "Run `hermes caduceus setup` to build it, then try again."
        )
    proc = _run(
        [str(binary), "status", "--json"],
        cwd=_plugin_root(),
        timeout=SUBPROCESS_TIMEOUT_SECONDS,
    )
    if proc.returncode != 0:
        message = _redact(_truncate(proc.stderr or proc.stdout))
        if not message:
            message = f"exit status {proc.returncode}"
        return f"caduceus status failed: {message.strip()}"
    body = _truncate(proc.stdout)
    try:
        parsed = json.loads(body)
    except json.JSONDecodeError:
        return body.strip() or "(empty caduceus status output)"
    return _format_status_for_chat(parsed)


def _format_status_for_chat(payload: Dict[str, Any]) -> str:
    """Render a status JSON payload as a short chat-friendly summary."""
    version = payload.get("version") or "unknown"
    last_tick = payload.get("last_tick") or "never"
    last_outcome = payload.get("last_outcome") or "n/a"
    phases = payload.get("phases") or {}
    phase_counts = ", ".join(f"{k}={v}" for k, v in phases.items()) or "none"
    next_head = payload.get("next_head") or "none"
    rate_limit = payload.get("rate_limit") or {}
    rl = (
        f"rate_limit={rate_limit.get('remaining')}/{rate_limit.get('limit')}"
        if rate_limit.get("limit")
        else "rate_limit=unknown"
    )
    return (
        f"caduceus {version} — last tick: {last_tick} ({last_outcome})\n"
        f"  queue: {phase_counts}\n"
        f"  next: {next_head}\n"
        f"  {rl}"
    )


# ---------------------------------------------------------------------------
# CLI command: hermes caduceus <setup|doctor|status|cron-install|cron-remove>
# ---------------------------------------------------------------------------


def _register_caduceus_cli(subparser: Any) -> None:
    """Wire the ``hermes caduceus`` argparse tree."""
    subs = subparser.add_subparsers(dest="caduceus_command", required=True)

    setup = subs.add_parser(
        "setup",
        help="Build the Rust binary and seed the user-owned bridge.",
    )
    setup.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the actions without running them.",
    )

    subs.add_parser(
        "doctor",
        help="Verify the binary, bridge, and cron job are healthy.",
    )

    subs.add_parser(
        "status",
        help="Run `caduceus status` and print the result.",
    )

    cron_install = subs.add_parser(
        "cron-install",
        help="Create the no-agent 2-minute cron job + bash wrapper.",
    )
    cron_install.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the planned changes without applying them.",
    )

    cron_remove = subs.add_parser(
        "cron-remove",
        help="Remove the cron job and bash wrapper.",
    )

    subparser.set_defaults(func=_caduceus_cli_command)


def _caduceus_cli_command(args: Any) -> int:
    sub = getattr(args, "caduceus_command", None)
    if sub == "setup":
        return _cli_setup(dry_run=getattr(args, "dry_run", False))
    if sub == "doctor":
        return _cli_doctor()
    if sub == "status":
        return _cli_status()
    if sub == "cron-install":
        return _cli_cron_install(dry_run=getattr(args, "dry_run", False))
    if sub == "cron-remove":
        return _cli_cron_remove()
    print(f"caduceus: unknown subcommand {sub!r}", file=sys.stderr)
    return 2


# ---- setup -----------------------------------------------------------------


def _cli_setup(*, dry_run: bool) -> int:
    """Build the daemon binary and seed the user-owned bridge.

    Steps, in order:

    1. Verify Rust/Cargo/Git/Python prerequisites.
    2. ``cargo build --release --locked --manifest-path <root>/Cargo.toml``.
    3. Atomically install the resulting executable at
       ``<root>/bin/caduceus`` with mode 0755.
    4. Create the configured state directories with mode 0700.
    5. Seed the user-owned bridge at ``$HERMES_HOME/caduceus/worker-bridge.py``
       (default ``~/.hermes/caduceus/worker-bridge.py``) only when absent.
       If the shipped bridge template differs from the user copy, write a
       sibling ``.new`` candidate and report it.
    """
    root = _plugin_root()
    if dry_run:
        print(f"[dry-run] caduceus setup: would build {root}/Cargo.toml")
        print(f"[dry-run] caduceus setup: would install to {_binary_path()}")
        print(f"[dry-run] caduceus setup: would seed {_user_bridge_path()}")
        return 0

    failures = _check_setup_prerequisites(root)
    if failures:
        for line in failures:
            print(f"caduceus setup: {line}", file=sys.stderr)
        return 1

    binary = _build_daemon_binary(root)
    if binary is None:
        print("caduceus setup: cargo build failed", file=sys.stderr)
        return 1
    _atomic_install_binary(binary, _binary_path())
    _ensure_state_directories(_state_dir())
    _seed_user_bridge()
    print(f"caduceus setup: installed {_binary_path()}")
    return 0


def _check_setup_prerequisites(root: Path) -> list:
    """Return a list of human-readable errors if prerequisites are missing."""
    errors = []
    for tool in ("cargo", "git", "python3"):
        proc = _run(
            [tool, "--version"],
            cwd=root,
            timeout=SUBPROCESS_TIMEOUT_SECONDS,
        )
        if proc.returncode != 0:
            errors.append(f"{tool} is not installed or returned non-zero")
    if not (root / "Cargo.toml").is_file():
        errors.append(f"missing manifest at {root / 'Cargo.toml'}")
    return errors


def _build_daemon_binary(root: Path) -> Optional[Path]:
    """Run ``cargo build --release --locked`` and return the produced path.

    Returns ``None`` on build failure. The adapter never invokes cargo with
    ``shell=True`` and never bypasses ``--locked``; lockfile drift is a
    documented contract concern.
    """
    proc = _run(
        ["cargo", "build", "--release", "--locked", "--manifest-path", str(root / "Cargo.toml")],
        cwd=root,
        timeout=SUBPROCESS_BUILD_TIMEOUT_SECONDS,
    )
    if proc.returncode != 0:
        sys.stderr.write(_redact(_truncate(proc.stderr or proc.stdout)))
        sys.stderr.write("\n")
        return None
    target = root / "target" / "release" / "caduceus"
    if not target.is_file():
        print(f"caduceus setup: cargo reported success but {target} is missing", file=sys.stderr)
        return None
    return target


def _atomic_install_binary(src: Path, dst: Path) -> None:
    """Atomically move *src* to *dst* and chmod 0755.

    Uses ``os.replace`` for atomicity on the same filesystem; on a
    cross-filesystem move the call is preceded by ``shutil.copy2`` so the
    final replace stays on *dst*'s filesystem.
    """
    dst.parent.mkdir(parents=True, exist_ok=True)
    tmp = dst.with_name(dst.name + ".tmp")
    if tmp.exists() or tmp.is_symlink():
        tmp.unlink()
    import shutil

    shutil.copy2(src, tmp)
    os.replace(tmp, dst)
    os.chmod(dst, 0o755)


# ---- doctor / status -------------------------------------------------------


def _cli_doctor() -> int:
    """Print whether the binary, bridge, cron wrapper, and cron job look healthy.

    The doctor output is deliberately self-explanatory: every line names
    the file path or cron job identifier, the next action to take when
    something is wrong, and the lifecycle facts operators routinely
    forget (gateway requirement, plugin-skill opt-in nature, etc.).
    """
    binary_ok = _binary_path().is_file()
    bridge_path = _user_bridge_path()
    bridge_ok = bridge_path.is_file() and not bridge_path.is_symlink()
    wrapper = _pulse_wrapper_path()
    wrapper_ok = wrapper.is_file() and not wrapper.is_symlink()
    cron_ok = False
    cron_detail = "not found"
    try:
        cron_ok, cron_detail = _cron_job_state(name="caduceus")
    except RuntimeError as exc:
        cron_detail = str(exc)
    print(f"binary present : {'yes' if binary_ok else 'no'} ({_binary_path()})")
    print(f"bridge present : {'yes' if bridge_ok else 'no'} ({bridge_path})")
    print(f"cron wrapper   : {'yes' if wrapper_ok else 'no'} ({wrapper})")
    print(f"cron job       : {'yes' if cron_ok else 'no'} ({cron_detail})")
    print(f"plugin skill   : opt-in (caduceus:caduceus — explicit skill_view only)")
    print(f"plugin layout  : standalone (no tools/hooks; explicit setup required)")
    if binary_ok and bridge_ok and wrapper_ok and cron_ok:
        print(
            "gateway req    : the Hermes gateway (or a configured managed cron "
            "provider) must be running for the cron job to fire"
        )
    print()
    print("lifecycle:")
    print("  install     : hermes plugins install barkley-assistant/caduceus --enable")
    print("  build       : hermes caduceus setup")
    print("  schedule    : hermes caduceus cron-install")
    print("  inspect     : hermes caduceus status   |   /caduceus-status")
    print("  source up   : hermes plugins update caduceus   then   hermes caduceus setup")
    print("  uninstall   : hermes caduceus cron-remove   then   hermes plugins remove caduceus")
    return 0 if (binary_ok and bridge_ok) else 1


def _cli_status() -> int:
    binary = _binary_path()
    if not binary.is_file():
        print(
            "caduceus: binary not built — run `hermes caduceus setup`",
            file=sys.stderr,
        )
        return 1
    proc = _run(
        [str(binary), "status"],
        cwd=_plugin_root(),
        timeout=SUBPROCESS_TIMEOUT_SECONDS,
    )
    if proc.stdout:
        sys.stdout.write(proc.stdout)
    if proc.stderr:
        sys.stderr.write(proc.stderr)
    return proc.returncode


# ---- state dirs / bridge ---------------------------------------------------


def _hermes_home() -> Path:
    """Return HERMES_HOME, defaulting to ``~/.hermes`` per Hermes."""
    raw = os.environ.get("HERMES_HOME")
    if raw:
        return Path(raw).expanduser().resolve()
    return Path(os.path.expanduser("~/.hermes")).resolve()


def _state_dir() -> Path:
    """Return the configured state directory.

    Resolution follows the daemon's own chain: ``$CADUCEUS_STATE_DIR``,
    then ``$HERMES_HOME/caduceus-state``, then
    ``~/.config/caduceus/state``. The adapter always returns the
    Hermes-canonical default — explicit overrides are the daemon's
    concern.
    """
    raw = os.environ.get("CADUCEUS_STATE_DIR")
    if raw:
        return Path(raw).expanduser().resolve()
    return _hermes_home() / "caduceus-state"


def _ensure_state_directories(state_dir: Path) -> None:
    """Create the daemon state directories with mode 0700."""
    for sub in ("", "runs", "claims", "cache"):
        path = state_dir if not sub else state_dir / sub
        path.mkdir(parents=True, exist_ok=True)
        try:
            os.chmod(path, 0o700)
        except OSError:
            pass


def _user_bridge_path() -> Path:
    return _hermes_home() / "caduceus" / "worker-bridge.py"


def _seed_user_bridge() -> None:
    """Write a user-owned bridge from the shipped template if absent.

    If the user already has a bridge and the shipped template differs,
    write a sibling ``.new`` candidate and report it on stdout. The user
    copy is never overwritten; CONTRACTS.md is explicit about this.
    """
    template = _bridge_template_path()
    target = _user_bridge_path()
    target.parent.mkdir(parents=True, exist_ok=True)
    if not template.is_file():
        return  # Nothing to seed; the daemon will surface a precise error.
    if not target.exists():
        target.write_text(template.read_text(encoding="utf-8"), encoding="utf-8")
        try:
            os.chmod(target, 0o755)
        except OSError:
            pass
        return
    template_text = template.read_text(encoding="utf-8")
    target_text = target.read_text(encoding="utf-8")
    if template_text != target_text:
        candidate = target.with_name(target.name + ".new")
        candidate.write_text(template_text, encoding="utf-8")
        try:
            os.chmod(candidate, 0o755)
        except OSError:
            pass
        print(f"caduceus setup: bridge updated upstream — wrote {candidate}")


# ---- cron ------------------------------------------------------------------


def _pulse_wrapper_path() -> Path:
    """Return the absolute path of the installed bash wrapper."""
    return _hermes_home() / "scripts" / "caduceus-pulse.sh"


def _cron_install(*, dry_run: bool) -> tuple:
    """Reconcile the single ``caduceus`` cron job via ``ctx.dispatch_tool``.

    Returns a tuple ``(action, note)`` describing what happened. The
    helper is shared between the public CLI and the pytest suite.

    * 0 matches → ``("created", job_id)``.
    * 1 match   → ``("reused", job_id)`` (after update if needed).
    * >1 match  → raises ``RuntimeError`` listing the job IDs.

    The wrapper is unconditionally rewritten so its absolute binary path
    matches the current plugin install location.
    """
    binary = _binary_path()
    if not binary.is_file():
        raise RuntimeError("caduceus binary not built; run `hermes caduceus setup`")
    _write_pulse_wrapper(binary)
    cronjob = _cron_job_registry()
    matches = [job for job in cronjob.values() if job.get("name") == "caduceus"]
    if len(matches) > 1:
        ids = ", ".join(sorted(str(j.get("id")) for j in matches))
        raise RuntimeError(f"multiple caduceus cron jobs found: {ids}")
    if dry_run:
        return (("created" if not matches else "reused"), "dry-run")
    if not matches:
        job_id = _cronjob_create(
            schedule="every 2m",
            name="caduceus",
            script=_pulse_wrapper_path().name,
            no_agent=True,
        )
        return ("created", job_id)
    job_id = str(matches[0].get("id"))
    _cronjob_update(
        job_id=job_id,
        schedule="every 2m",
        name="caduceus",
        script=_pulse_wrapper_path().name,
        no_agent=True,
    )
    return ("reused", job_id)


def _write_pulse_wrapper(binary: Path) -> None:
    """Atomically write the ``caduceus-pulse.sh`` wrapper.

    The wrapper contains the absolute installed binary path and uses
    ``exec`` so the cron process replaces its shell with the daemon. This
    matches the contract's "exec <binary> run" requirement.
    """
    path = _pulse_wrapper_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    body = (
        "#!/usr/bin/env bash\n"
        "# Generated by `hermes caduceus cron-install`. Do not edit by hand;\n"
        "# rerun that command after moving the plugin.\n"
        "set -euo pipefail\n"
        f"exec {binary} run \"$@\"\n"
    )
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(body, encoding="utf-8")
    try:
        os.chmod(tmp, 0o755)
    except OSError:
        pass
    os.replace(tmp, path)


def _cron_install_cli(*, dry_run: bool) -> int:
    try:
        action, note = _cron_install(dry_run=dry_run)
    except RuntimeError as exc:
        print(f"caduceus cron-install: {exc}", file=sys.stderr)
        return 1
    print(f"caduceus cron-install: {action} ({note})")
    return 0


def _cli_cron_install(*, dry_run: bool) -> int:
    return _cron_install_cli(dry_run=dry_run)


def _cli_cron_remove() -> int:
    try:
        cronjob = _cron_job_registry()
        matches = [job for job in cronjob.values() if job.get("name") == "caduceus"]
    except RuntimeError as exc:
        print(f"caduceus cron-remove: {exc}", file=sys.stderr)
        return 1
    for job in matches:
        try:
            _cronjob_remove(str(job.get("id")))
        except RuntimeError as exc:
            print(f"caduceus cron-remove: {exc}", file=sys.stderr)
            return 1
    wrapper = _pulse_wrapper_path()
    if wrapper.is_file() or wrapper.is_symlink():
        try:
            wrapper.unlink()
        except OSError as exc:
            print(f"caduceus cron-remove: cannot delete wrapper: {exc}", file=sys.stderr)
            return 1
    print("caduceus cron-remove: complete")
    return 0


# ---- cronjob registry helpers ---------------------------------------------


def _cron_job_registry() -> Dict[str, Dict[str, Any]]:
    """Return the current cron-job registry as a mapping keyed by job id.

    The adapter accesses Hermes's cron system through the documented
    ``ctx.dispatch_tool("cronjob", {"action": "list"})`` interface; this
    function lets us replace the indirection in tests.
    """
    # Late import keeps the module stdlib-only at import time and lets
    # the test suite substitute a stub ``ctx`` if the cron tool is
    # absent in a minimal environment.
    from . import _runtime as rt  # type: ignore[import-not-found]

    return rt.cron_list_jobs()


def _cronjob_create(*, schedule: str, name: str, script: str, no_agent: bool) -> str:
    from . import _runtime as rt  # type: ignore[import-not-found]

    return rt.cron_create_job(schedule=schedule, name=name, script=script, no_agent=no_agent)


def _cronjob_update(*, job_id: str, schedule: str, name: str, script: str, no_agent: bool) -> None:
    from . import _runtime as rt  # type: ignore[import-not-found]

    rt.cron_update_job(
        job_id=job_id,
        schedule=schedule,
        name=name,
        script=script,
        no_agent=no_agent,
    )


def _cronjob_remove(job_id: str) -> None:
    from . import _runtime as rt  # type: ignore[import-not-found]

    rt.cron_remove_job(job_id)


def _cron_job_state(*, name: str) -> tuple:
    """Return ``(exists, detail)`` for the named cron job.

    ``detail`` is a human-readable summary of what the registry returned
    (job id + schedule), or an error message when the registry could not
    be queried.
    """
    try:
        cronjob = _cron_job_registry()
    except RuntimeError as exc:
        return (False, str(exc))
    matches = [job for job in cronjob.values() if job.get("name") == name]
    if not matches:
        return (False, "not registered")
    job = matches[0]
    return (True, f"id={job.get('id')} schedule={job.get('schedule')}")


__all__ = [
    "PLUGIN_NAME",
    "SKILL_BARE_NAME",
    "SKILL_QUALIFIED_NAME",
    "register",
    "_handle_caduceus_status",
    "_register_caduceus_cli",
    "_caduceus_cli_command",
    "_plugin_root",
    "_binary_path",
    "_bridge_template_path",
    "_pulse_template_path",
]
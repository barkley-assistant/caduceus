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
import stat
import subprocess
import sys
from collections import namedtuple
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Union


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


# ---------------------------------------------------------------------------
# Transactional types
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class _Snapshot:
    """Immutable snapshot of wrapper state before a cron mutation.

    Captured by ``_snapshot_wrapper_and_job`` before any create/update/remove
    operation so the system can roll back to a known state if the mutation
    fails (AC-01).

    Attributes:
        wrapper_bytes: The raw bytes of the wrapper file at snapshot time.
            Empty ``b""`` if the file did not exist.
        wrapper_mode: The ``st_mode`` bits of the wrapper file. ``0`` if
            the file did not exist.
        job_dict: The matching cron job dict (from ``_cron_job_registry``),
            or ``None`` if no job with the target name was registered.
    """

    wrapper_bytes: bytes
    wrapper_mode: int
    job_dict: Optional[Dict[str, Any]]


class _NeedsAttention:
    """Returned by ``_reconcile_after_error`` when rollback is impossible.

    The CLI MUST exit nonzero when this is returned (AC-04). The
    ``recovery_evidence`` string describes what state was found and what
    manual steps are needed.

    This is intentionally not a frozen dataclass so it does not look like
    ``_Snapshot`` and is not a tuple so callers cannot accidentally unpack
    it as ``(action, note)``.
    """

    def __init__(self, recovery_evidence: str) -> None:
        self.recovery_evidence = recovery_evidence

    def __repr__(self) -> str:
        return f"NeedsAttention(recovery_evidence={self.recovery_evidence!r})"


# ---------------------------------------------------------------------------
# Doctor types
# ---------------------------------------------------------------------------


_DoctorFinding = namedtuple(
    "_DoctorFinding",
    ["category", "status", "detail", "next_action", "internal_detail"],
    defaults=("",),
)
"""Structured doctor finding.

Attributes:
    category: One of ``"host-capability-unavailable"``, ``"gateway-inactive"``,
        ``"config-incomplete"``, ``"daemon-defect"``.
    status: ``"ok"`` or ``"fail"``.
    detail: Human-readable operator-facing description of the finding.
    next_action: What the operator should do to fix the issue, or empty
        string if status is ``"ok"``.
    internal_detail: Internal diagnostic for ``--verbose`` output, never
        shown by default. May include structured categories such as
        ``"malformed-response"`` because it is not operator-facing.
"""


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
    # Cron mutations happen through the ``hermes`` CLI subprocess (see
    # ``_runtime.py``). Hermes v0.19.0 no longer exposes a ``cronjob`` MCP
    # tool, so there is nothing to wire from ``ctx.dispatch_tool``. The
    # adapter never *creates* cron jobs at register time.


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

    doctor = subs.add_parser(
        "doctor",
        help="Verify the binary, bridge, and cron job are healthy.",
    )
    doctor.add_argument(
        "--verbose",
        action="store_true",
        help="Print internal detail and structured category (human debugging only).",
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
    cron_install.add_argument(
        "--verbose",
        action="store_true",
        help="Print internal detail and structured category (human debugging only).",
    )

    cron_remove = subs.add_parser(
        "cron-remove",
        help="Remove the cron job and bash wrapper.",
    )
    cron_remove.add_argument(
        "--verbose",
        action="store_true",
        help="Print internal detail and structured category (human debugging only).",
    )

    subparser.set_defaults(func=_caduceus_cli_command)


def _caduceus_cli_command(args: Any) -> int:
    sub = getattr(args, "caduceus_command", None)
    if sub == "setup":
        return _cli_setup(dry_run=getattr(args, "dry_run", False))
    if sub == "doctor":
        return _cli_doctor(verbose=getattr(args, "verbose", False))
    if sub == "status":
        return _cli_status()
    if sub == "cron-install":
        return _cli_cron_install(
            dry_run=getattr(args, "dry_run", False),
            verbose=getattr(args, "verbose", False),
        )
    if sub == "cron-remove":
        return _cli_cron_remove(verbose=getattr(args, "verbose", False))
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


def _doctor_check_binary() -> _DoctorFinding:
    """Check that the caduceus binary is present at the expected path (AC-05)."""
    binary = _binary_path()
    if binary.is_file():
        return _DoctorFinding(
            category="host-capability-unavailable",
            status="ok",
            detail=f"caduceus binary present at {binary}",
            next_action="",
            internal_detail="",
        )
    return _DoctorFinding(
        category="host-capability-unavailable",
        status="fail",
        detail=f"caduceus binary not found at {binary} (run setup to build it)",
        next_action="run `hermes caduceus setup` to build and install the binary",
        internal_detail="",
    )


def _doctor_check_bridge_harness() -> _DoctorFinding:
    """Check that the configured worker_command path is executable (AC-10).

    Checks ``os.X_OK`` on the bridge path. Does NOT execute the script.
    """
    bridge = _user_bridge_path()
    if not bridge.is_file():
        # Not necessarily a defect — the bridge may not have been seeded yet.
        # But if the path exists and is not executable, that's a problem.
        return _DoctorFinding(
            category="host-capability-unavailable",
            status="ok",
            detail=f"worker bridge not yet seeded at {bridge} (external prerequisite)",
            next_action="",
            internal_detail=f"bridge harness at {bridge} (not yet created)",
        )
    if os.access(bridge, os.X_OK):
        return _DoctorFinding(
            category="host-capability-unavailable",
            status="ok",
            detail=f"worker bridge at {bridge} is executable",
            next_action="",
            internal_detail="",
        )
    return _DoctorFinding(
        category="host-capability-unavailable",
        status="fail",
        detail=f"worker bridge at {bridge} is not executable (mode {oct(stat.S_IMODE(bridge.stat().st_mode))})",
        next_action=f"run `chmod +x {bridge}` to make it executable",
        internal_detail="",
    )


def _doctor_check_provider_secret() -> _DoctorFinding:
    """Check that the provider secret name is configured (AC-10).

    Checks for the presence of a secret-name in the environment or config.
    Does NOT read the secret value and does NOT make network calls.
    """
    # The provider secret name is expected in the environment.
    # Caduceus's daemon config uses CADUCEUS_GITHUB_TOKEN or GITHUB_TOKEN.
    # The secret *name* (not value) is checked here.
    checked = "CADUCEUS_GITHUB_TOKEN, GITHUB_TOKEN, GH_TOKEN"
    for secret_name in ("CADUCEUS_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"):
        if os.environ.get(secret_name):
            return _DoctorFinding(
                category="config-incomplete",
                status="ok",
                detail=f"provider secret name {secret_name} is configured (no value read)",
                next_action="",
                internal_detail="",
            )
    # No secret name found — the operator may need to configure one.
    return _DoctorFinding(
        category="config-incomplete",
        status="fail",
        detail=f"no provider secret name configured (checked {checked})",
        next_action="set one of CADUCEUS_GITHUB_TOKEN, GITHUB_TOKEN, or GH_TOKEN in the environment",
        internal_detail="",
    )


def _doctor_check_cron_capability(ctx: Any) -> _DoctorFinding:
    """Check that cron capability is available via a bounded round-trip (AC-05).

    Performs a single ``cronjob list`` call via the runtime dispatcher,
    then distinguishes five outcomes: dispatcher absent, no Caduceus job,
    one or more Caduceus jobs, malformed payload, and every other error.
    """
    del ctx  # ctx is not needed — we use the runtime dispatcher directly
    from . import _runtime as rt  # type: ignore[import-not-found]

    try:
        rt._resolve_hermes()
    except rt.CronCapabilityError:
        return _DoctorFinding(
            category="host-capability-unavailable",
            status="fail",
            detail="hermes CLI not on PATH",
            next_action="install Hermes Agent v0.18.2+ and ensure `hermes` is on PATH",
            internal_detail="shutil.which('hermes') returned None",
        )
    try:
        jobs = rt.cron_list_jobs()
    except rt.CronCapabilityError as exc:
        if exc.category == "malformed-response":
            return _DoctorFinding(
                category="host-capability-unavailable",
                status="fail",
                detail="Hermes returned an unexpected payload shape for the cron list",
                next_action="run `hermes plugins install --enable` to refresh the adapter, then re-check",
                internal_detail=str(exc),
            )
        return _DoctorFinding(
            category="host-capability-unavailable",
            status="fail",
            detail=f"cron list call raised an exception: {exc.category}",
            next_action="run `hermes caduceus cron-install` to register the 2-minute job (or re-run `hermes plugins install --enable` if the adapter was reinstalled)",
            internal_detail=f"cron list call raised: {exc.category}: {exc.detail}",
        )
    caduceus_jobs = [job for job in jobs.values() if job.get("name") == "caduceus"]
    if caduceus_jobs:
        noun = "job" if len(caduceus_jobs) == 1 else "jobs"
        return _DoctorFinding(
            category="host-capability-unavailable",
            status="ok",
            detail=f"{len(caduceus_jobs)} Caduceus cron {noun} registered (external prerequisite, exercised)",
            next_action="",
            internal_detail="cron capability is available (cronjob list succeeded)",
        )
    return _DoctorFinding(
        category="host-capability-unavailable",
        status="ok",
        detail="no Caduceus cron job registered yet (external prerequisite, not exercised)",
        next_action="run `hermes caduceus cron-install` to register the 2-minute job",
        internal_detail="cron list returned 0 Caduceus jobs",
    )


def _doctor_check_hermes_home() -> _DoctorFinding:
    """Check that the Hermes home directory exists on disk (AC-05).

    This is a prerequisite check, not an active probe of the Hermes
    gateway. It confirms a well-known directory the Hermes CLI owns.
    """
    hermes_home = _hermes_home()
    if hermes_home.is_dir():
        return _DoctorFinding(
            category="gateway-inactive",
            status="ok",
            detail=f"Hermes home at {hermes_home} exists (external prerequisite)",
            next_action="",
            internal_detail="",
        )
    return _DoctorFinding(
        category="gateway-inactive",
        status="fail",
        detail=f"Hermes home at {hermes_home} not found",
        next_action="install Hermes and ensure the home directory is initialised",
        internal_detail="",
    )


def _cli_doctor(verbose: bool = False) -> int:
    """Run all doctor checks and print a structured report (AC-06/07/08/11).

    Each check is independent — a failure in one does NOT short-circuit
    the others. Exit codes:
        0: all checks healthy
        1: Caduceus config/runtime defects (``daemon-defect``, ``config-incomplete``)
        2: host capability / external prerequisite (``host-capability-unavailable``,
           ``gateway-inactive``)

    Exit 2 takes precedence over exit 1 because prerequisites block everything.

    ``--verbose`` prints the internal detail string and the structured
    category on FAIL lines. The verbose flag is always honoured when
    the operator passes it — operators running ``--verbose`` from a CI
    shell (e.g. for debugging) get the internal detail. CI log hygiene
    is achieved by the default output being operator-only (verbose=False
    is the default), not by overriding an explicit verbose flag.
    """
    checks = [
        ("Binary", _doctor_check_binary()),
        ("Bridge Harness", _doctor_check_bridge_harness()),
        ("Provider Secret", _doctor_check_provider_secret()),
        ("Cron Capability", _doctor_check_cron_capability(ctx=None)),
        ("Hermes Home", _doctor_check_hermes_home()),
    ]

    effective_verbose = verbose
    max_severity = 0  # 0 = ok, 1 = config/runtime, 2 = prerequisite
    for name, finding in checks:
        status_mark = "OK" if finding.status == "ok" else "FAIL"
        print(f"[{status_mark}] {name} — {finding.detail}")
        if finding.next_action:
            print(f"       next action: {finding.next_action}")
        if effective_verbose:
            internal = finding.internal_detail or finding.detail
            print(f"       detail:      {internal}")
            if finding.status != "ok":
                print(f"       category:    {finding.category}")
        print()
        if finding.status != "ok":
            if finding.category in ("host-capability-unavailable", "gateway-inactive"):
                max_severity = max(max_severity, 2)
            else:
                max_severity = max(max_severity, 1)

    return max_severity


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


def _snapshot_wrapper_and_job(
    wrapper_path: Path, job_name: str, registry: Dict[str, Dict[str, Any]]
) -> _Snapshot:
    """Capture the current wrapper bytes/mode and the matching job (AC-01).

    Reads the wrapper file at *wrapper_path* (if it exists) and searches
    *registry* for a job whose ``name`` matches *job_name*. The returned
    ``_Snapshot`` is the rollback target for ``_reconcile_after_error``.

    ``registry`` is the already-resolved result of ``_cron_job_registry()``
    — the function does NOT make a dispatch call itself.
    """
    wrapper_bytes: bytes = b""
    wrapper_mode: int = 0
    try:
        if wrapper_path.is_file():
            wrapper_bytes = wrapper_path.read_bytes()
            wrapper_mode = stat.S_IMODE(wrapper_path.stat().st_mode)
    except OSError:
        # Best-effort — if we cannot read the wrapper, proceed with empty.
        pass
    matches = [job for job in registry.values() if job.get("name") == job_name]
    job_dict = matches[0] if matches else None
    return _Snapshot(
        wrapper_bytes=wrapper_bytes,
        wrapper_mode=wrapper_mode,
        job_dict=job_dict,
    )


def _reconcile_after_error(
    error: Exception,
    snapshot: _Snapshot,
    ctx: Any,
    job_name: str,
    intended_state: str,
) -> Union[None, _NeedsAttention]:
    """Re-list and restore after a cron mutation error (AC-03/04).

    Called from ``_cron_install`` and ``_cli_cron_remove`` when a
    create/update/remove operation raises. The function:

    1. Re-lists the cron job registry.
    2. If *intended_state* is ``"present"`` and a matching job exists →
       success (no-op).
    3. If *intended_state* is ``"absent"`` and no matching job exists →
       success (no-op).
    4. If the registry and wrapper match the *snapshot* → success (no-op).
    5. Otherwise, restores the wrapper bytes/mode from *snapshot* and
       re-creates or re-updates the cron job to match *snapshot.job_dict*.
    6. If restoration is impossible → returns ``_NeedsAttention`` with
       recovery evidence.

    Returns ``None`` on successful reconciliation, or ``_NeedsAttention``
    when manual intervention is needed.
    """
    del error  # error is logged, but reconciliation is based on state
    wrapper_path = _pulse_wrapper_path()

    # Step 1: Re-list.
    try:
        registry = _cron_job_registry()
    except Exception:
        # Cannot even re-list — return NeedsAttention.
        return _NeedsAttention(
            recovery_evidence=(
                f"re-list failed after cron error; "
                f"snapshot wrapper_bytes={len(snapshot.wrapper_bytes)}B "
                f"job_dict={'present' if snapshot.job_dict else 'absent'}"
            )
        )

    matches = [job for job in registry.values() if job.get("name") == job_name]

    # Step 2: Intended state already achieved.
    if intended_state == "present" and matches:
        return None
    if intended_state == "absent" and not matches:
        return None

    # Step 3: Nothing changed from snapshot.
    current_wrapper_bytes: bytes = b""
    current_wrapper_mode: int = 0
    try:
        if wrapper_path.is_file():
            current_wrapper_bytes = wrapper_path.read_bytes()
            current_wrapper_mode = stat.S_IMODE(wrapper_path.stat().st_mode)
    except OSError:
        pass
    if (
        current_wrapper_bytes == snapshot.wrapper_bytes
        and current_wrapper_mode == snapshot.wrapper_mode
        and (not matches if snapshot.job_dict is None else len(matches) == 1)
    ):
        return None

    # Step 4: Restore wrapper from snapshot.
    wrapper_restored = False
    try:
        if snapshot.wrapper_bytes:
            wrapper_path.parent.mkdir(parents=True, exist_ok=True)
            wrapper_path.write_bytes(snapshot.wrapper_bytes)
            if snapshot.wrapper_mode:
                os.chmod(wrapper_path, snapshot.wrapper_mode)
            wrapper_restored = True
        elif wrapper_path.exists():
            wrapper_path.unlink()
            wrapper_restored = True
    except OSError:
        pass

    # Step 5: Restore job from snapshot.
    job_restored = False
    if snapshot.job_dict is not None:
        try:
            job_id = snapshot.job_dict.get("id")
            if job_id and job_id in registry:
                # Already exists — update.
                _cronjob_update(
                    job_id=job_id,
                    schedule=snapshot.job_dict.get("schedule", "every 2m"),
                    name=job_name,
                    script=snapshot.job_dict.get("script", "caduceus-pulse.sh"),
                    no_agent=snapshot.job_dict.get("no_agent", False),
                )
            else:
                # Re-create.
                _cronjob_create(
                    schedule=snapshot.job_dict.get("schedule", "every 2m"),
                    name=job_name,
                    script=snapshot.job_dict.get("script", "caduceus-pulse.sh"),
                    no_agent=snapshot.job_dict.get("no_agent", False),
                )
            job_restored = True
        except Exception:
            pass
    else:
        # Snapshot had no job — ensure it's gone.
        for job in matches:
            try:
                _cronjob_remove(str(job.get("id")))
            except Exception:
                pass
        job_restored = True  # best-effort

    # Step 6: Check if both were restored.
    if wrapper_restored and job_restored:
        return None

    return _NeedsAttention(
        recovery_evidence=(
            f"rollback {'partial' if wrapper_restored or job_restored else 'failed'}: "
            f"wrapper_restored={wrapper_restored}, "
            f"job_restored={job_restored}, "
            f"snapshot_bytes={len(snapshot.wrapper_bytes)}B, "
            f"snapshot_job={'present' if snapshot.job_dict else 'absent'}"
        )
    )


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

    Flow (AC-01/03/09):
    1. Snapshot the wrapper and job registry BEFORE any mutation.
    2. Write the pulse wrapper.
    3. Check the cron capability (may raise CronCapabilityError).
    4. Create or update the cron job.
    5. On any error → reconcile from snapshot.
    """
    binary = _binary_path()
    if not binary.is_file():
        raise RuntimeError("caduceus binary not built; run `hermes caduceus setup`")

    # Step 1: Snapshot before any mutation.
    try:
        registry = _cron_job_registry()
    except Exception as exc:
        raise RuntimeError(f"cannot list cron jobs: {exc}") from exc
    wrapper_path = _pulse_wrapper_path()
    snapshot = _snapshot_wrapper_and_job(wrapper_path, "caduceus", registry)

    # Step 2: Write the pulse wrapper (unconditional rewrite).
    _write_pulse_wrapper(binary)

    # Step 3: Check cron capability (may raise CronCapabilityError).
    # The registry is re-read here because the list may fail.
    try:
        cronjob = _cron_job_registry()
    except Exception as exc:
        # Reconcile: the wrapper was written but we cannot proceed.
        result = _reconcile_after_error(
            error=exc, snapshot=snapshot, ctx=None,
            job_name="caduceus", intended_state="present",
        )
        if isinstance(result, _NeedsAttention):
            raise RuntimeError(f"cron install failed, {result.recovery_evidence}") from exc
        raise RuntimeError(f"cron job registry unavailable: {exc}") from exc

    matches = [job for job in cronjob.values() if job.get("name") == "caduceus"]
    if len(matches) > 1:
        ids = ", ".join(sorted(str(j.get("id")) for j in matches))
        raise RuntimeError(f"multiple caduceus cron jobs found: {ids}")

    if dry_run:
        return (("created" if not matches else "reused"), "dry-run")

    # Step 4: Create or update.
    try:
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
    except Exception as exc:
        # Step 5: Reconcile on error.
        result = _reconcile_after_error(
            error=exc, snapshot=snapshot, ctx=None,
            job_name="caduceus", intended_state="present",
        )
        if isinstance(result, _NeedsAttention):
            raise RuntimeError(
                f"cron install failed and requires attention: "
                f"{result.recovery_evidence}"
            ) from exc
        # Reconcile succeeded — re-raise to let caller know the error occurred.
        raise RuntimeError(f"cron install failed (reconciled): {exc}") from exc


def _write_pulse_wrapper(binary: Path) -> None:
    """Atomically write the ``caduceus-pulse.sh`` wrapper.

    The wrapper contains the absolute installed binary path and uses
    ``exec`` so the cron process replaces its shell with the daemon.
    """
    path = _pulse_wrapper_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    body = (
        "#!/usr/bin/env bash\n"
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


_CRON_CATEGORY_PLAIN_ENGLISH = {
    "denied": "Hermes refused the cron operation",
    "timed-out": "Hermes cron bridge did not respond",
    "eof": "Hermes cron bridge closed unexpectedly",
    "crashed": "Hermes cron bridge crashed",
    "duplicate-name": "A job named 'caduceus' already exists — run `hermes caduceus cron-remove` first",
    "foreign-name-collision": "A different plugin owns a 'caduceus' cron job",
    "malformed-response": "Hermes returned an unexpected payload shape",
}


_CRON_CATEGORY_NEXT_ACTION = {
    "denied": "check that the Hermes gateway is running and the cron subsystem is enabled, then retry",
    "timed-out": "check Hermes host load and the cronjob tool timeout, then retry",
    "eof": "re-run `hermes plugins install --enable` to refresh the adapter, then re-check",
    "crashed": "re-run `hermes plugins install --enable` to refresh the adapter, then re-check",
    "duplicate-name": "run `hermes caduceus cron-remove` first, then re-run `hermes caduceus cron-install`",
    "foreign-name-collision": "run `hermes caduceus cron-remove` to clear the collision, then re-install",
    "malformed-response": "re-run `hermes plugins install --enable` to refresh the adapter, then re-check",
}


def _format_cron_finding(
    *,
    label: str,
    ok: bool,
    detail: str,
    next_action: str = "",
    internal_detail: str = "",
    verbose: bool = False,
) -> str:
    """Return a single operator-readable line for a cron subcommand.

    Mirrors the SEP-01 contract from ``_cli_doctor``: ``internal_detail``
    and structured categories are only emitted when ``verbose`` is True.
    """
    prefix = "[OK]" if ok else "[FAIL]"
    line = f"{prefix} {label} — {detail}"
    if next_action:
        line += f" — {next_action}"
    if verbose and internal_detail:
        line += f"\n       internal: {internal_detail}"
    return line


def _unwrap_cron_capability_error(
    exc: BaseException,
) -> Optional["CronCapabilityError"]:
    """Walk ``__cause__`` / ``__context__`` to find a ``CronCapabilityError``."""
    from . import _runtime as rt  # type: ignore[import-not-found]

    cur: Optional[BaseException] = exc
    seen: set = set()
    while cur is not None and id(cur) not in seen:
        seen.add(id(cur))
        if isinstance(cur, rt.CronCapabilityError):
            return cur
        cur = getattr(cur, "__cause__", None) or getattr(cur, "__context__", None)
    return None


def _format_cron_failure(
    label: str,
    exc: BaseException,
    verbose: bool = False,
) -> str:
    """Map a cron exception to an operator-readable failure string."""
    from . import _runtime as rt  # type: ignore[import-not-found]

    cron_err = _unwrap_cron_capability_error(exc)
    if cron_err is not None:
        detail = _CRON_CATEGORY_PLAIN_ENGLISH.get(
            cron_err.category,
            f"{cron_err.category} — {cron_err.detail}",
        )
        next_action = _CRON_CATEGORY_NEXT_ACTION.get(
            cron_err.category,
            "re-run `hermes plugins install --enable` to refresh the adapter, then re-check",
        )
        internal_detail = cron_err.internal_detail or str(exc)
    else:
        detail = str(exc)
        if detail.startswith("caduceus binary not built"):
            detail = (
                "the Caduceus binary has not been built — run "
                "`hermes caduceus setup` first"
            )
            next_action = "then re-run the command"
        else:
            next_action = (
                "check the output above and retry, or re-run with "
                "`--verbose` for the internal detail"
            )
        internal_detail = str(exc)

    return _format_cron_finding(
        label=label,
        ok=False,
        detail=detail,
        next_action=next_action,
        internal_detail=internal_detail,
        verbose=verbose,
    )


def _cli_cron_install(*, dry_run: bool, verbose: bool = False) -> int:
    try:
        _cron_install(dry_run=dry_run)
    except RuntimeError as exc:
        print(
            _format_cron_failure("cron-install", exc, verbose=verbose),
            file=sys.stderr,
        )
        return 1
    print(
        _format_cron_finding(
            label="cron-install",
            ok=True,
            detail="registered caduceus pulse at */2 * * * *",
            next_action="wrapper at ~/.hermes/scripts/caduceus-pulse.sh",
            verbose=verbose,
        )
    )
    return 0


def _cli_cron_remove(*, verbose: bool = False) -> int:
    from . import _runtime as rt  # type: ignore[import-not-found]

    try:
        # Step 1: Snapshot before any mutation.
        cronjob = _cron_job_registry()
        wrapper_path = _pulse_wrapper_path()
        snapshot = _snapshot_wrapper_and_job(wrapper_path, "caduceus", cronjob)
        matches = [job for job in cronjob.values() if job.get("name") == "caduceus"]
    except (RuntimeError, rt.CronCapabilityError) as exc:
        print(
            _format_cron_failure("cron-remove", exc, verbose=verbose),
            file=sys.stderr,
        )
        return 1

    # Step 2: Remove the cron job(s).
    error = None
    for job in matches:
        try:
            _cronjob_remove(str(job.get("id")))
        except (RuntimeError, rt.CronCapabilityError) as exc:
            error = exc
            break

    # Step 3: Remove the wrapper file.
    wrapper_removed = True
    if wrapper_path.is_file() or wrapper_path.is_symlink():
        try:
            wrapper_path.unlink()
        except OSError as exc:
            error = error or exc
            wrapper_removed = False

    # Step 4: If anything failed, reconcile.
    if error is not None:
        try:
            result = _reconcile_after_error(
                error=error, snapshot=snapshot, ctx=None,
                job_name="caduceus", intended_state="absent",
            )
        except Exception as reconcile_error:
            print(
                _format_cron_failure(
                    "cron-remove", reconcile_error, verbose=verbose
                ),
                file=sys.stderr,
            )
            return 1

        if isinstance(result, _NeedsAttention):
            print(
                _format_cron_failure(
                    "cron-remove",
                    RuntimeError(result.recovery_evidence),
                    verbose=verbose,
                ),
                file=sys.stderr,
            )
            return 1

        if not wrapper_removed:
            # Reconcile cleaned up but wrapper still present — report.
            print(
                _format_cron_finding(
                    label="cron-remove",
                    ok=False,
                    detail="cron job removed, but the wrapper could not be removed",
                    next_action="manual cleanup of ~/.hermes/scripts/caduceus-pulse.sh may be needed",
                    internal_detail=str(error),
                    verbose=verbose,
                ),
                file=sys.stderr,
            )
            return 1

    print(
        _format_cron_finding(
            label="cron-remove",
            ok=True,
            detail="removed the Caduceus cron job and wrapper",
            verbose=verbose,
        )
    )
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
    "_Snapshot",
    "_NeedsAttention",
    "_DoctorFinding",
    "_handle_caduceus_status",
    "_register_caduceus_cli",
    "_caduceus_cli_command",
    "_plugin_root",
    "_binary_path",
    "_bridge_template_path",
    "_pulse_template_path",
]
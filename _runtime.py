"""Hermes-side runtime helpers for the Caduceus adapter.

Kept in a sibling module so the import surface of ``__init__.py`` is
minimal and so tests can substitute a fake cron registry without
patching the registration entry point.

All functions are stdlib-only and return plain Python dicts / strings.
Starting with Hermes v0.19.0 the host no longer exposes a ``cronjob`` MCP
tool, so Caduceus reaches the cron subsystem by spawning
``hermes cron <list|create|remove>`` via ``subprocess.run``.
"""

from __future__ import annotations

import json
import re
import shutil
import subprocess
from typing import Any, Callable, Dict, Optional, Union


# ---------------------------------------------------------------------------
# Exceptions
# ---------------------------------------------------------------------------


class CronCapabilityError(Exception):
    """Raised when a cron capability response is invalid or rejected.

    Attributes:
        category: A short machine-readable category string identifying the
            error type (e.g. ``"malformed-response"``, ``"denied"``,
            ``"timed-out"``, ``"eof"``, ``"crashed"``, ``"duplicate-name"``,
            ``"foreign-name-collision"``).
        detail: A human-readable description of what went wrong.
        internal_detail: Internal diagnostic for verbose output, never
            shown to operators by default.
    """

    def __init__(
        self,
        category: str,
        detail: str,
        internal_detail: Optional[str] = None,
    ) -> None:
        self.category = category
        self.detail = detail
        self.internal_detail = internal_detail
        super().__init__(f"{category}: {detail}")


# ---------------------------------------------------------------------------
# Subprocess helper
# ---------------------------------------------------------------------------


_HERMES_TIMEOUT_SECONDS = 30


class _Missing:
    """Sentinel distinguishing "not yet resolved" from "resolved and absent"."""


_MISSING = _Missing()

_HERMES_PATH: Union[str, None, _Missing] = _MISSING


def _resolve_hermes() -> str:
    """Return the absolute path to the ``hermes`` binary, or raise.

    Resolution is cached per process using ``shutil.which("hermes")``.
    A missing binary is reported as a ``CronCapabilityError`` so callers
    can wrap it in SEP-01 anti-leak output.
    """
    global _HERMES_PATH
    if isinstance(_HERMES_PATH, _Missing):
        _HERMES_PATH = shutil.which("hermes")
    if _HERMES_PATH is None:
        raise CronCapabilityError(
            "denied",
            "hermes CLI not on PATH",
            "shutil.which('hermes') returned None",
        )
    return _HERMES_PATH


def _subprocess_run(
    argv: list,
    *,
    timeout: int = _HERMES_TIMEOUT_SECONDS,
) -> "subprocess.CompletedProcess[str]":
    """Spawn *argv* with bounded output and timeout.

    ``subprocess.run`` is captured behind this indirection so tests can
    feed scripted ``CompletedProcess`` responses without touching real
    host state. All subprocess exceptions are mapped to
    ``CronCapabilityError`` categories that the adapter already knows how
    to render to operators.
    """
    try:
        return subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        raise CronCapabilityError(
            "timed-out",
            "hermes cron command did not respond within 30s",
            " ".join(map(str, argv)),
        ) from exc
    except FileNotFoundError as exc:
        raise CronCapabilityError(
            "denied",
            "hermes CLI not found on PATH",
            str(exc),
        ) from exc
    except OSError as exc:
        raise CronCapabilityError(
            "denied",
            "failed to spawn hermes CLI",
            str(exc),
        ) from exc


# Error prefixes that ``hermes cron`` may emit on stderr. If a non-zero
# exit begins with ``<category>: ...`` we preserve the category so the
# adapter can surface a precise next action.
_KNOWN_STDERR_CATEGORIES = frozenset(
    {
        "denied",
        "timed-out",
        "eof",
        "crashed",
        "duplicate-name",
        "foreign-name-collision",
        "malformed-response",
    }
)


def _category_from_stderr(stderr: str) -> tuple[str, str]:
    """Infer a ``CronCapabilityError`` category from the first stderr line."""
    if not stderr or not stderr.strip():
        return "denied", "hermes cron command failed"
    first = stderr.strip().splitlines()[0]
    if ":" in first:
        prefix, rest = first.split(":", 1)
        prefix = prefix.strip()
        if prefix in _KNOWN_STDERR_CATEGORIES:
            detail = rest.strip() or "hermes cron command failed"
            return prefix, detail
    return "denied", first


# ---------------------------------------------------------------------------
# hermes cron list parser
# ---------------------------------------------------------------------------


_BLOCK_RE = re.compile(r"^  ([0-9a-f]+) \[(active|disabled)\]$")
_FIELD_RE = re.compile(r"^    ([A-Za-z ]+?):\s{2,}(.+)$")
_BANNER_RE = re.compile(r"^[┌│└─]")
_HEADER_RE = re.compile(r"^\s*Scheduled Jobs\s*$")
_JOB_ID_RE = re.compile(r"[0-9a-f]{8,}")


def _parse_cron_list_table(stdout: Optional[str]) -> Dict[str, Dict[str, Any]]:
    """Parse ``hermes cron list --all`` stdout into ``{job_id: job_dict}``.

    The human-readable table is structured as a banner followed by blocks
    starting with two spaces, a hex job id, and a status bracket. Each
    block is followed by four-space-indented ``Label:      value`` lines.
    Unknown labels are preserved verbatim; ``Last run`` values may carry a
    trailing ``ok`` / ``fail`` token which is split into
    ``last_run_status``.

    Non-table output (including ``"No cron jobs found"``) and banner-only
    output produce an empty dict without raising.
    """
    jobs: Dict[str, Dict[str, Any]] = {}
    if stdout is None:
        return jobs
    current: Optional[Dict[str, Any]] = None
    for raw in stdout.splitlines():
        line = raw.rstrip()
        if not line.strip() or _BANNER_RE.match(line) or _HEADER_RE.match(line):
            continue
        match = _BLOCK_RE.match(line)
        if match:
            job_id = match.group(1)
            status = match.group(2)
            current = {"id": job_id, "status": status}
            jobs[job_id] = current
            continue
        match = _FIELD_RE.match(line)
        if match and current is not None:
            label = match.group(1).strip()
            value = match.group(2).strip()
            key = label.lower().replace(" ", "_")
            if key == "last_run":
                parts = value.rsplit(None, 1)
                if len(parts) == 2 and parts[1] in ("ok", "fail"):
                    current["last_run"] = parts[0]
                    current["last_run_status"] = parts[1]
                else:
                    current["last_run"] = value
            else:
                current[key] = value
    return jobs


def _extract_job_id(stdout: Optional[str]) -> Optional[str]:
    """Return the first plausible hex job id found in command stdout."""
    if not stdout:
        return None
    match = _JOB_ID_RE.search(stdout)
    if match:
        return match.group(0)
    return None


# ---------------------------------------------------------------------------
# Cron operations
# ---------------------------------------------------------------------------


def _dispatch_list() -> Dict[str, Dict[str, Any]]:
    """Run ``hermes cron list --all`` and parse the table."""
    _resolve_hermes()
    proc = _subprocess_run(["hermes", "cron", "list", "--all"], timeout=_HERMES_TIMEOUT_SECONDS)
    if proc.returncode != 0:
        category, detail = _category_from_stderr(proc.stderr or "")
        raise CronCapabilityError(category, detail, _redact_stderr(proc.stderr or ""))
    return _parse_cron_list_table(proc.stdout)


def _dispatch_create(
    *,
    schedule: str,
    name: str,
    script: str,
    no_agent: bool,
) -> str:
    """Run ``hermes cron create`` and return the new job id."""
    _resolve_hermes()
    argv = [
        "hermes",
        "cron",
        "create",
        schedule,
        "--name",
        name,
        "--script",
        script,
    ]
    if no_agent:
        argv.append("--no-agent")
    proc = _subprocess_run(argv, timeout=_HERMES_TIMEOUT_SECONDS)
    if proc.returncode != 0:
        category, detail = _category_from_stderr(proc.stderr or "")
        raise CronCapabilityError(category, detail, _redact_stderr(proc.stderr or ""))
    job_id = _extract_job_id(proc.stdout or "")
    if job_id is not None:
        return job_id
    # Fallback: list jobs and match by name. Hermes does not guarantee a
    # job id on stdout, but the job should be visible immediately.
    jobs = _dispatch_list()
    for jid, job in jobs.items():
        if job.get("name") == name:
            return jid
    raise CronCapabilityError(
        "malformed-response",
        "created job id could not be extracted from stdout or list output",
        proc.stdout,
    )


def _dispatch_remove(job_id: str) -> None:
    """Run ``hermes cron remove <job_id>``."""
    _resolve_hermes()
    proc = _subprocess_run(
        ["hermes", "cron", "remove", job_id],
        timeout=_HERMES_TIMEOUT_SECONDS,
    )
    if proc.returncode != 0:
        category, detail = _category_from_stderr(proc.stderr or "")
        raise CronCapabilityError(category, detail, _redact_stderr(proc.stderr or ""))


def _redact_stderr(stderr: str) -> str:
    """Best-effort redaction of sensitive tokens before the error is logged."""
    if not stderr:
        return ""
    redacted = stderr
    for needle in ("GITHUB_TOKEN", "CADUCEUS_GITHUB_TOKEN", "GH_TOKEN"):
        if needle in redacted:
            redacted = re.sub(
                rf"({re.escape(needle)}\s*=\s*)(['\"]?[^\s'\"]+['\"]?)",
                lambda m: f"{m.group(1)}<redacted>",
                redacted,
            )
    return redacted


def cron_list_jobs() -> Dict[str, Dict[str, Any]]:
    result = _dispatch_list()
    return _coerce_jobs(result)


def cron_create_job(*, schedule: str, name: str, script: str, no_agent: bool) -> str:
    return _dispatch_create(schedule=schedule, name=name, script=script, no_agent=no_agent)


def cron_update_job(
    *, job_id: str, schedule: str, name: str, script: str, no_agent: bool
) -> None:
    # Hermes does not expose a cron update subcommand; remove the old job
    # and recreate it with the intended configuration.
    _dispatch_remove(job_id)
    _dispatch_create(schedule=schedule, name=name, script=script, no_agent=no_agent)


def cron_remove_job(job_id: str) -> None:
    _dispatch_remove(job_id)


# ---------------------------------------------------------------------------
# Backward-compat no-op shims
# ---------------------------------------------------------------------------


_INSTALL_DISPATCHER_DOC = (
    "In versions before Hermes v0.19.0 this installed the dispatcher for the "
    "deprecated ``cronjob`` MCP tool. Cron is now reached through the ``hermes`` "
    "CLI subprocess, so this call is ignored. Kept so legacy importers and tests "
    "do not break during migration."
)


_DISPATCHER: Optional[Callable[[str, Dict[str, Any]], Any]] = None


def install_dispatcher(dispatcher: Callable[[str, Dict[str, Any]], Any]) -> None:
    """No-op shim that ignores the supplied dispatcher.

    %s
    """ % _INSTALL_DISPATCHER_DOC
    # Deliberately no-op: production code never reads ``_DISPATCHER``.
    del dispatcher


def reset_dispatcher() -> None:
    """Reset the cron runtime hooks used by tests.

    This clears the deprecated ``_DISPATCHER`` reference and, importantly,
    restores the default ``subprocess.run`` indirection and the Hermes path
    cache so tests cannot leak monkeypatches across cases.
    """
    global _DISPATCHER, _subprocess_run, _HERMES_PATH
    _DISPATCHER = None
    _subprocess_run = subprocess.run
    _HERMES_PATH = _MISSING


# ---------------------------------------------------------------------------
# Coercion helpers
# ---------------------------------------------------------------------------


def _coerce_jobs(result: Any) -> Dict[str, Dict[str, Any]]:
    """Return ``{job_id: job_dict, ...}`` from whatever we receive.

    Supports three production shapes and a handful of legacy test shapes:

    * ``Dict[str, Dict[str, Any]]`` keyed by job id (the parsed table shape).
    * A JSON string encoding a ``{jobs: [...]}`` or ``[...]`` payload.
    * ``None`` / empty containers, meaning no jobs registered.

    Raises
    ------
    CronCapabilityError
        If the response is malformed, denied, timed-out, EOF, crashed, or
        contains duplicate or foreign-name collisions.
    """
    if result is None:
        # No response — empty cron list, no error.
        return {}
    if isinstance(result, str):
        try:
            parsed = json.loads(result)
        except json.JSONDecodeError:
            raise CronCapabilityError(
                "malformed-response",
                "cron bridge returned an unparseable response",
                internal_detail=result,
            ) from None
        if isinstance(parsed, dict) and "error" in parsed:
            category = parsed.get("category")
            if not isinstance(category, str) or not category:
                category = "denied"
            raise CronCapabilityError(
                category,
                str(parsed.get("error", "cron capability denied")),
                internal_detail=result,
            ) from None
        # Parsed value falls through to the existing shape branches.
        return _coerce_jobs(parsed)
    if isinstance(result, dict) and "jobs" in result and isinstance(result["jobs"], list):
        # Empty jobs list is valid — no jobs registered.
        if not result["jobs"]:
            return {}
        return {str(job["id"]): job for job in result["jobs"] if "id" in job}
    if isinstance(result, list):
        if not result:
            # Empty list — no jobs registered, no error.
            return {}
        return {str(job["id"]): job for job in result if isinstance(job, dict) and "id" in job}
    if isinstance(result, dict):
        # Already keyed by job id.
        if not result:
            # Empty dict — no jobs, no error.
            return {}
        return {str(k): v for k, v in result.items() if isinstance(v, dict)}
    raise CronCapabilityError(
        "malformed-response",
        f"unexpected cron list response type: {type(result).__name__}",
    )

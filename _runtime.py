"""Hermes-side runtime helpers for the Caduceus adapter.

Kept in a sibling module so the import surface of ``__init__.py`` is
minimal and so tests can substitute a fake cron registry without
patching the registration entry point.

All functions are stdlib-only and return plain Python dicts / strings.
They invoke Hermes's ``cronjob`` tool through a deferred hook so the
adapter can be exercised in environments without the cron subsystem
loaded (notably, the ``pytest`` environment).
"""

from __future__ import annotations

from typing import Any, Callable, Dict, Optional


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
    """

    def __init__(self, category: str, detail: str) -> None:
        self.category = category
        self.detail = detail
        super().__init__(f"{category}: {detail}")


# ---------------------------------------------------------------------------
# Cronjob bridge
# ---------------------------------------------------------------------------


_DISPATCHER: Optional[Callable[[str, Dict[str, Any]], Any]] = None


def install_dispatcher(dispatcher: Callable[[str, Dict[str, Any]], Any]) -> None:
    """Install the ``ctx.dispatch_tool("cronjob", {...})`` callable.

    The plugin's ``register(ctx)`` installs this once, then ``__init__``
    goes through ``cron_list_jobs`` etc. The test suite installs a stub
    instead so the adapter can be exercised without a live Hermes CLI.
    """
    global _DISPATCHER
    _DISPATCHER = dispatcher


def reset_dispatcher() -> None:
    """Clear the cronjob dispatcher. Used by the test suite."""
    global _DISPATCHER
    _DISPATCHER = None


def _dispatch(action: str, **fields: Any) -> Any:
    """Invoke ``ctx.dispatch_tool("cronjob", {"action": ..., ...})``."""
    if _DISPATCHER is None:
        raise RuntimeError(
            "Caduceus cronjob bridge not initialised. The adapter must be "
            "registered through Hermes with a live plugin context, or tests "
            "must call `install_dispatcher(...)` first."
        )
    payload = {"action": action}
    payload.update(fields)
    return _DISPATCHER("cronjob", payload)


def cron_list_jobs() -> Dict[str, Dict[str, Any]]:
    result = _dispatch("list")
    return _coerce_jobs(result)


def cron_create_job(*, schedule: str, name: str, script: str, no_agent: bool) -> str:
    result = _dispatch(
        "create",
        schedule=schedule,
        name=name,
        script=script,
        no_agent=no_agent,
    )
    if isinstance(result, dict) and "id" in result:
        return str(result["id"])
    if isinstance(result, str):
        return result
    raise RuntimeError(f"unexpected cron create response: {result!r}")


def cron_update_job(
    *, job_id: str, schedule: str, name: str, script: str, no_agent: bool
) -> None:
    _dispatch(
        "update",
        job_id=job_id,
        schedule=schedule,
        name=name,
        script=script,
        no_agent=no_agent,
    )


def cron_remove_job(job_id: str) -> None:
    _dispatch("remove", job_id=job_id)


def _coerce_jobs(result: Any) -> Dict[str, Dict[str, Any]]:
    """Return ``{job_id: job_dict, ...}`` from whatever the dispatcher returns.

    Hermes's ``cronjob`` action=``list`` returns either a dict mapping ids
    to job dicts, or a list of job dicts (each with ``id``). Both shapes
    are accepted so the adapter does not depend on the wire format.

    Raises
    ------
    CronCapabilityError
        If the response is malformed (None, non-dict/non-list), denied,
        timed-out, EOF, crashed, or contains duplicate or foreign-name
        collisions.
    """
    if result is None:
        # No response — empty cron list, no error.
        return {}
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
"""Cron capability simulators for the Caduceus Hermes plugin test suite."""

from __future__ import annotations

import json
from typing import Any, Dict

from caduceus._runtime import CronCapabilityError


# ---------------------------------------------------------------------------
# Table fixtures (new subprocess path)
# ---------------------------------------------------------------------------

_HERMES_CRON_LIST_HEADER = (
    "\u250c\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2510\n"
    "\u2502 Scheduled Jobs \u2502\n"
    "\u2514\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2518\n"
)


def well_formed_table(stdout: str | None = None) -> str:
    """Return a well-formed ``hermes cron list --all`` table with one caduceus job."""
    if stdout is not None:
        return stdout
    return (
        _HERMES_CRON_LIST_HEADER
        + "  abc [active]\n"
        "    Name:      caduceus\n"
        "    Schedule:  every 2m\n"
        "    Repeat:    yes\n"
        "    Next run:  2026-07-24T18:10:00Z\n"
        "    Deliver:   stdout\n"
        "    Script:    caduceus-pulse.sh\n"
        "    Mode:      no-agent (script stdout delivered directly)\n"
        "    Last run:  2026-07-24T18:08:00Z  ok\n"
        "    Execution:  completed  exec-01\n"
    )


def empty_table() -> str:
    """Return a banner-only table (no job blocks)."""
    return _HERMES_CRON_LIST_HEADER


def disabled_job_table() -> str:
    """Return a table containing one active and one disabled job."""
    return (
        _HERMES_CRON_LIST_HEADER
        + "  abc [active]\n"
        "    Name:      caduceus\n"
        "    Schedule:  every 2m\n"
        "    Script:    caduceus-pulse.sh\n"
        "    Mode:      no-agent\n"
        "\n"
        "  1a2b [disabled]\n"
        "    Name:      other-job\n"
        "    Schedule:  every 5m\n"
        "    Deliver:   stderr\n"
        "    Script:    /tmp/x.sh\n"
        "    Workdir:   /home/agent\n"
    )


def create_stdout(job_id: str = "deadbeef") -> str:
    """Return a realistic ``hermes cron create`` stdout containing *job_id*."""
    return f"Created cron job {job_id}\n"


def create_stdout_fallback_only_lists() -> str:
    """Return create stdout without a job id, forcing the list fallback."""
    return "Created cron job caduceus\n"


# ---------------------------------------------------------------------------
# Simulator factories
# ---------------------------------------------------------------------------


def well_formed(name: str, args: Dict[str, Any]) -> Dict[str, Any]:
    """Return a well-formed job list with one caduceus job."""
    return {
        "jobs": [{"id": "abc", "name": "caduceus", "schedule": "every 2m"}]
    }


def malformed(name: str, args: Dict[str, Any]) -> Any:
    """Return a non-dict, non-list value — simulates a malformed dispatch."""
    return "garbled"


def denied(name: str, args: Dict[str, Any]) -> None:
    """Raise CronCapabilityError — simulates capability denial."""
    raise CronCapabilityError("denied", "cron denied")


def timed_out(name: str, args: Dict[str, Any]) -> None:
    """Raise CronCapabilityError — simulates a Hermes timeout."""
    raise CronCapabilityError("timed-out", "cron timed out")


def eof(name: str, args: Dict[str, Any]) -> None:
    """Raise CronCapabilityError — simulates end-of-stream from Hermes."""
    raise CronCapabilityError("eof", "cron capability returned EOF")


def crashed(name: str, args: Dict[str, Any]) -> None:
    """Raise CronCapabilityError — simulates a Hermes internal crash."""
    raise CronCapabilityError("crashed", "cron crashed")


def duplicate(name: str, args: Dict[str, Any]) -> Dict[str, Any]:
    """Return a list with two jobs sharing the same name \"caduceus\"."""
    return {
        "jobs": [
            {"id": "abc", "name": "caduceus", "schedule": "every 2m"},
            {"id": "def", "name": "caduceus", "schedule": "every 5m"},
        ]
    }


def foreign_name_collision(name: str, args: Dict[str, Any]) -> Dict[str, Any]:
    """Return a list where a non-caduceus job has the name \"caduceus\"."""
    return {
        "jobs": [
            {"id": "other", "name": "caduceus", "schedule": "every 2m"},
        ]
    }


def absent(name: str, args: Dict[str, Any]) -> None:
    """Return None — simulates a missing capability (no raise)."""
    return None


def real_hermes(name: str, args: Dict[str, Any]) -> str:
    """Return the documented cronjob list success shape as a JSON string."""
    jobs = [{"id": "caduceus", "name": "caduceus", "schedule": "every 2m"}]
    return json.dumps({"success": True, "count": len(jobs), "jobs": jobs}, indent=2)


def real_hermes_empty(name: str, args: Dict[str, Any]) -> str:
    """Return the empty-success JSON string (count: 0)."""
    return json.dumps({"success": True, "count": 0, "jobs": []}, indent=2)


def error_envelope(name: str, args: Dict[str, Any]) -> str:
    """Return the registry error envelope as a JSON string."""
    return json.dumps({"error": "permission denied"})


# ---------------------------------------------------------------------------
# Registry
# ---------------------------------------------------------------------------


SIMULATORS: Dict[str, Any] = {
    "well_formed": well_formed,
    "malformed": malformed,
    "denied": denied,
    "timed_out": timed_out,
    "eof": eof,
    "crashed": crashed,
    "duplicate": duplicate,
    "foreign_name_collision": foreign_name_collision,
    "absent": absent,
    "real_hermes": real_hermes,
    "real_hermes_empty": real_hermes_empty,
    "error_envelope": error_envelope,
}


def get_simulator(category: str) -> Any:
    """Return the simulator callable for *category*, or raise ValueError."""
    fn = SIMULATORS.get(category)
    if fn is None:
        raise ValueError(f"unknown cron capability category: {category!r}")
    return fn
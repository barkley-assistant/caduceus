"""Cron capability simulators for the Caduceus Hermes plugin test suite.

Each simulator is a ``dispatch_tool``-compatible callable
``(name: str, args: dict) -> Any`` that returns a pre-determined response
matching a real Hermes cron capability failure mode.  The simulators let
the test suite exercise every dispatch-tool path without a live Hermes
binary.

Categories
----------
* ``well_formed`` — returns a valid job list.
* ``malformed`` — returns ``None`` (simulates a broken dispatch).
* ``denied`` — raises ``RuntimeError("cron denied")`` (simulates a
  capability rejection).
* ``timed_out`` — raises ``TimeoutError("cron timed out")`` (simulates
  a Hermes-side timeout).
* ``eof`` — returns ``{"jobs": []}`` (simulates an empty stream).
* ``crashed`` — raises ``RuntimeError("cron crashed")`` (simulates
  an internal Hermes crash).
* ``duplicate`` — returns a list with two jobs sharing the same name
  (simulates a name collision).
* ``foreign_name_collision`` — returns a list where a non-caduceus job
  has the name ``"caduceus"`` (simulates a foreign-name collision).
* ``absent`` — returns ``None`` *without* raising (simulates a missing
  capability).
"""

from __future__ import annotations

from typing import Any, Dict

from caduceus._runtime import CronCapabilityError


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
}


def get_simulator(category: str) -> Any:
    """Return the simulator callable for *category*, or raise ValueError."""
    fn = SIMULATORS.get(category)
    if fn is None:
        raise ValueError(f"unknown cron capability category: {category!r}")
    return fn
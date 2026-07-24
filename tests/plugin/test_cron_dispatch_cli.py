"""CRONCLI-05: unit tests for the hermes cron CLI subprocess dispatcher."""

from __future__ import annotations

import subprocess
from typing import List

import pytest

from tests.fixtures.capability_simulator import (
    create_stdout,
    disabled_job_table,
    empty_table,
    well_formed_table,
)
from tests.plugin._helpers import subprocess_run_recorder


def _job_ids(calls: List[List[str]]) -> List[str]:
    """Return the operation names (argv[2]) captured by the recorder."""
    return [c[2] for c in calls]


def test_subprocess_returns_well_formed_table_parses_to_jobs_dict() -> None:
    """CRONCLI-05: ``hermes cron list --all`` stdout parses to ``{id: job_dict}`` (design §Table parser)."""
    from caduceus import _runtime

    with subprocess_run_recorder({"list": well_formed_table()}) as calls:
        jobs = _runtime.cron_list_jobs()

    assert calls[0] == ["hermes", "cron", "list", "--all"]
    assert "abc" in jobs
    job = jobs["abc"]
    assert job["id"] == "abc"
    assert job["status"] == "active"
    assert job["name"] == "caduceus"
    assert job["schedule"] == "every 2m"
    assert job["repeat"] == "yes"
    assert job["script"] == "caduceus-pulse.sh"
    assert job["last_run"] == "2026-07-24T18:08:00Z"
    assert job["last_run_status"] == "ok"
    assert job["execution"] == "completed  exec-01"


def test_subprocess_returns_empty_list_returns_empty_dict() -> None:
    """CRONCLI-05: banner-only output returns ``{}``."""
    from caduceus import _runtime

    with subprocess_run_recorder({"list": empty_table()}) as calls:
        jobs = _runtime.cron_list_jobs()

    assert jobs == {}


def test_subprocess_timeout_raises_cron_capability_error_with_operator_readable_message() -> None:
    """CRONCLI-05: a timeout maps to category ``timed-out``."""
    from caduceus import _runtime

    with subprocess_run_recorder(
        {"list": _runtime.CronCapabilityError("timed-out", "hermes cron timed out")}
    ):
        with pytest.raises(_runtime.CronCapabilityError) as excinfo:
            _runtime.cron_list_jobs()

    assert excinfo.value.category == "timed-out"
    assert "timed out" in excinfo.value.detail.lower()


def test_subprocess_returns_non_table_text_returns_empty_dict() -> None:
    """CRONCLI-05: non-table stdout returns ``{}`` instead of leaking raw text."""
    from caduceus import _runtime

    stdout = "No scheduled jobs.\nCreate one with 'hermes cron create ...'\n"
    with subprocess_run_recorder({"list": stdout}) as calls:
        jobs = _runtime.cron_list_jobs()

    assert jobs == {}


def test_subprocess_exits_nonzero_raises_cron_capability_error_with_stderr_as_internal_detail() -> None:
    """CRONCLI-05: non-zero exit surfaces category ``denied`` and preserves stderr."""
    from caduceus import _runtime

    def fail_list(argv, kwargs):
        return subprocess.CompletedProcess(
            argv, 1, "", "denied: permission denied"
        )

    with subprocess_run_recorder({"list": fail_list}):
        with pytest.raises(_runtime.CronCapabilityError) as excinfo:
            _runtime.cron_list_jobs()

    assert excinfo.value.category == "denied"
    assert "permission denied" in excinfo.value.detail.lower()
    assert "permission denied" in excinfo.value.internal_detail


def test_hermes_not_on_path_raises_denied() -> None:
    """CRONCLI-05: ``_resolve_hermes`` miss raises ``denied`` before spawning."""
    from caduceus import _runtime

    with subprocess_run_recorder({"list": ""}) as calls:
        _runtime._HERMES_PATH = None  # simulate which() miss
        with pytest.raises(_runtime.CronCapabilityError) as excinfo:
            _runtime.cron_list_jobs()

    assert excinfo.value.category == "denied"
    assert "hermes cli not on path" in excinfo.value.detail.lower()
    assert not any(c[2] == "list" for c in calls)


def test_cron_list_with_disabled_jobs_uses_all_flag() -> None:
    """CRONCLI-05: list includes ``--all`` so disabled jobs are visible."""
    from caduceus import _runtime

    with subprocess_run_recorder({"list": disabled_job_table()}) as calls:
        jobs = _runtime.cron_list_jobs()

    assert calls[0] == ["hermes", "cron", "list", "--all"]
    assert "abc" in jobs
    assert "1a2b" in jobs
    assert jobs["1a2b"]["status"] == "disabled"
    assert jobs["1a2b"]["name"] == "other-job"


def test_cron_create_returns_extracted_job_id_from_stdout() -> None:
    """CRONCLI-05: create extracts the hex job id from stdout."""
    from caduceus import _runtime

    with subprocess_run_recorder({"create": create_stdout("deadbeef")}) as calls:
        job_id = _runtime.cron_create_job(
            schedule="every 2m",
            name="caduceus",
            script="caduceus-pulse.sh",
            no_agent=True,
        )

    assert job_id == "deadbeef"
    assert ["hermes", "cron", "create", "every 2m", "--name", "caduceus", "--script", "caduceus-pulse.sh", "--no-agent"] in calls


def test_cron_update_falls_back_to_remove_then_create() -> None:
    """CRONCLI-05: update is implemented as remove followed by create."""
    from caduceus import _runtime

    with subprocess_run_recorder(
        {"remove": "", "create": create_stdout("cafef00d")}
    ) as calls:
        _runtime.cron_update_job(
            job_id="oldjobid",
            schedule="every 2m",
            name="caduceus",
            script="caduceus-pulse.sh",
            no_agent=True,
        )

    ops = _job_ids(calls)
    assert ops == ["remove", "create"]
    remove_call = next(c for c in calls if c[2] == "remove")
    create_call = next(c for c in calls if c[2] == "create")
    assert remove_call[3] == "oldjobid"
    assert create_call[3] == "every 2m"
    assert "--name" in create_call

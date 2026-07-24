"""Cron capability dispatcher unit tests."""

from __future__ import annotations

import json

import pytest
from tests.fixtures.capability_simulator import get_simulator
from tests.plugin._helpers import subprocess_run_recorder


# ---------------------------------------------------------------------------
# Cron list through the subprocess recorder
# ---------------------------------------------------------------------------


def test_cron_well_formed_returns_job_list() -> None:
    """A well-formed table parses to a dict keyed by job id."""
    from tests.fixtures.capability_simulator import well_formed_table
    from caduceus import _runtime

    with subprocess_run_recorder({"list": well_formed_table()}) as calls:
        result = _runtime.cron_list_jobs()
    assert len(calls) == 1
    assert calls[0] == ["hermes", "cron", "list", "--all"]
    assert isinstance(result, dict)
    assert "abc" in result
    assert result["abc"]["name"] == "caduceus"
    assert result["abc"]["schedule"] == "every 2m"


def test_cron_malformed_raises_cron_capability_error() -> None:
    """Non-table stdout maps to an empty result, not an exception."""
    from caduceus import _runtime

    with subprocess_run_recorder({"list": "garbled"}) as calls:
        result = _runtime.cron_list_jobs()
    assert result == {}


def test_cron_denied_raises_cron_capability_error() -> None:
    """A denied stderr prefix raises CronCapabilityError with category denied."""
    import subprocess
    from caduceus import _runtime

    def fail_list(argv, kwargs):
        return subprocess.CompletedProcess(
            argv, 1, "", "denied: cron subsystem not available"
        )

    with subprocess_run_recorder({"list": fail_list}):
        with pytest.raises(_runtime.CronCapabilityError) as excinfo:
            _runtime.cron_list_jobs()
    assert excinfo.value.category == "denied"
    assert excinfo.value.detail is not None


def test_cron_timed_out_raises_cron_capability_error() -> None:
    """A timeout raises CronCapabilityError with timed-out category."""
    from caduceus import _runtime

    def raise_timed_out(argv, kwargs):
        raise _runtime.CronCapabilityError("timed-out", "hermes cron timed out")

    with subprocess_run_recorder({"list": raise_timed_out}):
        with pytest.raises(_runtime.CronCapabilityError) as excinfo:
            _runtime.cron_list_jobs()
    assert excinfo.value.category == "timed-out"
    assert excinfo.value.detail is not None


def test_cron_eof_raises_cron_capability_error() -> None:
    """An EOF condition raises CronCapabilityError with eof category."""
    from caduceus import _runtime

    def raise_eof(argv, kwargs):
        raise _runtime.CronCapabilityError("eof", "cron capability returned EOF")

    with subprocess_run_recorder({"list": raise_eof}):
        with pytest.raises(_runtime.CronCapabilityError) as excinfo:
            _runtime.cron_list_jobs()
    assert excinfo.value.category == "eof"
    assert excinfo.value.detail is not None


def test_cron_crashed_raises_cron_capability_error() -> None:
    """A Hermes crash raises CronCapabilityError with crashed category."""
    from caduceus import _runtime

    def raise_crashed(argv, kwargs):
        raise _runtime.CronCapabilityError("crashed", "cron crashed")

    with subprocess_run_recorder({"list": raise_crashed}):
        with pytest.raises(_runtime.CronCapabilityError) as excinfo:
            _runtime.cron_list_jobs()
    assert excinfo.value.category == "crashed"
    assert excinfo.value.detail is not None


def test_cron_absent_returns_empty_dict() -> None:
    """An empty table (banner only) returns {}."""
    from tests.fixtures.capability_simulator import empty_table
    from caduceus import _runtime

    with subprocess_run_recorder({"list": empty_table()}):
        result = _runtime.cron_list_jobs()
    assert result == {}


def test_cron_unknown_category_raises_value_error() -> None:
    """An unrecognised simulator category still raises ValueError."""
    with pytest.raises(ValueError, match="unknown cron capability category"):
        get_simulator("bogus")


# ---------------------------------------------------------------------------
# CronCapabilityError construction and attributes
# ---------------------------------------------------------------------------


def test_cron_capability_error_is_exception() -> None:
    """CronCapabilityError is a proper Exception subclass."""
    from caduceus import _runtime

    err = _runtime.CronCapabilityError(category="test", detail="test detail")
    assert isinstance(err, Exception)
    assert issubclass(_runtime.CronCapabilityError, Exception)


def test_cron_capability_error_has_category_and_detail() -> None:
    """CronCapabilityError stores category and detail fields."""
    from caduceus import _runtime

    err = _runtime.CronCapabilityError(category="denied", detail="permission denied")
    assert err.category == "denied"
    assert err.detail == "permission denied"


def test_cron_capability_error_str_includes_category() -> None:
    """str(error) includes the category for readable messages."""
    from caduceus import _runtime

    err = _runtime.CronCapabilityError(category="malformed-response", detail="None")
    msg = str(err)
    assert "malformed-response" in msg


# ---------------------------------------------------------------------------
# _coerce_jobs direct tests (triangulation)
# ---------------------------------------------------------------------------


def test_coerce_jobs_none_returns_empty() -> None:
    """_coerce_jobs(None) returns {}."""
    from caduceus._runtime import _coerce_jobs

    assert _coerce_jobs(None) == {}


def test_coerce_jobs_empty_jobs_list_returns_empty() -> None:
    """_coerce_jobs({"jobs": []}) returns {}."""
    from caduceus._runtime import _coerce_jobs

    assert _coerce_jobs({"jobs": []}) == {}


def test_coerce_jobs_empty_dict_returns_empty() -> None:
    """_coerce_jobs({}) returns {}."""
    from caduceus._runtime import _coerce_jobs

    assert _coerce_jobs({}) == {}


def test_coerce_jobs_empty_list_returns_empty() -> None:
    """_coerce_jobs([]) returns {}."""
    from caduceus._runtime import _coerce_jobs

    assert _coerce_jobs([]) == {}


def test_coerce_jobs_string_raises_malformed() -> None:
    """_coerce_jobs(str) raises CronCapabilityError with malformed-response."""
    from caduceus._runtime import CronCapabilityError, _coerce_jobs

    with pytest.raises(CronCapabilityError) as excinfo:
        _coerce_jobs("garbled")
    assert excinfo.value.category == "malformed-response"


def test_coerce_jobs_populated_jobs_list() -> None:
    """_coerce_jobs({"jobs": [{"id": "x", "name": "test"}]}) returns dict."""
    from caduceus._runtime import _coerce_jobs

    result = _coerce_jobs({"jobs": [{"id": "x", "name": "test"}]})
    assert result == {"x": {"id": "x", "name": "test"}}


def test_coerce_jobs_plain_list() -> None:
    """_coerce_jobs([{"id": "x", "name": "test"}]) returns dict."""
    from caduceus._runtime import _coerce_jobs

    result = _coerce_jobs([{"id": "x", "name": "test"}])
    assert result == {"x": {"id": "x", "name": "test"}}


def test_coerce_jobs_keyed_dict() -> None:
    """_coerce_jobs({"x": {"id": "x", "name": "test"}}) returns dict."""
    from caduceus._runtime import _coerce_jobs

    result = _coerce_jobs({"x": {"id": "x", "name": "test"}})
    assert result == {"x": {"id": "x", "name": "test"}}


def test_coerce_jobs_json_string_success_returns_jobs_dict() -> None:
    """A real Hermes JSON-string success shape is parsed and keyed by id."""
    from caduceus._runtime import _coerce_jobs

    payload = json.dumps({"success": True, "count": 1, "jobs": [{"id": "caduceus", "name": "caduceus", "schedule": "every 2m"}]})
    result = _coerce_jobs(payload)
    assert result == {"caduceus": {"id": "caduceus", "name": "caduceus", "schedule": "every 2m"}}


def test_coerce_jobs_json_string_success_empty_returns_empty_dict() -> None:
    """A real Hermes empty JSON-string success shape returns {}."""
    from caduceus._runtime import _coerce_jobs

    assert _coerce_jobs(json.dumps({"success": True, "count": 0, "jobs": []})) == {}


def test_coerce_jobs_json_string_error_envelope_raises_denied() -> None:
    """A registry error envelope JSON string raises ``denied``."""
    from caduceus._runtime import CronCapabilityError, _coerce_jobs

    with pytest.raises(CronCapabilityError) as excinfo:
        _coerce_jobs(json.dumps({"error": "permission denied"}))
    assert excinfo.value.category == "denied" and excinfo.value.detail == "permission denied"


def test_coerce_jobs_unparseable_string_raises_malformed_with_internal_detail() -> None:
    """An unparseable string preserves the raw value in ``internal_detail``."""
    from caduceus._runtime import CronCapabilityError, _coerce_jobs

    with pytest.raises(CronCapabilityError) as excinfo:
        _coerce_jobs("garbled")
    assert excinfo.value.category == "malformed-response"
    assert excinfo.value.internal_detail == "garbled"
    assert "garbled" not in excinfo.value.detail


def test_coerce_jobs_existing_shapes_unchanged() -> None:
    """The pre-existing None/dict/list shape branches are unchanged."""
    from caduceus._runtime import _coerce_jobs

    assert _coerce_jobs(None) == {}
    assert _coerce_jobs({"jobs": [{"id": "x", "name": "test"}]}) == {"x": {"id": "x", "name": "test"}}
    assert _coerce_jobs([{"id": "x", "name": "test"}]) == {"x": {"id": "x", "name": "test"}}
    assert _coerce_jobs({"x": {"id": "x", "name": "test"}}) == {"x": {"id": "x", "name": "test"}}


# ---------------------------------------------------------------------------
# Distinct cron-capability findings through the doctor probe (REQ-55)
# ---------------------------------------------------------------------------


def test_doctor_check_cron_capability_no_caduceus_job_registered(
    adapter, install_with_fake_binary: Path
) -> None:
    """A well-formed empty job list becomes a distinct OK prerequisite finding."""
    from tests.fixtures.capability_simulator import empty_table
    from caduceus import _runtime

    with subprocess_run_recorder({"list": empty_table()}):
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    assert finding.status == "ok"
    assert "no Caduceus cron job registered yet" in finding.detail
    assert "external prerequisite, not exercised" in finding.detail
    assert "hermes caduceus cron-install" in finding.next_action


def test_doctor_check_cron_capability_raises_distinct_finding(
    adapter, install_with_fake_binary: Path
) -> None:
    """A denied capability becomes a distinct FAIL finding mentioning the category."""
    import subprocess
    from caduceus import _runtime

    def fail_list(argv, kwargs):
        return subprocess.CompletedProcess(
            argv, 1, "", "denied: cron subsystem not available"
        )

    with subprocess_run_recorder({"list": fail_list}):
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    assert finding.status == "fail"
    assert "cron list call raised an exception" in finding.detail
    assert "denied" in finding.detail
    assert "hermes caduceus cron-install" in finding.next_action


def test_doctor_check_cron_capability_unexpected_payload_shape_distinct_finding(
    adapter, install_with_fake_binary: Path
) -> None:
    """A malformed payload becomes a distinct FAIL finding without leaking category text."""
    from caduceus import _runtime

    def raise_malformed(argv, kwargs):
        raise _runtime.CronCapabilityError(
            "malformed-response",
            "Hermes returned an unexpected payload shape",
            "garbled",
        )

    with subprocess_run_recorder({"list": raise_malformed}):
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    assert finding.status == "fail"
    assert "unexpected payload shape" in finding.detail
    assert "hermes plugins install --enable" in finding.next_action
    assert "malformed-response" not in finding.detail


def test_doctor_check_cron_capability_findings_are_distinct(
    adapter, install_with_fake_binary: Path
) -> None:
    """No-job, exception, and shape findings are pairwise operator-distinguishable."""
    import subprocess
    from tests.fixtures.capability_simulator import empty_table
    from caduceus import _runtime

    def fail_denied(argv, kwargs):
        return subprocess.CompletedProcess(
            argv, 1, "", "denied: cron subsystem not available"
        )

    def raise_malformed(argv, kwargs):
        raise _runtime.CronCapabilityError(
            "malformed-response",
            "Hermes returned an unexpected payload shape",
            "garbled",
        )

    scenarios = {
        "empty": empty_table(),
        "denied": fail_denied,
        "malformed": raise_malformed,
    }

    findings = []
    for scenario in scenarios.values():
        with subprocess_run_recorder({"list": scenario}):
            findings.append(adapter._doctor_check_cron_capability(ctx=adapter))

    details = [f.detail for f in findings]
    next_actions = [f.next_action for f in findings]
    assert len(set(details)) == len(details)
    assert len(set(next_actions)) == len(next_actions)

"""Cron capability dispatcher unit tests."""

from __future__ import annotations

import json

import pytest
from tests.fixtures.fake_ctx import FakePluginContext


# ---------------------------------------------------------------------------
# CRON-01: well_formed
# ---------------------------------------------------------------------------


def test_cron_well_formed_returns_job_list() -> None:
    """``well_formed`` dispatcher returns a well-formed job list."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("well_formed")
    try:
        result = _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()
    assert isinstance(result, dict)
    assert "abc" in result
    assert result["abc"]["name"] == "caduceus"
    assert result["abc"]["schedule"] == "every 2m"


# ---------------------------------------------------------------------------
# CRON-02: malformed
# ---------------------------------------------------------------------------


def test_cron_malformed_raises_cron_capability_error() -> None:
    """``malformed`` dispatcher returns None -> CronCapabilityError raised."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("malformed")
    try:
        with pytest.raises(_runtime.CronCapabilityError) as excinfo:
            _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()
    assert excinfo.value.category == "malformed-response"
    assert excinfo.value.detail is not None


# ---------------------------------------------------------------------------
# CRON-03: denied
# ---------------------------------------------------------------------------


def test_cron_denied_raises_cron_capability_error() -> None:
    """``denied`` raises CronCapabilityError with denied category."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("denied")
    try:
        with pytest.raises(_runtime.CronCapabilityError) as excinfo:
            _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()
    assert excinfo.value.category == "denied"
    assert excinfo.value.detail is not None


# ---------------------------------------------------------------------------
# CRON-04: timed_out
# ---------------------------------------------------------------------------


def test_cron_timed_out_raises_cron_capability_error() -> None:
    """``timed_out`` raises CronCapabilityError with timed-out category."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("timed_out")
    try:
        with pytest.raises(_runtime.CronCapabilityError) as excinfo:
            _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()
    assert excinfo.value.category == "timed-out"
    assert excinfo.value.detail is not None


# ---------------------------------------------------------------------------
# CRON-05: eof
# ---------------------------------------------------------------------------


def test_cron_eof_raises_cron_capability_error() -> None:
    """``eof`` raises CronCapabilityError with eof category."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("eof")
    try:
        with pytest.raises(_runtime.CronCapabilityError) as excinfo:
            _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()
    assert excinfo.value.category == "eof"
    assert excinfo.value.detail is not None


# ---------------------------------------------------------------------------
# CRON-06: crashed
# ---------------------------------------------------------------------------


def test_cron_crashed_raises_cron_capability_error() -> None:
    """``crashed`` raises CronCapabilityError with crashed category."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("crashed")
    try:
        with pytest.raises(_runtime.CronCapabilityError) as excinfo:
            _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()
    assert excinfo.value.category == "crashed"
    assert excinfo.value.detail is not None


# ---------------------------------------------------------------------------
# CRON-07: absent
# ---------------------------------------------------------------------------


def test_cron_absent_returns_empty_dict() -> None:
    """``absent`` returns None -> _coerce_jobs returns {}."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("absent")
    try:
        result = _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()
    # The absent simulator returns None, which _coerce_jobs coerces to {}
    # (empty dict — no jobs, no error).
    assert result == {}


# ---------------------------------------------------------------------------
# CRON-08: unknown category
# ---------------------------------------------------------------------------


def test_cron_unknown_category_raises_value_error() -> None:
    """An unrecognised category raises ValueError."""
    ctx = FakePluginContext(name="caduceus")
    with pytest.raises(ValueError, match="unknown cron capability category"):
        ctx.install_cron_capability("bogus")


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

    payload = json.dumps(
        {
            "success": True,
            "count": 1,
            "jobs": [{"id": "caduceus", "name": "caduceus", "schedule": "every 2m"}],
        }
    )
    result = _coerce_jobs(payload)
    assert result == {"caduceus": {"id": "caduceus", "name": "caduceus", "schedule": "every 2m"}}


def test_coerce_jobs_json_string_success_empty_returns_empty_dict() -> None:
    """A real Hermes empty JSON-string success shape returns {}."""
    from caduceus._runtime import _coerce_jobs

    payload = json.dumps({"success": True, "count": 0, "jobs": []})
    assert _coerce_jobs(payload) == {}


def test_coerce_jobs_json_string_error_envelope_raises_denied() -> None:
    """A registry error envelope JSON string raises ``denied``."""
    from caduceus._runtime import CronCapabilityError, _coerce_jobs

    with pytest.raises(CronCapabilityError) as excinfo:
        _coerce_jobs(json.dumps({"error": "permission denied"}))
    assert excinfo.value.category == "denied"
    assert excinfo.value.detail == "permission denied"


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
    assert _coerce_jobs({"jobs": [{"id": "x", "name": "test"}]}) == {
        "x": {"id": "x", "name": "test"}
    }
    assert _coerce_jobs([{"id": "x", "name": "test"}]) == {"x": {"id": "x", "name": "test"}}
    assert _coerce_jobs({"x": {"id": "x", "name": "test"}}) == {
        "x": {"id": "x", "name": "test"}
    }


# ---------------------------------------------------------------------------
# Distinct cron-capability findings through the doctor probe (REQ-55)
# ---------------------------------------------------------------------------


def test_doctor_check_cron_capability_no_caduceus_job_registered(
    adapter, install_with_fake_binary: Path
) -> None:
    """A well-formed empty job list becomes a distinct OK prerequisite finding."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("absent")
    try:
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    finally:
        _runtime.reset_dispatcher()
    assert finding.status == "ok"
    assert "no Caduceus cron job registered yet" in finding.detail
    assert "external prerequisite, not exercised" in finding.detail
    assert "hermes caduceus cron-install" in finding.next_action


def test_doctor_check_cron_capability_raises_distinct_finding(
    adapter, install_with_fake_binary: Path
) -> None:
    """A denied capability becomes a distinct FAIL finding mentioning the category."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("denied")
    try:
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    finally:
        _runtime.reset_dispatcher()
    assert finding.status == "fail"
    assert "cron list call raised an exception" in finding.detail
    assert "denied" in finding.detail
    assert "hermes caduceus cron-install" in finding.next_action


def test_doctor_check_cron_capability_unexpected_payload_shape_distinct_finding(
    adapter, install_with_fake_binary: Path
) -> None:
    """A malformed payload becomes a distinct FAIL finding without leaking category text."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("malformed")
    try:
        finding = adapter._doctor_check_cron_capability(ctx=adapter)
    finally:
        _runtime.reset_dispatcher()
    assert finding.status == "fail"
    assert "unexpected payload shape" in finding.detail
    assert "hermes plugins install --enable" in finding.next_action
    assert "malformed-response" not in finding.detail


def test_doctor_check_cron_capability_findings_are_distinct(
    adapter, install_with_fake_binary: Path
) -> None:
    """No-job, exception, and shape findings are pairwise operator-distinguishable."""
    from caduceus import _runtime

    findings = []
    categories = ["absent", "denied", "malformed"]
    for category in categories:
        ctx = FakePluginContext(name="caduceus")
        ctx.install_cron_capability(category)
        try:
            finding = adapter._doctor_check_cron_capability(ctx=adapter)
        finally:
            _runtime.reset_dispatcher()
        findings.append(finding)

    details = [f.detail for f in findings]
    next_actions = [f.next_action for f in findings]
    assert len(set(details)) == len(details)
    assert len(set(next_actions)) == len(next_actions)
"""Capability tests for FakePluginContext.install_cron_capability.

Each category dispatches through ``install_cron_capability`` which wires
a callable into ``_runtime.install_dispatcher``. The adapter's existing
cron helpers (``cron_list_jobs``, ``cron_create_job``, etc.) are the
canonical consumers — this test exercises every category listed in the
design contract.

See also: ``tests/hermes_plugin_test.py`` for the integration-level
cron-reconciliation tests that exercise the same dispatcher.
"""

from __future__ import annotations

import pytest
from tests.fake_ctx import FakePluginContext


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


def test_cron_malformed_returns_empty() -> None:
    """``malformed`` dispatcher returns None → cron_list_jobs returns {}."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("malformed")
    try:
        result = _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()
    assert result == {}


# ---------------------------------------------------------------------------
# CRON-03: denied
# ---------------------------------------------------------------------------


def test_cron_denied_raises_runtime_error() -> None:
    """``denied`` raises RuntimeError("cron denied")."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("denied")
    try:
        with pytest.raises(RuntimeError, match="cron denied"):
            _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()


# ---------------------------------------------------------------------------
# CRON-04: timed_out
# ---------------------------------------------------------------------------


def test_cron_timed_out_raises_timeout_error() -> None:
    """``timed_out`` raises TimeoutError("cron timed out")."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("timed_out")
    try:
        with pytest.raises(TimeoutError, match="cron timed out"):
            _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()


# ---------------------------------------------------------------------------
# CRON-05: eof
# ---------------------------------------------------------------------------


def test_cron_eof_returns_empty_jobs() -> None:
    """``eof`` returns {"jobs": []} → cron_list_jobs returns {}."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("eof")
    try:
        result = _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()
    assert result == {}


# ---------------------------------------------------------------------------
# CRON-06: crashed
# ---------------------------------------------------------------------------


def test_cron_crashed_raises_runtime_error() -> None:
    """``crashed`` raises RuntimeError("cron crashed")."""
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("crashed")
    try:
        with pytest.raises(RuntimeError, match="cron crashed"):
            _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()


# ---------------------------------------------------------------------------
# CRON-07: absent
# ---------------------------------------------------------------------------


def test_cron_absent_restores_default_capture() -> None:
    """``absent`` restores capture-only dispatch: appends to dispatch_calls.

    The default ``dispatch_tool`` appends to ``dispatch_calls`` and
    returns None. After installing ``absent``, the dispatcher should
    behave exactly like the default.
    """
    from caduceus import _runtime

    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("absent")
    try:
        # The absent dispatcher restores the default capture-only
        # behaviour that appends to dispatch_calls and returns None.
        _runtime.cron_list_jobs()
    finally:
        _runtime.reset_dispatcher()
    assert len(ctx.dispatch_calls) >= 1
    last_call = ctx.dispatch_calls[-1]
    assert last_call["name"] == "cronjob"
    assert last_call["args"]["action"] == "list"


# ---------------------------------------------------------------------------
# CRON-08: unknown category
# ---------------------------------------------------------------------------


def test_cron_unknown_category_raises_value_error() -> None:
    """An unrecognised category raises ValueError."""
    ctx = FakePluginContext(name="caduceus")
    with pytest.raises(ValueError, match="unknown cron capability category"):
        ctx.install_cron_capability("bogus")
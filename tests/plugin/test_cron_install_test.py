"""End-to-end cron-install / cron-remove tests with the JSON-string dispatcher."""

from __future__ import annotations

from tests.fixtures.fake_ctx import FakePluginContext


def test_cron_install_with_real_hermes_dispatcher_succeeds(adapter, install_with_fake_binary):
    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("real_hermes")
    try:
        action, note = adapter._cron_install(dry_run=True)
    finally:
        from caduceus import _runtime as rt
        rt.reset_dispatcher()
    assert action in ("created", "reused")
    assert note == "dry-run"


def test_cron_install_with_error_envelope_dispatcher_fails_operator_readable(
    adapter, install_with_fake_binary, capsys
):
    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("error_envelope")
    try:
        rc = adapter._cli_cron_install(dry_run=False)
    finally:
        from caduceus import _runtime as rt
        rt.reset_dispatcher()
    stderr = capsys.readouterr().err
    assert rc == 1
    assert "[FAIL]" in stderr
    assert "Hermes refused the cron operation" in stderr
    assert "malformed-response:" not in stderr
    assert "CronCapabilityError" not in stderr
    assert "RuntimeError" not in stderr


def test_cron_install_with_malformed_string_dispatcher_fails_operator_readable(
    adapter, install_with_fake_binary, capsys
):
    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("malformed")
    try:
        rc = adapter._cli_cron_install(dry_run=False)
    finally:
        from caduceus import _runtime as rt
        rt.reset_dispatcher()
    stderr = capsys.readouterr().err
    assert rc == 1
    assert "[FAIL]" in stderr
    assert "Hermes returned an unexpected payload shape" in stderr
    assert "malformed-response:" not in stderr
    assert "CronCapabilityError" not in stderr
    assert "RuntimeError" not in stderr
    assert "garbled" not in stderr


def test_cron_remove_with_real_hermes_dispatcher_succeeds(adapter, install_with_fake_binary, capsys):
    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("real_hermes")
    try:
        rc = adapter._cli_cron_remove()
    finally:
        from caduceus import _runtime as rt
        rt.reset_dispatcher()
    assert rc == 0
    assert "[OK] cron-remove —" in capsys.readouterr().out


def test_cron_remove_with_error_envelope_dispatcher_fails_operator_readable(
    adapter, install_with_fake_binary, capsys
):
    ctx = FakePluginContext(name="caduceus")
    ctx.install_cron_capability("error_envelope")
    try:
        rc = adapter._cli_cron_remove()
    finally:
        from caduceus import _runtime as rt
        rt.reset_dispatcher()
    stderr = capsys.readouterr().err
    assert rc == 1
    assert "[FAIL] cron-remove —" in stderr
    assert "malformed-response:" not in stderr
    assert "CronCapabilityError" not in stderr
    assert "RuntimeError" not in stderr

"""End-to-end cron-install / cron-remove tests with the subprocess recorder."""

from __future__ import annotations

import subprocess

from tests.plugin._helpers import subprocess_run_recorder


def test_cron_install_with_real_hermes_table_succeeds(adapter, install_with_fake_binary):
    from tests.fixtures.capability_simulator import well_formed_table

    list_stdout = well_formed_table()
    with subprocess_run_recorder({"list": list_stdout}) as calls:
        action, note = adapter._cron_install(dry_run=True)
    assert action == "reused"
    assert note == "dry-run"
    assert any(c[2] == "list" for c in calls)


def test_cron_install_empty_list_creates(adapter, install_with_fake_binary):
    from tests.fixtures.capability_simulator import empty_table, create_stdout

    with subprocess_run_recorder(
        {"list": empty_table(), "create": create_stdout("deadbeef")}
    ) as calls:
        action, note = adapter._cron_install(dry_run=False)
    assert action == "created"
    assert note == "deadbeef"
    assert any(c[2] == "list" for c in calls)
    assert any(c[2] == "create" for c in calls)


def test_cron_install_with_error_envelope_fails_operator_readable(
    adapter, install_with_fake_binary, capsys
):
    def fail_list(argv, kwargs):
        return subprocess.CompletedProcess(
            argv, 1, "", "denied: cron subsystem not available"
        )

    with subprocess_run_recorder({"list": fail_list}) as calls:
        rc = adapter._cli_cron_install(dry_run=False)
    stderr = capsys.readouterr().err
    assert rc == 1
    assert "[FAIL]" in stderr
    assert "Hermes refused the cron operation" in stderr
    assert "malformed-response:" not in stderr
    assert "CronCapabilityError" not in stderr
    assert "RuntimeError" not in stderr


def test_cron_install_with_malformed_string_fails_operator_readable(
    adapter, install_with_fake_binary, capsys
):
    """A response that cannot be parsed as a table is treated as malformed."""
    from caduceus._runtime import CronCapabilityError

    def raise_malformed(argv, kwargs):
        raise CronCapabilityError(
            "malformed-response",
            "Hermes returned an unexpected payload shape",
            "garbled",
        )

    with subprocess_run_recorder({"list": raise_malformed}) as calls:
        rc = adapter._cli_cron_install(dry_run=False)
    stderr = capsys.readouterr().err
    assert rc == 1
    assert "[FAIL]" in stderr
    assert "Hermes returned an unexpected payload shape" in stderr
    assert "malformed-response:" not in stderr
    assert "CronCapabilityError" not in stderr
    assert "RuntimeError" not in stderr
    assert "garbled" not in stderr


def test_cron_remove_with_real_hermes_table_succeeds(adapter, install_with_fake_binary, capsys):
    from tests.fixtures.capability_simulator import well_formed_table, empty_table

    with subprocess_run_recorder({"list": well_formed_table(), "remove": empty_table()}) as calls:
        rc = adapter._cli_cron_remove()
    assert rc == 0
    assert "[OK] cron-remove" in capsys.readouterr().out
    assert [c[2] for c in calls] == ["list", "remove"]


def test_cron_remove_with_error_envelope_fails_operator_readable(
    adapter, install_with_fake_binary, capsys
):
    def fail_list(argv, kwargs):
        return subprocess.CompletedProcess(
            argv, 1, "", "denied: cron subsystem not available"
        )

    with subprocess_run_recorder({"list": fail_list}) as calls:
        rc = adapter._cli_cron_remove()
    stderr = capsys.readouterr().err
    assert rc == 1
    assert "[FAIL] cron-remove" in stderr
    assert "malformed-response:" not in stderr
    assert "CronCapabilityError" not in stderr
    assert "RuntimeError" not in stderr

"""Doctor finding namedtuple tests."""

from __future__ import annotations

import json
import os
import re
import shutil
import stat
import subprocess
import sys
from pathlib import Path
from typing import Any, Dict, List

import pytest

from tests.fixtures.fake_ctx import (
    FakePluginContext,
    assert_cli_command_registered,
    assert_command_registered,
    assert_skill_registered,
)


def test_doctor_finding_is_namedtuple() -> None:
    """_DoctorFinding is a namedtuple with category, status, detail, next_action, internal_detail."""
    from collections import namedtuple
    from caduceus import _DoctorFinding

    assert isinstance(_DoctorFinding, type)
    assert issubclass(_DoctorFinding, tuple)
    # Namedtuples have _fields.
    assert _DoctorFinding._fields == (
        "category",
        "status",
        "detail",
        "next_action",
        "internal_detail",
    )




def test_doctor_finding_holds_values() -> None:
    """_DoctorFinding stores category, status, detail, next_action."""
    from caduceus import _DoctorFinding

    f = _DoctorFinding(
        category="host-capability-unavailable",
        status="fail",
        detail="/path/to/binary not found",
        next_action="run `hermes caduceus setup` to build the binary",
    )
    assert f.category == "host-capability-unavailable"
    assert f.status == "fail"
    assert f.detail == "/path/to/binary not found"
    assert f.next_action == "run `hermes caduceus setup` to build the binary"




def test_doctor_finding_accepts_ok_status() -> None:
    """_DoctorFinding accepts status='ok'."""
    from caduceus import _DoctorFinding

    f = _DoctorFinding(
        category="config-incomplete",
        status="ok",
        detail="all config present",
        next_action="",
    )
    assert f.status == "ok"
    assert f.next_action == ""




def test_doctor_finding_is_immutable() -> None:
    """_DoctorFinding is a namedtuple — fields cannot be reassigned."""
    from caduceus import _DoctorFinding

    f = _DoctorFinding(
        category="gateway-inactive",
        status="fail",
        detail="gateway not reachable",
        next_action="run `hermes gateway restart`",
    )
    with pytest.raises(AttributeError):
        f.status = "ok"  # type: ignore[misc]

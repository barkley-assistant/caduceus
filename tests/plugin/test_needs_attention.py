"""NeedsAttention return type tests."""

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


def test_needs_attention_has_recovery_evidence() -> None:
    """_NeedsAttention carries a recovery_evidence string."""
    from caduceus import _NeedsAttention

    na = _NeedsAttention(recovery_evidence="wrapper and job state diverged")
    assert na.recovery_evidence == "wrapper and job state diverged"




def test_needs_attention_is_not_a_success() -> None:
    """_NeedsAttention is not a tuple and does not falsy-pass as success."""
    from caduceus import _NeedsAttention

    na = _NeedsAttention(recovery_evidence="unrecoverable")
    # It must not be a tuple (the normal (action, note) return shape).
    assert not isinstance(na, tuple)

"""Display policy for the wrapper doctor (``hermes caduceus doctor``).

Two rendering modes, chosen by one gate:

* **plain** — the legacy byte-for-byte text (``[OK]``/``[FAIL]``/``[WARN]``
  lines, 7-space sub-line indent, no wrapping). This is what piped, CI, and
  ``NO_COLOR`` runs see, so log grepping and the release-canary doctor
  classifier keep working unchanged.
* **interactive** — status glyphs, ANSI color on the status word, an
  aligned check-name column, and width-aware wrapping with a hanging
  indent.

Stdlib-only and free of plugin imports so it never cycles: the caller
imports it function-locally (``from . import _display``).
"""

from __future__ import annotations

import os
import shutil
import textwrap
from dataclasses import dataclass
from typing import List

RESET = "\x1b[0m"
_GREEN = "\x1b[32m"
_RED = "\x1b[31m"
_YELLOW = "\x1b[33m"

#: status -> (tty_glyph, tty_word, ascii_word, ansi)
#:
#: ``warn`` is emitted by the chained OCI-readiness and tick-freshness
#: checks (issue #414): advisory findings that the operator should see
#: but that never change the exit code. The table stays the single
#: source of truth for status presentation; an unknown status renders as
#: the ``fail`` row.
_STATUS_VOCAB = {
    "ok": ("\u2713", "OK", "[OK]", _GREEN),
    "fail": ("\u2717", "FAIL", "[FAIL]", _RED),
    "warn": ("!", "WARN", "[WARN]", _YELLOW),
}

_LABEL_NEXT_ACTION = "next action: "
_LABEL_DETAIL = "detail:      "
_LABEL_CATEGORY = "category:    "

#: Legacy sub-line indent. Combined with the 13-character labels above it
#: places sub-line content at a shared column; both are part of the plain
#: output's byte contract.
_PLAIN_INDENT = " " * 7

_STATUS_WIDTH = 4  # "OK  " / "FAIL" / "WARN"

#: Below this much room for text after the hanging indent, wrapping cannot
#: improve anything (and would fight ``textwrap``'s indent handling), so the
#: logical line is emitted unwrapped.
_MIN_WRAP_TEXT_WIDTH = 12

_FALLBACK_WIDTH = 80


@dataclass(frozen=True)
class Style:
    """Resolved rendering mode for one output stream."""

    interactive: bool


def _is_tty(stream) -> bool:
    isatty = getattr(stream, "isatty", None)
    if not callable(isatty):
        return False
    try:
        return bool(isatty())
    except (ValueError, OSError):
        return False


def detect_style(stream) -> Style:
    """Resolve the rendering mode for *stream*.

    Interactive only when every one of these holds:

    * ``stream.isatty()`` -- a pipe or test capture stays plain;
    * ``TERM`` is not ``dumb`` -- an unset ``TERM`` on a real TTY is
      treated as capable;
    * ``NO_COLOR`` is absent from the environment -- ``NO_COLOR=""``
      counts as set, so "unset" is literal;
    * the stream encoding can carry the status glyphs.
    """
    if not _is_tty(stream):
        return Style(interactive=False)
    if os.environ.get("TERM", "") == "dumb":
        return Style(interactive=False)
    if "NO_COLOR" in os.environ:
        return Style(interactive=False)
    if not _can_encode_glyphs(getattr(stream, "encoding", "") or ""):
        return Style(interactive=False)
    return Style(interactive=True)


def _can_encode_glyphs(encoding: str) -> bool:
    """Whether *encoding* can carry the status glyphs.

    The ``ascii`` family -- which on glibc also reports itself as
    ``ANSI_X3.4-1968`` -- is rejected by name, and any other codec is
    probed by encoding the glyph. A missing/empty encoding (test fakes,
    some wrappers) is treated as capable.
    """
    if "ascii" in encoding.lower():
        return False
    if not encoding:
        return True
    try:
        for glyph in {entry[0] for entry in _STATUS_VOCAB.values()}:
            glyph.encode(encoding)
    except (UnicodeEncodeError, LookupError):
        return False
    return True


def terminal_width() -> int:
    """Return the terminal width in columns, falling back to 80."""
    try:
        columns = shutil.get_terminal_size().columns
    except (ValueError, OSError):
        return _FALLBACK_WIDTH
    if columns <= 0:
        return _FALLBACK_WIDTH
    return columns


def wrap_hanging(text: str, width: int, hang: int) -> str:
    """Wrap *text* to *width* columns with continuation lines at *hang*.

    Never truncates: ``break_long_words=False`` lets an unbreakable token
    (a long path) overflow rather than lose bytes, and
    ``break_on_hyphens=False`` keeps hyphenated names like
    ``worker-bridge.py`` intact.
    """
    if width < hang + _MIN_WRAP_TEXT_WIDTH:
        return text
    return textwrap.fill(
        text,
        width=width,
        subsequent_indent=" " * hang,
        break_long_words=False,
        break_on_hyphens=False,
    )


class DoctorRenderer:
    """Renders doctor findings for one stream.

    ``finding()`` returns the lines for a single check *without* the
    trailing blank line the caller prints; severities and exit codes stay
    in ``_cli_doctor``.
    """

    def __init__(self, style: Style, width: int, name_width: int) -> None:
        self.style = style
        self.width = width
        self.name_width = name_width
        # One shared continuation column: the status cell, the aligned
        # name, and the " — " separator.
        self.hang = 1 + 1 + _STATUS_WIDTH + 1 + name_width + 3

    @classmethod
    def from_stream(cls, stream, name_width: int) -> "DoctorRenderer":
        """Build a renderer whose policy is resolved from *stream*."""
        return cls(detect_style(stream), terminal_width(), name_width)

    def finding(self, name: str, finding, *, verbose: bool) -> List[str]:
        """Render one ``(name, _DoctorFinding)`` check into lines.

        Unknown or missing statuses fall back to the ``fail`` row so a
        malformed finding can never render as healthy.
        """
        raw_status = getattr(finding, "status", "")
        status = raw_status if raw_status in _STATUS_VOCAB else "fail"
        if not self.style.interactive:
            return self._finding_plain(name, finding, status=status, verbose=verbose)
        return self._finding_interactive(name, finding, status=status, verbose=verbose)

    # -- plain mode: the legacy byte contract ----------------------------

    def _finding_plain(self, name: str, finding, *, status: str, verbose: bool) -> List[str]:
        ascii_word = _STATUS_VOCAB[status][2]
        lines = [f"{ascii_word} {name} \u2014 {finding.detail}"]
        if finding.next_action:
            lines.append(f"{_PLAIN_INDENT}{_LABEL_NEXT_ACTION}{finding.next_action}")
        if verbose:
            internal = finding.internal_detail or finding.detail
            lines.append(f"{_PLAIN_INDENT}{_LABEL_DETAIL}{internal}")
            if status != "ok":
                lines.append(f"{_PLAIN_INDENT}{_LABEL_CATEGORY}{finding.category}")
        return lines

    # -- interactive mode ------------------------------------------------

    def _finding_interactive(
        self, name: str, finding, *, status: str, verbose: bool
    ) -> List[str]:
        glyph, word, _, ansi = _STATUS_VOCAB[status]
        padded_word = f"{word:<{_STATUS_WIDTH}}"
        aligned_name = f"{name:<{self.name_width}}"
        plain_head = f"{glyph} {padded_word} {aligned_name} \u2014 "
        colored_head = f"{ansi}{glyph} {padded_word}{RESET} {aligned_name} \u2014 "

        body = wrap_hanging(
            plain_head + finding.detail, self.width, self.hang
        ).split("\n")
        body[0] = colored_head + body[0][len(plain_head):]

        if finding.next_action:
            body.extend(self._sublines(_LABEL_NEXT_ACTION, finding.next_action))
        if verbose:
            internal = finding.internal_detail or finding.detail
            body.extend(self._sublines(_LABEL_DETAIL, internal))
            if status != "ok":
                body.extend(self._sublines(_LABEL_CATEGORY, finding.category))
        return body

    def _sublines(self, label: str, text: str) -> List[str]:
        indent = " " * self.hang
        return wrap_hanging(f"{indent}{label}{text}", self.width, self.hang).split("\n")

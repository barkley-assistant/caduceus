#!/bin/sh
# sentinel-read probe (WR-4): reads a sentinel file from /workspace and
# reports its contents in a single-line pass report.
#
#   sentinel-read [path]   default /workspace/sentinel.txt
#
# Strict POSIX sh: only utilities bundled with busybox.

sentinel="${1:-/workspace/sentinel.txt}"

if [ ! -f "$sentinel" ]; then
  echo "sentinel-read: FAIL sentinel file not found: $sentinel" >&2
  exit 1
fi
if [ ! -r "$sentinel" ]; then
  echo "sentinel-read: FAIL sentinel file not readable: $sentinel" >&2
  exit 1
fi

# Collapse newlines so the report stays on a single line.
contents=$(cat "$sentinel" 2>/dev/null | tr '\n' ' ')

echo "PASS sentinel-read: $contents"
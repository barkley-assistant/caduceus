#!/bin/sh
# Caduceus OCI worker result writer (WR-3).
#
# Writes a worker-result.json document honoring CADUCEUS_RESULT_PATH
# (default /output/worker-result.json) via a temp file + atomic rename,
# so no partial file survives. The document matches the worker-result
# schema (src/worker/worker_contract.rs): status is "success" or
# "failure", with non-empty summary, commit_message, and
# pull_request_title strings.
#
#   write-result.sh                    status: success
#   write-result.sh --status failure   status: failure
#
# If the target path is not writable, the writer exits non-zero with a
# diagnostic and leaves no partial file.
#
# Strict POSIX sh: only utilities bundled with busybox.

status="success"
case "${1:-}" in
  "")
    ;;
  --status)
    case "${2:-}" in
      success | failure) status="$2" ;;
      *)
        echo "usage: write-result.sh [--status success|failure]" >&2
        exit 2
        ;;
    esac
    ;;
  *)
    echo "usage: write-result.sh [--status success|failure]" >&2
    exit 2
    ;;
esac

result_path="${CADUCEUS_RESULT_PATH:-/output/worker-result.json}"
case "$result_path" in
  /*) ;;
  *)
    echo "write-result.sh: CADUCEUS_RESULT_PATH must be absolute: $result_path" >&2
    exit 1
    ;;
esac

dir=$(dirname "$result_path")
tmp="$result_path.tmp.$$"

cleanup() {
  rm -f "$tmp"
}
trap cleanup EXIT HUP INT TERM

if ! mkdir -p "$dir" 2>/dev/null; then
  echo "write-result.sh: cannot create result directory: $dir" >&2
  exit 1
fi

{
  printf '{\n'
  printf '  "status": "%s",\n' "$status"
  printf '  "summary": "%s",\n' "Reference worker completed with status $status"
  printf '  "commit_message": "%s",\n' "caduceus-worker-reference: completed with status $status"
  printf '  "pull_request_title": "%s"\n' "Caduceus reference worker: $status"
  printf '}\n'
} > "$tmp" 2>/dev/null || {
  echo "write-result.sh: cannot write temporary result: $tmp" >&2
  exit 1
}

if ! mv -f "$tmp" "$result_path" 2>/dev/null; then
  echo "write-result.sh: cannot move result into place: $result_path" >&2
  exit 1
fi

trap - EXIT HUP INT TERM
echo "write-result.sh: wrote $result_path"
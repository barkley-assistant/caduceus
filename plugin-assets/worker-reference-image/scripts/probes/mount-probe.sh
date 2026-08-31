#!/bin/sh
# mount-probe probe (WR-4): verifies /workspace and /output are
# writable and /tmp is writable and bounded — a tmpfs, per the worker
# filesystem contract in src/worker/worker_contract.rs.
#
# Strict POSIX sh: only utilities bundled with busybox.

fail() {
  echo "mount-probe: FAIL $*" >&2
  exit 1
}

for dir in /workspace /output /tmp; do
  [ -d "$dir" ] || fail "directory missing: $dir"
  [ -w "$dir" ] || fail "directory not writable: $dir"
  probe_file="$dir/.mount-probe.$$"
  if ! : > "$probe_file" 2>/dev/null; then
    fail "cannot create a file in $dir"
  fi
  rm -f "$probe_file" 2>/dev/null || fail "cannot remove probe file in $dir"
done

# /tmp must be a bounded tmpfs, not a host-backed bind mount.
tmp_fstype=$(awk '$2 == "/tmp" { print $3; exit }' /proc/mounts 2>/dev/null)
[ "$tmp_fstype" = "tmpfs" ] || fail "/tmp is not a bounded tmpfs (fstype: ${tmp_fstype:-<none>})"

echo "PASS mount-probe: /workspace and /output writable; /tmp writable bounded tmpfs"
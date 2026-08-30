#!/bin/sh
# resource-hog probe (WR-4): allocates CPU and memory up to a bounded
# amount and reports the result.
#
#   resource-hog [cpu_seconds] [memory_kib]
#     cpu_seconds  default 1, hard bound 10
#     memory_kib   default 1024, hard bound 32768
#
# A request beyond a bound fails with a diagnostic; the allocation
# itself never exceeds the requested amount. Strict POSIX sh: only
# utilities bundled with busybox.

cpu_seconds="${1:-1}"
memory_kib="${2:-1024}"
max_cpu_seconds=10
max_memory_kib=32768

case "$cpu_seconds" in
  '' | *[!0-9]*)
    echo "resource-hog: FAIL cpu_seconds must be a non-negative integer: $cpu_seconds" >&2
    exit 1
    ;;
esac
case "$memory_kib" in
  '' | *[!0-9]*)
    echo "resource-hog: FAIL memory_kib must be a non-negative integer: $memory_kib" >&2
    exit 1
    ;;
esac

if [ "$cpu_seconds" -gt "$max_cpu_seconds" ]; then
  echo "resource-hog: FAIL cpu bound of ${max_cpu_seconds}s exceeded: $cpu_seconds" >&2
  exit 1
fi
if [ "$memory_kib" -gt "$max_memory_kib" ]; then
  echo "resource-hog: FAIL memory bound of ${max_memory_kib} KiB exceeded: $memory_kib" >&2
  exit 1
fi

# Bounded CPU: spin until the requested wall-clock seconds elapse.
start=$(date +%s)
while [ "$(($(date +%s) - start))" -lt "$cpu_seconds" ]; do
  :
done

# Bounded memory: read memory_kib bytes of zeros from /dev/zero.
payload=$(head -c "$((memory_kib * 1024))" /dev/zero 2>/dev/null) || {
  echo "resource-hog: FAIL could not allocate $memory_kib KiB" >&2
  exit 1
}
: "$payload"

echo "PASS resource-hog: allocated ${cpu_seconds}s CPU and ${memory_kib} KiB memory"
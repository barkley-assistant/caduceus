#!/bin/sh
# network-probe probe (WR-4): verifies network reachability consistent
# with the configured network mode.
#
#   network-probe [none|unrestricted|auto]
#     none           require no reachability (sandbox network: none)
#     unrestricted   require reachability
#     auto           report reachability either way (default)
#
# The probe target defaults to 1.1.1.1:53 (DNS over TCP) and can be
# overridden with CADUCEUS_NETWORK_TARGET_HOST / CADUCEUS_NETWORK_TARGET_PORT.
# Strict POSIX sh: only utilities bundled with busybox.

mode="${1:-auto}"
target_host="${CADUCEUS_NETWORK_TARGET_HOST:-1.1.1.1}"
target_port="${CADUCEUS_NETWORK_TARGET_PORT:-53}"

case "$mode" in
  none | unrestricted | auto) ;;
  *)
    echo "network-probe: FAIL unknown network mode: $mode (expected none|unrestricted|auto)" >&2
    exit 1
    ;;
esac

# busybox nc has no -z flag; closing stdin immediately still proves the
# TCP connect result. Exit status 0 means the connection succeeded.
if nc -w 2 "$target_host" "$target_port" </dev/null >/dev/null 2>&1; then
  reachable=yes
else
  reachable=no
fi

case "$mode" in
  none)
    if [ "$reachable" = yes ]; then
      echo "network-probe: FAIL mode=none but $target_host:$target_port is reachable" >&2
      exit 1
    fi
    echo "PASS network-probe: mode=none, no reachability to $target_host:$target_port"
    ;;
  unrestricted)
    if [ "$reachable" = no ]; then
      echo "network-probe: FAIL mode=unrestricted but $target_host:$target_port is unreachable" >&2
      exit 1
    fi
    echo "PASS network-probe: mode=unrestricted, reachable to $target_host:$target_port"
    ;;
  auto)
    echo "PASS network-probe: mode=auto, reachability=$reachable to $target_host:$target_port"
    ;;
esac
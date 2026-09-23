#!/usr/bin/env bash
# SYNTHETIC privileged-boundary lifecycle test, not kernel ingress qualification.
# Pass the compiled native_host_linux_serial test binary. No production network,
# system bus, firewall, sysfs, or /run state is changed outside the fresh namespace.
set -euo pipefail
[[ $# == 1 && -x "$1" ]] || { echo 'usage: test_linux_serial_lifecycle.sh TEST_BINARY' >&2; exit 2; }
binary=$(realpath -- "$1")
parent_netns=$(readlink /proc/self/ns/net)
exec unshare --user --map-root-user --mount --net -- bash -ec '
  mount --make-rprivate /
  mount -t tmpfs tmpfs /sys
  mount -t tmpfs -o mode=0755 tmpfs /run
  mkdir -p /sys/class/net/fr-fixture
  printf "42\n" > /sys/class/net/fr-fixture/ifindex
  printf "1\n" > /sys/class/net/fr-fixture/flags
  printf "1\n" > /sys/class/net/fr-fixture/tun_flags
  printf "synthetic-only\n" > /run/fr-synthetic-ingress
  printf "%s\n" "$2" > /run/fr-parent-netns
  ip link set lo up
  ip addr add 100.64.0.1/32 dev lo
  ip addr add 100.64.0.2/32 dev lo
  exec "$1" --ignored --test-threads=1
' sh "$binary" "$parent_netns"

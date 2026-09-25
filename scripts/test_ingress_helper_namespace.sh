#!/usr/bin/env bash
# REAL nftables qualification of the ingress helper split, in a private
# network + mount namespace as real root (sudo). Nothing outside the fresh
# namespace is changed: its interfaces, second netns, nftables tables, /run,
# /sys and /tmp all disappear with it. Not a live-tailnet test.
#   scripts/test_ingress_helper_namespace.sh QUALIFY_EXAMPLE FRD_BINARY
# Build: cargo build -p fr-tailnet --example qualify_ingress_helper
#        cargo build -p frd --bin frd
set -euo pipefail
[[ $# == 2 && -x "$1" && -x "$2" ]] || {
  echo 'usage: test_ingress_helper_namespace.sh QUALIFY_EXAMPLE FRD_BINARY' >&2
  exit 2
}
example=$(realpath -- "$1")
frd=$(realpath -- "$2")
exec timeout 300s sudo unshare --mount --net -- bash -ec '
  mount --make-rprivate /
  mount -t sysfs sysfs /sys
  mount -t tmpfs -o mode=0755 tmpfs /run
  mount -t tmpfs -o mode=1777 tmpfs /tmp
  exec "$1" "$2"
' sh "$example" "$frd"

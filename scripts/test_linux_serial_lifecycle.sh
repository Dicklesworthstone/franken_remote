#!/usr/bin/env bash
# SYNTHETIC privileged-boundary lifecycle test, not kernel ingress qualification.
# Pass the compiled native_host_linux_serial test binary. No production network,
# system bus, firewall, sysfs, or /run state is changed outside the fresh namespace.
set -euo pipefail
[[ $# -ge 1 && -x "$1" ]] || { echo 'usage: test_linux_serial_lifecycle.sh TEST_BINARY [TEST_FILTER]' >&2; exit 2; }
binary=$(realpath -- "$1")
shift
parent_netns=$(readlink /proc/self/ns/net)
# Hosts that restrict unprivileged user namespaces (Ubuntu's AppArmor
# kernel.apparmor_restrict_unprivileged_userns=1) can opt into real root in the
# same fresh mount/net namespaces with FR_NS_SUDO=1.
if [[ "${FR_NS_SUDO:-0}" == 1 ]]; then
  enter=(sudo unshare --mount --net --)
else
  enter=(unshare --user --map-root-user --mount --net --)
fi
exec "${enter[@]}" bash -ec '
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
  ip -6 addr add fd7a:115c:a1e0::1/128 dev lo nodad
  ip -6 addr add fd7a:115c:a1e0::2/128 dev lo nodad
  # The live tailnet path: tailscale0 has MTU 1280 and Tailscale drops IP
  # fragments, so a QUIC datagram above the path MTU is lost, not reassembled.
  # The drop runs before conntrack defragmentation (priority -400).
  command -v nft >/dev/null || { echo "nft is required to emulate the tailnet path (MTU 1280, fragments dropped)" >&2; exit 2; }
  ip link set lo mtu 1280
  printf "%s\n" "table ip fr_tailnet_path {" \
    "  chain prerouting {" \
    "    type filter hook prerouting priority -450; policy accept;" \
    "    ip frag-off & 0x3fff != 0 counter drop" \
    "  }" "}" | nft -f -
  binary=$1
  shift 2
  exec "$binary" --ignored --test-threads=1 "$@"
' sh "$binary" "$parent_netns" "$@"

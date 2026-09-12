#!/usr/bin/env bash
# Explicit test-only overlay fixture. Never modifies the caller's network.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
if [[ "${1:-}" == --inside ]]; then
  test "${FR_DIAL_TEST_NAMESPACE:-}" = isolated
  ip link set lo up
  ip addr add 100.64.0.1/32 dev lo
  ip addr add 100.64.0.2/32 dev lo
  ip -6 addr add fd7a:115c:a1e0::1/128 dev lo nodad
  ip -6 addr add fd7a:115c:a1e0::2/128 dev lo nodad
  exec "$2" local::dial::tests::native_dial --ignored --test-threads=1 --nocapture
fi
mkdir -p target/qualification
cargo test -p fr-tailnet --locked --no-run --message-format=json > target/qualification/native-dial-build.json
binary="$(python3 - <<'PY'
import json, os
paths=[]
for line in open('target/qualification/native-dial-build.json'):
    row=json.loads(line)
    if row.get('reason')=='compiler-artifact' and row.get('target',{}).get('name')=='fr_tailnet' and row.get('profile',{}).get('test') and row.get('executable'):
        paths.append(os.path.abspath(row['executable']))
assert len(paths)==1, 'expected exactly one fr-tailnet unit-test executable'
print(paths[0])
PY
)"
# Requires Linux unprivileged user/network namespaces (or root). Failure is an
# explicit qualification failure; never run address changes outside a namespace.
unshare --user --map-root-user --net env FR_DIAL_TEST_NAMESPACE=isolated \
  bash "$0" --inside "$binary"

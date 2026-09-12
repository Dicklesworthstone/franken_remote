#!/usr/bin/env bash
# Actual public viewer connector inside an isolated, test-only overlay network.
# Never add an address in the caller's original network namespace.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
if [[ "${1:-}" == --inside ]]; then
  test "${FR_SESSION_TEST_NAMESPACE:-}" = isolated
  test -n "${FR_SESSION_ORIGINAL_NETNS:-}"
  test "$(readlink /proc/self/ns/net)" != "$FR_SESSION_ORIGINAL_NETNS"
  ip link set lo up
  ip addr add 100.64.0.1/32 dev lo
  ip addr add 100.64.0.2/32 dev lo
  exec "$2" native_connection::tests::namespace --ignored --test-threads=1 --nocapture
fi
mkdir -p target/qualification
cargo test -p frd --locked --lib --no-run --message-format=json > target/qualification/native-session-build.json
binary="$(python3 - <<'PYTHON'
import json, os
paths=[]
for line in open('target/qualification/native-session-build.json'):
    row=json.loads(line)
    if row.get('reason')=='compiler-artifact' and row.get('target',{}).get('name')=='frd' and row.get('profile',{}).get('test') and row.get('executable'):
        paths.append(os.path.abspath(row['executable']))
assert len(paths)==1, 'expected exactly one frd unit-test executable'
print(paths[0])
PYTHON
)"
export FR_SESSION_ORIGINAL_NETNS="$(readlink /proc/self/ns/net)"
unshare --user --map-root-user --net env FR_SESSION_TEST_NAMESPACE=isolated \
  bash "$0" --inside "$binary"

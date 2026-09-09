#!/usr/bin/env bash
# Reproducible driver for the fr-p0-quic-native-aqy qualification spike.
# Each scenario is its own process so CPU/socket measurements stay attributable.
set -euo pipefail
BIN="$(realpath "${1:?usage: run_all.sh <prebuilt-binary> <new-results-dir>}")"
RESULTS="$(realpath -m "${2:?provide a new results directory}")"
test -x "$BIN"
binary_identity="$(sha256sum "$BIN")"
# Refuse to overwrite historical measurements.
mkdir "$RESULTS"
cd "$(dirname "$0")"

{
  date -u +"%Y-%m-%dT%H:%M:%SZ"
  uname -a
  sha256sum "$BIN" Cargo.toml Cargo.lock src/*.rs
  echo "Dependency identity is in Cargo.lock; retain the matching RCH build log."
} | tee "$RESULTS/environment.txt"

failed=0
for scenario in self-pair tls-negative idle-cpu loss cancel interop-quinn-server interop-quinn-client; do
  if [ "$(sha256sum "$BIN")" != "$binary_identity" ]; then
    echo "Binary changed during the run; refusing mixed-build evidence." >&2
    exit 1
  fi
  echo "== scenario: $scenario =="
  status=0
  timeout -k 5s 180s "$BIN" "$scenario" 2>&1 | tee "$RESULTS/$scenario.log" || status=$?
  echo "$scenario exit=$status" | tee -a "$RESULTS/exits.txt"
  if [ "$status" -ne 0 ]; then failed=1; fi
done
if [ "$(sha256sum "$BIN")" != "$binary_identity" ]; then
  echo "Binary changed during the run; refusing mixed-build evidence." >&2
  exit 1
fi

echo "== summary =="
grep -h '^RESULT' "$RESULTS"/*.log || failed=1
exit "$failed"

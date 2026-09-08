#!/usr/bin/env bash
# Reproducible driver for the fr-p0-quic-native-aqy qualification spike.
# Each scenario is its own process so CPU/socket measurements stay attributable.
set -uo pipefail
cd "$(dirname "$0")"

mkdir -p results
BIN=target/release/quic-native-spike

echo "== environment identity ==" | tee results/environment.txt
{
  date -u +"%Y-%m-%dT%H:%M:%SZ"
  uname -a
  rustc --version
  cargo --version
  echo "asupersync: $(git -C ../../../asupersync rev-parse HEAD 2>/dev/null || echo 'not a git checkout') (path dep)"
  grep -E '^(quinn|rustls|rcgen) ' <(cargo tree --depth 1 2>/dev/null | sed 's/^[^a-z]*//') || true
} | tee -a results/environment.txt

cargo build --release 2>&1 | tail -3

for scenario in self-pair tls-negative idle-cpu loss cancel interop-quinn-server interop-quinn-client; do
  echo "== scenario: $scenario =="
  "$BIN" "$scenario" 2>&1 | tee "results/$scenario.log"
done

echo "== summary =="
grep -h '^RESULT' results/*.log

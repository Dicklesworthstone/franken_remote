#!/usr/bin/env bash
set -euo pipefail
cargo build --release -j 2
mkdir -p out
cp "${CARGO_TARGET_DIR:-target}/release/quic-native-spike" out/

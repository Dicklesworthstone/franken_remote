#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
RCH_REQUIRE_REMOTE=1 rch exec -- cargo build --locked --release -j 2 --manifest-path spikes/quic-native/Cargo.toml
mkdir -p spikes/quic-native/out
cp "${CARGO_TARGET_DIR:-spikes/quic-native/target}/release/quic-native-spike" spikes/quic-native/out/

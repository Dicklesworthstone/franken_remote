#!/usr/bin/env bash
# Exact production dependencies/test source, without unrelated daemon/HEVC dev
# dependencies. The full workspace gate is separate and is not weakened here.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
command -v "${FR_PULSE_BINARY:-pulseaudio}" >/dev/null || { echo 'BLOCKED: real PulseAudio daemon required' >&2; exit 2; }
command -v python3 >/dev/null
harness="$(mktemp -d "${TMPDIR:-/tmp}/fr-pulse-source-tests.XXXXXX")"
export FR_PULSE_CAPTURE_SCRIPT="$root/crates/fr-native/tests/pulse_support/capture.py"
cat > "$harness/Cargo.toml" <<MANIFEST
[package]
name = "fr-pulse-native-tests"
version = "0.1.0"
edition = "2024"
[workspace]
[features]
default = ["linux-pulse-playback"]
linux-pulse-playback = []
[dependencies]
fr-native = { path = "$root/crates/fr-native", default-features = false, features = ["linux-pulse-playback"] }
fr-core = { path = "$root/crates/fr-core" }
fr-wire = { path = "$root/crates/fr-wire" }
fr-client = { path = "$root/crates/fr-client" }
fr-media = { path = "$root/crates/fr-media" }
[[test]]
name = "pulse_playback"
path = "$root/crates/fr-native/tests/pulse_playback.rs"
[[test]]
name = "pulse_opus"
path = "$root/crates/fr-native/tests/pulse_opus.rs"
[lints.clippy]
pedantic = { level = "deny", priority = -1 }
missing_errors_doc = "allow"
missing_panics_doc = "allow"
module_name_repetitions = "allow"
must_use_candidate = "allow"
MANIFEST
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target/pulse-native}"
cargo test --manifest-path "$harness/Cargo.toml" --test pulse_playback --test pulse_opus -- --test-threads=1
cargo clippy --manifest-path "$harness/Cargo.toml" --all-targets -- -D warnings

#!/usr/bin/env bash
# Inside the builder image: record the toolchain and native library versions,
# then run each step of the all-features Rust lane independently, as CI does,
# and exit nonzero if any step failed. Run it through scripts/builder/run.sh.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
: "${CARGO_PROFILE_DEV_DEBUG:=0}" "${CARGO_PROFILE_TEST_DEBUG:=0}"
: "${CARGO_INCREMENTAL:=0}" "${RUST_MIN_STACK:=16777216}"
export CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_TEST_DEBUG CARGO_INCREMENTAL RUST_MIN_STACK

echo "== builder"
. /etc/os-release && echo "$PRETTY_NAME"
rustc --version
cargo --version
echo "== native packages"
dpkg-query -W -f='${Package} ${Version}\n' libpipewire-0.3-dev libavcodec-dev libavutil-dev \
    libswscale-dev libx11-dev libxcb1-dev libxtst6 libxi6 libxdamage1 libxrandr2 libxfixes3 \
    libx11-xcb1 libxcursor1 libopus0 libpulse0 pulseaudio xvfb nftables
echo "== pkg-config"
for module in libpipewire-0.3 libavcodec libavutil libswscale x11 xcb xcb-render; do
    echo "$module $(pkg-config --modversion "$module" 2>/dev/null || echo MISSING)"
done

failed=""
for step in fmt check clippy test examples; do
    echo "== verify.sh $step ($(date -u +%H:%M:%SZ))"
    ./scripts/verify.sh "$step" || failed="$failed $step"
done
echo "== done ($(date -u +%H:%M:%SZ))"
if [ -n "$failed" ]; then
    echo "Red steps:$failed"
    exit 1
fi
echo "All steps green"

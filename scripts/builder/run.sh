#!/usr/bin/env bash
# Build the pinned builder image and run a command in it against this checkout.
#
#   FR_BUILDER_TARGET=/scratch/fr-builder-target scripts/builder/run.sh            # smoke.sh
#   FR_BUILDER_TARGET=... scripts/builder/run.sh -- ./scripts/verify.sh clippy      # any command
#
# FR_BUILDER_TARGET (required): host directory used as CARGO_TARGET_DIR. An
#   all-features workspace test build needs tens of GB; never point it at a
#   target directory that a host-native build also uses.
# FR_BUILDER_CARGO_HOME: host directory for cargo's registry and git caches
#   (default: "$FR_BUILDER_TARGET.cargo-home").
# FR_BUILDER_TAG: image tag (default franken-remote-builder:local).
# CARGO_BUILD_JOBS, CARGO_PROFILE_*_DEBUG, CARGO_INCREMENTAL and RUST_MIN_STACK
#   pass through when set; smoke.sh defaults the last four to CI's values.
#
# The container runs as the caller's uid, so build output stays owned by the
# caller. It needs Docker (or a compatible CLI named by FR_BUILDER_DOCKER) and
# network access while building the image and fetching crates.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
docker=${FR_BUILDER_DOCKER:-docker}
tag=${FR_BUILDER_TAG:-franken-remote-builder:local}
target=${FR_BUILDER_TARGET:?set FR_BUILDER_TARGET to a host directory for build output}
cargo_home=${FR_BUILDER_CARGO_HOME:-$target.cargo-home}
mkdir -p "$target" "$cargo_home"
"$docker" build -f "$root/scripts/builder/Containerfile" -t "$tag" "$root/scripts/builder"
if [ "${1:-}" = "--" ]; then shift; fi
if [ $# -eq 0 ]; then set -- scripts/builder/smoke.sh; fi
# fr-files tests stage files on /dev/shm (tmpfs). Docker's default is 64 MiB,
# far below a CI runner's; 1 GiB keeps the container closer to CI.
exec "$docker" run --rm --init --shm-size=1g --user "$(id -u):$(id -g)" \
    -e HOME=/tmp -e CARGO_HOME=/cargo-home -e CARGO_TARGET_DIR=/target \
    -e CARGO_BUILD_JOBS -e CARGO_PROFILE_DEV_DEBUG -e CARGO_PROFILE_TEST_DEBUG \
    -e CARGO_INCREMENTAL -e RUST_MIN_STACK \
    -v "$root:/work" -v "$target:/target" -v "$cargo_home:/cargo-home" -w /work \
    "$tag" "$@"

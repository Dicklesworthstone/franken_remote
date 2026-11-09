#!/bin/sh
# Build the iOS app's Rust core (crates/fr-mobile-ffi, staticlib) for the
# platform and architectures Xcode is building, and put it where the app
# target links it ($TARGET_TEMP_DIR/fr-rust/libfr_mobile_ffi.a).
#
# Run by the Xcode project's "Build Rust core" phase. By hand:
#   PLATFORM_NAME=iphonesimulator ARCHS=arm64 CONFIGURATION=Debug \
#     FR_RUST_LIB_DIR=/tmp/fr-rust mobile/ios/scripts/build-rust.sh
#
# Optional environment:
#   FR_CARGO             cargo to run (default: $HOME/.cargo/bin/cargo)
#   FR_CARGO_TARGET_DIR  cargo target directory (default: <repo>/target/ios-app)
#   FR_CARGO_CONFIG      extra `cargo --config` file (e.g. a local [patch])
#
# Debug builds use cargo's dev profile at opt-level 2 (fast enough for live
# video and QUIC); Release uses the workspace release profile (LTO).
set -eu

script_dir=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$script_dir/../../.." && pwd)
out=${FR_RUST_LIB_DIR:-${TARGET_TEMP_DIR:?TARGET_TEMP_DIR or FR_RUST_LIB_DIR must be set}/fr-rust}
cargo=${FR_CARGO:-$HOME/.cargo/bin/cargo}
target_dir=${FR_CARGO_TARGET_DIR:-$repo/target/ios-app}
platform=${PLATFORM_NAME:?PLATFORM_NAME must be set}
archs=${ARCHS:-arm64}
configuration=${CONFIGURATION:-Debug}

case "$platform" in
    iphonesimulator) ;;
    iphoneos) ;;
    *) echo "error: build-rust.sh: unsupported platform '$platform'" >&2; exit 1 ;;
esac

if [ "$configuration" = "Release" ]; then
    profile_args="--release"
    profile_dir=release
else
    profile_args="--profile dev"
    profile_dir=debug
fi

mkdir -p "$out"
triples=""
for arch in $archs; do
    case "$platform/$arch" in
        iphonesimulator/arm64) triple=aarch64-apple-ios-sim ;;
        iphonesimulator/x86_64) triple=x86_64-apple-ios ;;
        iphoneos/arm64) triple=aarch64-apple-ios ;;
        *) echo "error: build-rust.sh: unsupported architecture '$arch' for $platform" >&2; exit 1 ;;
    esac
    echo "build-rust.sh: fr-mobile-ffi for $triple ($profile_dir)"
    # A clean environment: Xcode's SDKROOT and compiler settings are for the
    # app, not for cargo's host build scripts. xcrun (via DEVELOPER_DIR)
    # selects each SDK explicitly.
    # $profile_args is a fixed word list. Paths stay single words even with
    # spaces (FR_CARGO_CONFIG through ${:+"..."}).
    # shellcheck disable=SC2086
    (cd "$repo" && env -i \
        HOME="$HOME" \
        USER="${USER:-}" \
        PATH="$HOME/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
        TMPDIR="${TMPDIR:-/tmp}" \
        LANG=en_US.UTF-8 \
        DEVELOPER_DIR="${DEVELOPER_DIR:-$(xcode-select -p)}" \
        IPHONEOS_DEPLOYMENT_TARGET=17.0 \
        CARGO_TARGET_DIR="$target_dir" \
        CARGO_PROFILE_DEV_OPT_LEVEL=2 \
        CARGO_PROFILE_DEV_DEBUG=line-tables-only \
        "$cargo" ${FR_CARGO_CONFIG:+--config} ${FR_CARGO_CONFIG:+"$FR_CARGO_CONFIG"} \
            build -p fr-mobile-ffi --lib --features session \
            --target "$triple" $profile_args)
    triples="$triples $triple"
done

staged="$out/libfr_mobile_ffi.a.new"
set --
for triple in $triples; do
    set -- "$@" "$target_dir/$triple/$profile_dir/libfr_mobile_ffi.a"
done
if [ "$#" -eq 1 ]; then
    cp "$1" "$staged"
else
    xcrun lipo -create "$@" -output "$staged"
fi
# Replace only on change, so an unchanged core does not force a relink.
if cmp -s "$staged" "$out/libfr_mobile_ffi.a"; then
    rm -f "$staged"
else
    mv -f "$staged" "$out/libfr_mobile_ffi.a"
fi

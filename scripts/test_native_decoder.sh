#!/usr/bin/env bash
# Actual libavcodec/HEVC decode, identical output, and TSYNC on every slice thread.
# Timing rows report only native CPU decode plus BGRA conversion on this host.
set -euo pipefail
cd "$(dirname "$0")/.."
work="${FR_NATIVE_DECODER_TEST_DIR:-$(mktemp -d /tmp/fr-native-decoder.XXXXXX)}"
mkdir -p "$work"
native_flags_text="$(pkg-config --cflags --libs libavcodec libavutil libswscale x11 xcb)"
read -r -a native_flags <<< "$native_flags_text"
"${CC:-cc}" -std=c11 -O2 -Wall -Wextra -Werror=implicit-function-declaration \
  crates/fr-native/tests/decoder_threads.c "${native_flags[@]}" \
  -l:libX11-xcb.so.1 -o "$work/decoder-threads"
timeout 120 "$work/decoder-threads"
timeout 120 "$work/decoder-threads" desktop
timeout 120 "$work/decoder-threads" noise

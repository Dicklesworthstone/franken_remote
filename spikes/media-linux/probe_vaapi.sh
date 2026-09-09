#!/usr/bin/env bash
# Installed-driver startup preflight only; no desktop capture or codec fallback.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo 'usage: bash spikes/media-linux/probe_vaapi.sh /dev/dri/renderD128' >&2
  exit 2
fi
case "$1" in
  /dev/dri/renderD[0-9]*) ;;
  *) echo 'expected an explicit render-device path' >&2; exit 2 ;;
esac
if [ ! -c "$1" ]; then
  echo 'render device is absent or is not a character device' >&2
  exit 2
fi

exec timeout 20s ffmpeg -hide_banner -nostdin -loglevel verbose \
  -init_hw_device "vaapi=probe:$1" -filter_hw_device probe \
  -f lavfi -i testsrc2=size=1920x1080:rate=30 \
  -vf format=nv12,hwupload -an -c:v hevc_vaapi -profile:v main \
  -bf 0 -g 60 -async_depth 1 -frames:v 2 -f null -

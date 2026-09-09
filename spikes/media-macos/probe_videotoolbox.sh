#!/usr/bin/env bash
# Installed-codec preflight, not ScreenCaptureKit or presentation qualification.
set -euo pipefail
if [ "$#" -ne 1 ] || [ "$(uname -s)" != Darwin ]; then
  echo 'usage on macOS: bash probe_videotoolbox.sh NEW_OUTPUT_DIRECTORY' >&2
  exit 2
fi
for tool in ffmpeg ffprobe gtimeout python3; do
  command -v "$tool" >/dev/null
done
# Require a new directory; never overwrite a previous result or delete captures.
mkdir -- "$1"
cd -- "$1"
sw_vers > environment.txt
sysctl -n machdep.cpu.brand_string >> environment.txt
ffmpeg -version >> environment.txt
python3 - <<'PY' > capture-permission.txt
import ctypes
c = ctypes.CDLL('/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics')
c.CGPreflightScreenCaptureAccess.restype = ctypes.c_bool
print('screen_capture_preflight_granted=' + str(c.CGPreflightScreenCaptureAccess()))
print('scope=invoking execution context; no permission request or capture performed')
PY
gtimeout --kill-after=2s 20s ffmpeg -hide_banner -nostdin -xerror -loglevel verbose \
  -f lavfi -i testsrc2=size=1920x1080:rate=30 -vf format=nv12 -an \
  -c:v hevc_videotoolbox -profile:v main -allow_sw 0 -realtime 1 \
  -bf 0 -max_ref_frames 1 -g 60 -b:v 8M -frames:v 60 \
  -fs 16777216 -f hevc probe.hevc > encode.log 2>&1
gtimeout --kill-after=2s 20s ffmpeg -hide_banner -nostdin -xerror -max_error_rate 0 -loglevel verbose \
  -progress decode-progress.txt \
  -hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld \
  -i probe.hevc -an -frames:v 60 -f null - > decode.log 2>&1
gtimeout --kill-after=2s 20s ffprobe -v error -select_streams v:0 \
  -show_entries stream=codec_name,profile,width,height,pix_fmt,has_b_frames,refs \
  -show_entries frame=pict_type,key_frame -of json probe.hevc \
  > stream.json 2> stream.stderr
python3 - <<'PY' > stream-summary.json
import collections
import json
import pathlib
import re

with open('probe.hevc', 'rb') as source:
    data = source.read(16777217)
if len(data) > 16777216:
    raise SystemExit('encoded output exceeds preflight bound')
units = [u for u in re.split(b'\x00\x00\x00?\x01', data) if u]
if not units or any(len(u) < 2 for u in units):
    raise SystemExit('missing or truncated NAL headers')
types = [(u[0] >> 1) & 63 for u in units]
first_vcl = next((t for t in types if t < 32), None)
if first_vcl not in (19, 20):
    raise SystemExit('startup VCL is not an IDR NAL')
probe = json.loads(pathlib.Path('stream.json').read_text())
frames = probe['frames']
stream = probe['streams'][0]
if len(frames) != 60 or stream['codec_name'] != 'hevc' or stream['profile'] != 'Main':
    raise SystemExit('unexpected frame count or HEVC profile')
if (stream['width'], stream['height'], stream['pix_fmt']) != (1920, 1080, 'yuv420p'):
    raise SystemExit('unexpected geometry or pixel format')
if stream['has_b_frames'] != 0 or any(f['pict_type'] == 'B' for f in frames):
    raise SystemExit('unexpected reordered/B frames')
progress = dict(line.split('=', 1) for line in
                pathlib.Path('decode-progress.txt').read_text().splitlines() if '=' in line)
if progress.get('frame') != '60' or progress.get('progress') != 'end':
    raise SystemExit('VideoToolbox decode did not complete 60 frames')
if 'pixfmt:videotoolbox_vld' not in pathlib.Path('decode.log').read_text():
    raise SystemExit('VideoToolbox output format was not observed')
reference_request = ('ignored' if 'max_ref_frames option. Value ignored.' in
                     pathlib.Path('encode.log').read_text() else 'unverified')
print(json.dumps({'bytes': len(data), 'nal_types': dict(collections.Counter(types)),
                  'first_vcl': first_vcl, 'stream': stream,
                  'reference_limit_request': reference_request,
                  'decode_reported_frames': int(progress['frame']),
                  'frame_types': dict(collections.Counter(f['pict_type'] for f in frames)),
                  'scope': 'header inventory and FFmpeg inspection, not full HEVC admission'}, indent=2))
PY
printf '%s\n' 'codec_preflight_completed; capture and presentation remain unqualified'

#!/bin/bash
# Client side of scripts/live/two_node.sh. Every artifact lives in WORK/R.
# Subcommands: prepare | connect | drive | clip-read | finish | cleanup
set -u
CMD=$1; WORK=$2; N=$3; R=$4; shift 4
H=$(dirname "$0"); D=$WORK/$R
export $(bash "$H/display.sh" "$N" "$WORK")
viewer() { for _ in $(seq 1 60); do W=$(xdotool search --name '^FrankenRemote$' 2>/dev/null | head -1); [ -n "$W" ] && break; sleep 0.5; done; echo "${W:-}"; }
case $CMD in
prepare) # STOP
  mkdir -p "$D"; head -c 100000 /dev/urandom > "$D/send.bin"
  echo "file_sha256=$(sha256sum "$D/send.bin" | cut -d' ' -f1)"
  bash "$H/pulse.sh" "$D/pa" fr_view > /dev/null
  (env HOME="$D/pa" XDG_RUNTIME_DIR="$D/pa" PULSE_RUNTIME_PATH="$D/pa" setsid nohup \
    python3 -u "$H/tone_recorder.py" "unix:$D/pa/native" fr_view "$1" "$D/rec.bin" > "$D/rec.log" 2>&1 < /dev/null &) ;;
connect) # BIN NODE
  (env HOME="$D/pa" XDG_CONFIG_HOME="$D/pa" XDG_RUNTIME_DIR="$D/pa" PULSE_RUNTIME_PATH="$D/pa" \
    PULSE_SERVER="unix:$D/pa/native" setsid nohup timeout 180 "$1/fr" connect "$2" --control \
    --clipboard --send "$D/send.bin" --audio --experimental-native --display only \
    --x-display "$DISPLAY" --worker "$1/fr-media-worker" --json > "$D/client.log" 2> "$D/client.err" < /dev/null &) ;;
drive) # TEXT
  W=$(viewer); echo "viewer=${W:-none}"; [ -z "$W" ] && exit 0
  xdotool windowfocus --sync "$W"; sleep 4
  echo "view=$(python3 "$H/window_colour.py" "$DISPLAY" "$W" 336699)"
  eval "$(xdotool getwindowgeometry --shell "$W")"
  xdotool mousemove $((X + 800)) $((Y + 550)); sleep 1
  xdotool type --delay 120 hello; sleep 1
  xdotool click 4; sleep 0.5; xdotool click 5; sleep 3
  echo "clip_before=$(timeout 3 xclip -o -selection clipboard 2>/dev/null)"
  (printf '%s' "$1" | setsid nohup bash -c 'exec -a "fr-live-clip-$0" xclip -selection clipboard -loops 0' "$R" > /dev/null 2>&1 &) ;;
clip-read) timeout 3 xclip -o -selection clipboard 2>/dev/null; echo ;;
finish) # SWITCH BIN NODE
  W=$(viewer); [ -n "$W" ] && python3 "$H/close_window.py" "$DISPLAY" "$W"
  for _ in $(seq 1 40); do [ -s "$D/client.log" ] && break; sleep 0.5; done
  echo "completion=$(cat "$D/client.log")"
  python3 "$H/tone_analyze.py" "$D/rec.bin" 450 1050 "$1"
  "$2/fr" hosts --json 2>/dev/null | python3 -c "
import json, sys
peers = json.load(sys.stdin).get('peers', [])
print('path=' + str(next((p.get('transport_path') or {}).get('kind') for p in peers if p.get('node_id') == sys.argv[1] or p.get('certificate_name', '').split('.')[0] == sys.argv[1])))
" "$3" ;;
cleanup)
  pkill -f "$D/pa/daemon[.]pa"; pkill -f "fr-live-clip-$R"; true ;;
esac

#!/usr/bin/env bash
# LIVE two-node check (bead fr-rc2-live-lane-a6ug): one controlled session between
# two real tailnet nodes exercises view, keys, wheel, clipboard both ways (with
# the host's pre-session copy), a file send and host playback audio.
#
#   scripts/live/two_node.sh HOST_SSH CLIENT_SSH HOST_NODE HOST_BIN CLIENT_BIN [OUT]
#
# HOST_SSH/CLIENT_SSH are ssh destinations; the nodes never ssh to each other.
# HOST_NODE is what `fr connect` dials (node ID or name). HOST_BIN holds frd,
# fr-media-worker and fr-input-agent; CLIENT_BIN holds fr, fr-media-worker and
# fr-opus-worker. Env: WORK (directory under each node's home, default
# fr_live), HOST_DISPLAY (77), CLIENT_DISPLAY (78).
# Host needs sudo, Xvfb, xauth, mcookie, xsetroot, xclock, xev, xclip, python3,
# pulseaudio; the client needs Xvfb, xauth, mcookie, xdotool, xclip, python3,
# pulseaudio. Displays are private Xvfb servers and audio uses private
# PulseAudio null sinks: this is not evidence for a desktop session, speakers,
# a compositor or physical input. The raw transcript names tailnet nodes:
# keep OUT out of git; summary.txt holds only rows and results.
set -u
[ $# -ge 5 ] || { sed -n '2,20p' "$0"; exit 2; }
HS=$1; CS=$2; NODE=$3; HB=$4; CB=$5
OUT=${6:-${TMPDIR:-/tmp}/fr-live-$(date +%Y%m%dT%H%M%S)}
HN=${HOST_DISPLAY:-77}; CN=${CLIENT_DISPLAY:-78}
HW=$(ssh "$HS" 'echo $HOME')/${WORK:-fr_live}; CW=$(ssh "$CS" 'echo $HOME')/${WORK:-fr_live}
R=$(date +%s); SELF=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"; exec > >(tee "$OUT/transcript.txt") 2>&1
host() { ssh "$HS" "bash $HW/live/host.sh $1 $HW $HN $R ${*:2}"; }
client() { ssh "$CS" "bash $CW/live/client.sh $1 $CW $CN $R ${*:2}"; }
value() { grep -m1 "^$1=" | cut -d= -f2-; }

echo "== run $R; source $(git -C "$SELF" rev-parse --short HEAD 2>/dev/null)$(git -C "$SELF" diff --quiet 2>/dev/null || echo +dirty)"
ssh "$HS" "mkdir -p $HW/live" && scp -q "$SELF"/node/* "$HS:$HW/live/"
ssh "$CS" "mkdir -p $CW/live" && scp -q "$SELF"/node/* "$CS:$CW/live/"
echo "host binaries: $(ssh "$HS" "sha256sum $HB/frd $HB/fr-media-worker $HB/fr-input-agent | cut -c1-16 | tr '\n' ' '")"
echo "client binaries: $(ssh "$CS" "sha256sum $CB/fr $CB/fr-media-worker $CB/fr-opus-worker | cut -c1-16 | tr '\n' ' '")"
echo "== host: display, xev, pre-session copy, drop dir, PulseAudio"; host prepare
NOW=$(ssh "$HS" date +%s); SWITCH=$((NOW + 40)); STOP=$((NOW + 70))
host player "$SWITCH" "$STOP"
echo "== host: frd run"; host serve "$HB"
echo "== client: file, PulseAudio + recorder, fr connect"
SENT=$(client prepare "$STOP" | tee /dev/stderr | value file_sha256)
client connect "$CB" "$NODE"
echo "== client: focus, view, keys, wheel, copy"
HOST_VIEW=$(host view); echo "host root samples: $HOST_VIEW"
DRIVE=$(client drive "fr-live-client-$R" | tee /dev/stderr)
sleep 8; HOST_CLIP=$(host clip-read); echo "host clipboard: $HOST_CLIP"
host clip-copy "fr-live-host-after-$R"; sleep 8
CLIENT_CLIP=$(client clip-read); echo "client clipboard: $CLIENT_CLIP"
WAIT=$((STOP - $(date +%s) + 2)); [ "$WAIT" -gt 0 ] && { echo "== audio: waiting ${WAIT}s"; sleep "$WAIT"; }
echo "== client: close viewer, completion, audio analysis, path"
FINISH=$(client finish "$SWITCH" "$CB" "$NODE" | tee /dev/stderr)
echo "== host evidence"; EVIDENCE=$(host evidence send.bin | tee /dev/stderr)
host cleanup; client cleanup

COMPLETION=$(echo "$FINISH" | value completion)
field() { echo "$COMPLETION" | python3 -c "import json,sys
try: print(json.load(sys.stdin).get(sys.argv[1]))
except Exception: print(None)" "$1"; }
VIEW=$(echo "$DRIVE" | value view)
row() { printf '%-28s %-4s %s\n' "$1" "$2" "$3"; }
{
  echo "run $R ($(date -u +%FT%TZ)); path $(echo "$FINISH" | value path)"
  row session_open "$([ "$(field opened)" != None ] && [ "$(field opened)" -ge 1 ] 2>/dev/null && echo PASS || echo FAIL)" "opened=$(field opened) role=$(field role)"
  # The same 64x36 grid over the host root and the (same-size) viewer window.
  row view_pixels "$(python3 -c "import sys
h, v = (int(a.split()[0].split('/')[0]) / int(a.split()[0].split('/')[1]) for a in sys.argv[1:3])
print('PASS' if h >= 0.2 and abs(h - v) <= 0.1 else 'FAIL')" "$HOST_VIEW" "$VIEW" 2>/dev/null || echo FAIL)" "#336699 samples: host $HOST_VIEW, viewer $VIEW"
  row control_keys "$([ "$(echo "$EVIDENCE" | value keys)" = hello ] && echo PASS || echo FAIL)" "host saw '$(echo "$EVIDENCE" | value keys)'"
  row control_wheel "$([ "$(echo "$EVIDENCE" | value wheel)" = 45 ] && echo PASS || echo FAIL)" "host buttons '$(echo "$EVIDENCE" | value wheel)'"
  row clipboard_host_presession "$([ "$(echo "$DRIVE" | value clip_before)" = "fr-live-host-before-$R" ] && echo PASS || echo FAIL)" ""
  row clipboard_client_to_host "$([ "$HOST_CLIP" = "fr-live-client-$R" ] && echo PASS || echo FAIL)" ""
  row clipboard_host_to_client "$([ "$CLIENT_CLIP" = "fr-live-host-after-$R" ] && echo PASS || echo FAIL)" ""
  row file_send "$([ -n "$SENT" ] && [ "$(echo "$EVIDENCE" | value file_sha256)" = "$SENT" ] && echo PASS || echo FAIL)" "100000 bytes, sha256 match"
  row audio_tone "$([ "$(echo "$FINISH" | value verdict)" = pass ] && echo PASS || echo FAIL)" "switch->client $(echo "$FINISH" | value switch_to_client_detect_ms) ms; $(echo "$FINISH" | grep -o 'sustained_hz2_blocks=[0-9/]*')"
  row orderly_end "$([ "$(field outcome)" = stopped ] && echo "$EVIDENCE" | grep -q 'host_events=.*stopped' && echo PASS || echo FAIL)" "client $(field outcome)"
} | tee "$OUT/summary.txt"

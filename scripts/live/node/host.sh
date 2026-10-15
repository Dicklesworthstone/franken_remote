#!/bin/bash
# Host side of scripts/live/two_node.sh. Every artifact lives in WORK/R.
# Subcommands: prepare | player | serve | view | clip-read | clip-copy | evidence | cleanup
set -u
CMD=$1; WORK=$2; N=$3; R=$4; shift 4
H=$(dirname "$0"); D=$WORK/$R
export $(bash "$H/display.sh" "$N" "$WORK")
case $CMD in
prepare)
  mkdir -p "$D"; xsetroot -solid "#336699"
  (setsid nohup xev -geometry 600x400+600+400 -name "fr-live-xev-$R" > "$D/xev.log" 2>&1 < /dev/null &)
  (printf 'fr-live-host-before-%s' "$R" | setsid nohup bash -c 'exec -a "fr-live-clip-$0" xclip -selection clipboard -loops 0' "$R" > /dev/null 2>&1 &)
  sudo mkdir -m 700 "$D/drop"
  bash "$H/pulse.sh" "$D/pa" fr_host ;;
player) # SWITCH STOP
  (env HOME="$D/pa" XDG_RUNTIME_DIR="$D/pa" PULSE_RUNTIME_PATH="$D/pa" setsid nohup \
    python3 -u "$H/tone_player.py" "unix:$D/pa/native" fr_host 450 1050 "$1" "$2" > "$D/player.log" 2>&1 < /dev/null &)
  sleep 1; head -1 "$D/player.log" ;;
serve) # BIN: root frd in a private MOUNT namespace (tmpfs /root keeps libpulse's
  # root cookie off the real /root); the network namespace is the host's.
  B=$1
  printf '#!/bin/bash\nmount -t tmpfs -o mode=0700 tmpfs /root\nexec env DISPLAY=%s XAUTHORITY=%s %s/frd run --encoder software --display %s --worker %s/fr-media-worker --input-agent %s/fr-input-agent --clipboard --files %s/drop --audio --audio-server %s/pa/native --once --json\n' \
    "$DISPLAY" "$XAUTHORITY" "$B" "$DISPLAY" "$B" "$B" "$D" "$D" > "$D/serve.sh"
  chmod +x "$D/serve.sh"
  (sudo setsid nohup unshare --mount --propagation private "$D/serve.sh" > "$D/host.log" 2>&1 < /dev/null &)
  for _ in $(seq 1 120); do grep -q '"listening"' "$D/host.log" && break; sleep 0.5; done
  grep -q '"listening"' "$D/host.log" && echo listening || { echo not-listening; tail -3 "$D/host.log"; } ;;
view) python3 "$H/window_colour.py" "$DISPLAY" root 336699 ;;
clip-read) timeout 3 xclip -o -selection clipboard 2>/dev/null; echo ;;
clip-copy) (printf '%s' "$1" | setsid nohup bash -c 'exec -a "fr-live-clip-$0" xclip -selection clipboard -loops 0' "$R" > /dev/null 2>&1 &) ;;
evidence) # FILE_NAME
  echo "keys=$(grep -A2 '^KeyPress event' "$D/xev.log" | grep -o 'keysym 0x[0-9a-f]*, [a-zA-Z_0-9]*' | awk '{print $3}' | tr -d ',\n')"
  echo "wheel=$(grep -A2 'ButtonPress' "$D/xev.log" | grep -o 'button [0-9]*' | awk '{print $2}' | tr -d '\n')"
  echo "file_sha256=$(sudo sha256sum "$D/drop/$1" 2>/dev/null | cut -d' ' -f1)"
  sleep 2; echo "host_events=$(grep -o '"event":"[a-z_]*"' "$D/host.log" | cut -d'"' -f4 | tr '\n' ' ')"
  tail -c 600 "$D/host.log" ;;
cleanup)
  pkill -f "fr-live-xev-$R" ; pkill -f "$D/pa/daemon[.]pa"; pkill -f "fr-live-clip-$R"; true ;;
esac

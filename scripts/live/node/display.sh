#!/bin/bash
# Start a private cookie-authenticated Xvfb :N (1920x1080) with a solid
# #336699 root and an xclock, unless one already runs. Prints DISPLAY and
# XAUTHORITY assignments for the caller to export.
set -eu
N=$1; DIR=$2
XA=$DIR/xauth$N
if ! pgrep -x Xvfb -a | grep -q " :$N "; then
  : > "$XA"; chmod 600 "$XA"
  xauth -f "$XA" add ":$N" . "$(mcookie)"
  (setsid nohup Xvfb ":$N" -screen 0 1920x1080x24 -auth "$XA" -nolisten tcp > "$DIR/xvfb$N.log" 2>&1 < /dev/null &)
  for _ in $(seq 1 50); do [ -e "/tmp/.X11-unix/X$N" ] && break; sleep 0.1; done
  DISPLAY=":$N" XAUTHORITY="$XA" xsetroot -solid "#336699"
  (DISPLAY=":$N" XAUTHORITY="$XA" setsid nohup xclock -geometry 300x300+1500+100 -update 1 > /dev/null 2>&1 < /dev/null &)
fi
echo "DISPLAY=:$N XAUTHORITY=$XA"

#!/bin/bash
# Start a private PulseAudio daemon in DIR (0700) whose only sink is the null
# sink SINK, with anonymous auth on a socket inside DIR. Prints the socket.
set -eu
D=$1; S=$2
mkdir -m 700 -p "$D"
printf 'load-module module-native-protocol-unix socket=%s/native auth-anonymous=1\nload-module module-null-sink sink_name=%s format=s16le rate=48000 channels=2 norewinds=1\nset-default-sink %s\n' "$D" "$S" "$S" > "$D/daemon.pa"
(env -u DBUS_SESSION_BUS_ADDRESS HOME="$D" XDG_CONFIG_HOME="$D" XDG_RUNTIME_DIR="$D" PULSE_RUNTIME_PATH="$D" \
  setsid nohup pulseaudio --daemonize=no --use-pid-file=no --exit-idle-time=-1 --disable-shm=yes \
  --log-level=warning -nF "$D/daemon.pa" > /dev/null 2> "$D/daemon.stderr" < /dev/null &)
for _ in $(seq 1 100); do [ -S "$D/native" ] && break; sleep 0.05; done
[ -S "$D/native" ] && echo "$D/native"

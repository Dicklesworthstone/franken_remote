#!/bin/zsh
# Build, sign and install the FrankenRemote macOS host (a per-user
# LaunchAgent) and the `fr` client, on this Mac or on another Mac over SSH.
#
#   scripts/macos/install-host.sh [--remote HOST] [--view-only]
#       [--identity "Developer ID Application: ..."] [--skip-build]
#       [-- extra frd run options]
#
# - Builds release arm64 binaries here (never on the target Mac).
# - Signs each with a stable identifier and the hardened runtime. A
#   Developer ID signature keeps the Screen Recording and Accessibility
#   grants across updates; an ad-hoc signature would lose them every build.
# - Installs into ~/Library/Application Support/FrankenRemote/bin on the
#   target, replacing each binary by rename (a running frd keeps its file).
# - Runs `frd install` there, then (re)loads the agent with launchctl.
# By default the host allows control (--input-agent) at 60 fps and 30 Mbit/s;
# --view-only omits control.
# Sharing stays limited to your own devices (--approval none --sharing
# own-user) unless you pass other frd run options.
set -euo pipefail

repo=${0:A:h:h:h}
remote=""
view_only=0
skip_build=0
identity=${FR_SIGNING_IDENTITY:-}
extra=()
while (( $# )); do
  case $1 in
    --remote) remote=$2; shift 2 ;;
    --view-only) view_only=1; shift ;;
    --identity) identity=$2; shift 2 ;;
    --skip-build) skip_build=1; shift ;;
    --) shift; extra=("$@"); break ;;
    *) print -u2 "unknown option: $1"; exit 2 ;;
  esac
done

if [[ -z $identity ]]; then
  identity=$(security find-identity -v -p codesigning |
    sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -n 1)
fi
if [[ -z $identity ]]; then
  print -u2 "no Developer ID Application identity: pass --identity (an ad-hoc signature would lose the macOS permission grants at every update)"
  exit 2
fi

target=${CARGO_TARGET_DIR:-$repo/target}
release=$target/release
if (( ! skip_build )); then
  (cd "$repo" &&
    cargo build --release --locked -p frd --features macos-host --bin frd &&
    cargo build --release --locked -p fr-native \
      --features macos-media,macos-input,macos-desktop,macos-session-monitor \
      --bin fr-media-worker --bin fr-input-agent --bin fr-session-monitor --bin fr &&
    cargo build --release --locked -p fr-native --no-default-features \
      --features macos-observation-indicator --bin fr-observation-indicator)
fi

stage=$(mktemp -d "${TMPDIR:-/tmp}/fr-install.XXXXXX")
trap 'rm -f "$stage"/frd "$stage"/fr-media-worker "$stage"/fr-input-agent "$stage"/fr-session-monitor "$stage"/fr-observation-indicator "$stage"/fr; rmdir "$stage"' EXIT
for name in frd fr-media-worker fr-input-agent fr-session-monitor fr-observation-indicator fr; do
  cp "$release/$name" "$stage/$name"
  codesign --force --options runtime --timestamp \
    --identifier "com.frankenremote.$name" --sign "$identity" "$stage/$name"
  codesign --verify --strict "$stage/$name"
done

bin='Library/Application Support/FrankenRemote/bin'
# frd's built-in default is 30 fps at 8 Mbit/s; a desktop host on a tailnet
# should be smooth and sharp, so ask for 60 fps at 30 Mbit/s (both are caps:
# capture follows screen changes). Override after `--`.
run_args=(--encoder videotoolbox --fps 60 --bitrate 30000000)
if (( ! view_only )); then
  run_args+=(--input-agent "__BIN__/fr-input-agent")
fi
run_args+=("${extra[@]}")

# The target-side half, run by zsh on the target Mac with its own HOME/uid.
# Arguments: the staging directory, then frd run options (__BIN__ is the
# installed bin directory).
target_side='
set -euo pipefail
stage=$1; shift
bin="$HOME/Library/Application Support/FrankenRemote/bin"
mkdir -p -m 700 "$bin"
for name in frd fr-media-worker fr-input-agent fr-session-monitor fr-observation-indicator fr; do
  cp "$stage/$name" "$bin/.$name.new"
  chmod 755 "$bin/.$name.new"
  mv -f "$bin/.$name.new" "$bin/$name"
done
args=()
for arg in "$@"; do args+=("${arg//__BIN__/$bin}"); done
"$bin/frd" install --approval none --sharing own-user -- "${args[@]}"
domain=gui/$(id -u)
launchctl bootout "$domain/com.frankenremote.frd" 2>/dev/null || true
# bootout returns while the old frd is still ending its share; bootstrapping
# before launchd has let it go fails with "5: Input/output error" and leaves
# no agent loaded. Wait (bounded) until the old one is gone.
for _ in {1..100}; do
  launchctl print "$domain/com.frankenremote.frd" >/dev/null 2>&1 || break
  sleep 0.2
done
launchctl bootstrap "$domain" "$HOME/Library/LaunchAgents/com.frankenremote.frd.plist"
sleep 2
launchctl print "$domain/com.frankenremote.frd" | grep -E "^\s*(state|pid|last exit code) ="
print "log: $HOME/Library/Logs/FrankenRemote/frd.log"
'

if [[ -z $remote ]]; then
  zsh -c "$target_side" install "$stage" "${run_args[@]}"
else
  remote_stage=$(ssh "$remote" 'mktemp -d "${TMPDIR:-/tmp}/fr-install.XXXXXX"')
  scp -q "$stage"/frd "$stage"/fr-media-worker "$stage"/fr-input-agent "$stage"/fr-session-monitor "$stage"/fr-observation-indicator "$stage"/fr "$remote:$remote_stage/"
  ssh "$remote" zsh -c "${(q)target_side}" install "${(q)remote_stage}" "${(q@)run_args}"
  ssh "$remote" "rm -f ${(q)remote_stage}/frd ${(q)remote_stage}/fr-media-worker ${(q)remote_stage}/fr-input-agent ${(q)remote_stage}/fr-session-monitor ${(q)remote_stage}/fr-observation-indicator ${(q)remote_stage}/fr && rmdir ${(q)remote_stage}"
fi
print "installed ${bin} and the LaunchAgent on ${remote:-this Mac}"

#!/usr/bin/env bash
#
# FrankenRemote installer for macOS and Linux
#
#   curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/franken_remote/main/install.sh?$(date +%s)" | bash
#
# Also make this machine a host other devices can connect to (a macOS
# LaunchAgent, or a Linux systemd user service):
#
#   curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/franken_remote/main/install.sh?$(date +%s)" | bash -s -- --host
#
# The `?$(date +%s)` suffix only defeats stale CDN caches. Windows uses
# install.ps1 instead.
#
# What it does:
#   - checks the platform, disk space, network and Tailscale (offers to
#     install Tailscale)
#   - installs the latest release for this platform, verified with SHA-256 and,
#     when cosign is present, its Sigstore bundle; without a usable release it
#     builds from source, installing the build prerequisites it needs
#   - puts `fr` (and `frd`) on PATH, installs shell completions and an
#     agent skill for Claude Code / Codex
#   - with --host, registers the host service and walks through the
#     permissions it needs
#
# Options (see --help for all):
#   --version vX.Y.Z   Install that release (default: latest release)
#   --ref REF          Source build of a branch, tag or commit (default: main)
#   --from-source      Build from source even when a release exists
#   --client-only      Install the viewer only (no host binaries)
#   --host             Also set up this machine as a FrankenRemote host
#   --view-only        A host that never accepts remote control
#   --no-clipboard     A host that never shares the clipboard
#   --audio            Build and offer host playback audio (macOS needs Homebrew opus)
#   --dest DIR         Directory for the fr/frd commands (default: ~/.local/bin)
#   --easy-mode        Answer yes and add DEST to PATH in shell startup files
#   --yes              Answer yes to every prompt
#   --uninstall        Remove what this installer installed
#   --quiet | --no-gum | --force | --offline TARBALL | --verify | --no-verify

# This is a bash script and must stay parseable by macOS /bin/bash 3.2.
# Piped into another shell it would fail later with an unrelated error, so
# stop here. Keep this block POSIX sh: it runs before any bash syntax.
if [ -z "${BASH_VERSION:-}" ]; then
  printf '%s\n' \
    'FrankenRemote installer: this script needs bash. Run it as:' \
    '  curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/franken_remote/main/install.sh | bash' >&2
  exit 1
fi
case ":${SHELLOPTS:-}:" in
  *:posix:*) set +o posix ;;
esac

set -euo pipefail
umask 022
shopt -s lastpipe 2>/dev/null || true

# ═══════════════════════════════════════════════════════════════════════════════
# Configuration
# ═══════════════════════════════════════════════════════════════════════════════

OWNER="${FR_INSTALL_OWNER:-Dicklesworthstone}"
REPO="${FR_INSTALL_REPO:-franken_remote}"
PRODUCT="FrankenRemote"
REPO_URL="https://github.com/${OWNER}/${REPO}"
RAW_BASE="https://raw.githubusercontent.com/${OWNER}/${REPO}"

VERSION="${VERSION:-}"
REF=""
DEST="${FR_INSTALL_DEST:-$HOME/.local/bin}"
FROM_SOURCE=0
CLIENT_ONLY=0
HOST=0
HOST_CONTROL=1
HOST_CLIPBOARD=1
AUDIO=0
EASY=0
YES=0
NO_DEPS=0
NO_TAILSCALE=0
NO_CONFIGURE=0
OFFLINE_TARBALL=""
ARTIFACT_URL=""
CHECKSUM=""
NO_VERIFY=0
VERIFY=0
FORCE=0
UNINSTALL=0
QUIET=0
NO_GUM=0
KEEP_CACHE=0
SIGNING_IDENTITY="${FR_SIGNING_IDENTITY:-}"
# The release workflow signs from a v* tag push, or from a manual run on main
# (a draft published afterwards); only maintainers can start either.
COSIGN_IDENTITY_RE="^https://github.com/${OWNER}/${REPO}/.github/workflows/release.yml@refs/(tags/v.*|heads/main)$"
COSIGN_OIDC_ISSUER="https://token.actions.githubusercontent.com"

# Build sizes: the release profile (fat LTO) needs room for its target dir.
MIN_SOURCE_BUILD_KB=$((12 * 1024 * 1024))
MIN_BINARY_KB=$((300 * 1024))

HAS_GUM=0
if command -v gum >/dev/null 2>&1 && [ -t 1 ]; then
  HAS_GUM=1
fi

# Under `curl … | bash`, stdin is the script pipe, but a person at a terminal
# is still reachable through /dev/tty. Headless runs take the defaults.
FR_TTY=""
if ( : </dev/tty >/dev/tty ) 2>/dev/null; then
  FR_TTY="/dev/tty"
fi
have_tty() { [ -n "$FR_TTY" ]; }
PROMPT_TIMEOUT="${FR_INSTALL_PROMPT_TIMEOUT:-120}"

# ═══════════════════════════════════════════════════════════════════════════════
# Output
# ═══════════════════════════════════════════════════════════════════════════════

use_gum() { [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; }

info() {
  [ "$QUIET" -eq 1 ] && return 0
  if use_gum; then gum style --foreground 39 "→ $*"; else echo -e "\033[0;34m→\033[0m $*"; fi
}
ok() {
  [ "$QUIET" -eq 1 ] && return 0
  if use_gum; then gum style --foreground 42 "✓ $*"; else echo -e "\033[0;32m✓\033[0m $*"; fi
}
warn() {
  [ "$QUIET" -eq 1 ] && return 0
  if use_gum; then gum style --foreground 214 "⚠ $*"; else echo -e "\033[1;33m⚠\033[0m $*"; fi
}
err() {
  if use_gum; then gum style --foreground 196 "✗ $*" >&2; else echo -e "\033[0;31m✗\033[0m $*" >&2; fi
}
die() { err "$*"; exit 1; }

run_with_spinner() {
  local title="$1"
  shift
  if use_gum && [ "$QUIET" -eq 0 ]; then
    gum spin --spinner dot --title "$title" -- "$@"
  else
    info "$title"
    "$@"
  fi
}

# run_logged "title" cmd... : a spinner (or one line) while cmd's output goes
# to a log; the log's tail is shown only when it fails.
run_logged() {
  local title="$1" log status=0
  shift
  log="${TMP:-/tmp}/step-$$.log"
  if use_gum && [ "$QUIET" -eq 0 ]; then
    gum spin --spinner dot --title "$title" -- "$@" >"$log" 2>&1 || status=$?
  else
    info "$title"
    "$@" >"$log" 2>&1 || status=$?
  fi
  if [ "$status" -ne 0 ]; then
    err "$title failed (exit $status); last lines of its output:"
    tail -n 30 "$log" >&2
  fi
  rm -f "$log"
  return "$status"
}

# draw_box "color" "line" ... : a double-line box sized to the widest line.
draw_box() {
  local color="$1"
  shift
  local esc
  esc=$(printf '\033')
  local strip="s/${esc}\\[[0-9;]*m//g"
  local max=0 line stripped len i border pad
  for line in "$@"; do
    stripped=$(printf '%b' "$line" | LC_ALL=C sed "$strip")
    len=${#stripped}
    [ "$len" -gt "$max" ] && max=$len
  done
  border=""
  for ((i = 0; i < max + 4; i++)); do border+="═"; done
  printf "\033[%sm╔%s╗\033[0m\n" "$color" "$border"
  for line in "$@"; do
    stripped=$(printf '%b' "$line" | LC_ALL=C sed "$strip")
    len=${#stripped}
    pad=""
    for ((i = 0; i < max - len; i++)); do pad+=" "; done
    printf "\033[%sm║\033[0m  %b%s  \033[%sm║\033[0m\n" "$color" "$line" "$pad" "$color"
  done
  printf "\033[%sm╚%s╝\033[0m\n" "$color" "$border"
}

show_header() {
  [ "$QUIET" -eq 1 ] && return 0
  if use_gum; then
    gum style --border double --border-foreground 39 --padding "1 3" --margin "1 0" --align center \
      "$(gum style --foreground 42 --bold "$PRODUCT installer")" \
      "" \
      "$(gum style --foreground 245 'Remote desktop over your tailnet')" \
      "$(gum style --foreground 245 'Tailscale identity · hardware HEVC · no accounts')"
  else
    echo ""
    echo -e "\033[1;36m"
    cat <<'BANNER'
   ███████╗██████╗  █████╗ ███╗   ██╗██╗  ██╗███████╗███╗   ██╗
   ██╔════╝██╔══██╗██╔══██╗████╗  ██║██║ ██╔╝██╔════╝████╗  ██║
   █████╗  ██████╔╝███████║██╔██╗ ██║█████╔╝ █████╗  ██╔██╗ ██║
   ██╔══╝  ██╔══██╗██╔══██║██║╚██╗██║██╔═██╗ ██╔══╝  ██║╚██╗██║
   ██║     ██║  ██║██║  ██║██║ ╚████║██║  ██╗███████╗██║ ╚████║
   ╚═╝     ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═══╝╚═╝  ╚═╝╚══════╝╚═╝  ╚═══╝
BANNER
    echo -e "\033[0m"
    draw_box "1;36" "\033[1;32m$PRODUCT installer\033[0m" \
      "Remote desktop over your tailnet" \
      "Tailscale identity · hardware HEVC · no accounts"
    echo ""
  fi
}

# ask_yn "question" y|n : 0 for yes. Prompts on /dev/tty (works under
# curl | bash). --yes/--easy-mode answer yes; without a terminal the default
# answers.
ask_yn() {
  local prompt="$1" default="${2:-n}" reply="" status=0 hint="[y/N]"
  if [ "$YES" -eq 1 ] || [ "$EASY" -eq 1 ]; then return 0; fi
  [ "$default" = "y" ] && hint="[Y/n]"
  if have_tty; then
    printf '%s %s ' "$prompt" "$hint" >"$FR_TTY" 2>/dev/null || true
    IFS= read -r -t "$PROMPT_TIMEOUT" reply <"$FR_TTY" 2>/dev/null || status=$?
    if [ "$status" -ne 0 ]; then
      reply=""
      printf '\n' >"$FR_TTY" 2>/dev/null || true
      warn "No reply; taking the default ($default)."
    fi
  fi
  [ -n "$reply" ] || reply="$default"
  case "$reply" in
    y|Y|yes|Yes|YES) return 0 ;;
    *) return 1 ;;
  esac
}

usage() {
  cat <<EOFU
Usage: install.sh [options]

Installs the $PRODUCT client (fr) and, unless --client-only, the host daemon
(frd) and its helpers for the current user. Nothing outside your home
directory changes unless you agree to install system packages, Tailscale, or
(Linux hosts) the root ingress helper.

Selection:
  --version vX.Y.Z    Install this release (default: the latest release)
  --ref REF           Build this branch, tag or commit from source (default: main)
  --from-source       Build from source even when a release artifact exists
  --client-only       Install only the viewer (fr and its helpers)
  --audio             Offer host playback audio on --host (macOS source builds
                      need Homebrew's opus)

Host setup:
  --host              Register this machine as a host: a per-user LaunchAgent on
                      macOS, a systemd user service on Linux
  --view-only         The host never accepts remote control
  --no-clipboard      The host never shares the text clipboard
  --signing-identity "Developer ID Application: …"
                      macOS: sign with this identity (default: the first
                      Developer ID Application identity, else ad-hoc;
                      "-" forces an ad-hoc signature)

Placement:
  --dest DIR          Directory for the fr and frd commands (default: ~/.local/bin)
  --easy-mode         Answer yes to prompts and add DEST to PATH in shell rc files
  --yes, -y           Answer yes to every prompt
  --no-deps           Never install system packages, Rust or Tailscale
  --no-tailscale      Do not offer to install Tailscale
  --no-configure      Skip PATH edits, shell completions and the agent skill

Verification:
  --offline TARBALL   Install from a local release tarball (no network)
  --artifact-url URL  Download this release tarball instead of the default
  --checksum HEX      Expected SHA-256 of the tarball
  --no-verify         Skip checksum and signature checks (testing only)
  --verify            Run fr doctor after installing

Maintenance:
  --force             Reinstall even when this version is already installed
  --uninstall         Remove everything this installer installed
  --keep-cache        With --uninstall, keep the source build cache
  --quiet, -q         Print errors only
  --no-gum            Plain ANSI output even when gum is installed
  -h, --help          Show this help

Environment:
  FR_INSTALL_DEST, FR_INSTALL_ROOT (program folder), FR_INSTALL_CACHE (source
  build cache), FR_SIGNING_IDENTITY, HTTPS_PROXY / HTTP_PROXY / NO_PROXY
EOFU
}

require_value() {
  # A lone "-" is a value (--signing-identity - means ad-hoc).
  if [ -z "${2:-}" ] || { [[ "${2:-}" == -* ]] && [ "${2:-}" != "-" ]; }; then
    err "$1 requires a value"
    usage >&2
    exit 2
  fi
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) require_value "$1" "${2:-}"; VERSION="$2"; shift 2 ;;
    --ref) require_value "$1" "${2:-}"; REF="$2"; FROM_SOURCE=1; shift 2 ;;
    --from-source) FROM_SOURCE=1; shift ;;
    --client-only) CLIENT_ONLY=1; shift ;;
    --host) HOST=1; shift ;;
    --view-only) HOST_CONTROL=0; shift ;;
    --no-clipboard) HOST_CLIPBOARD=0; shift ;;
    --audio) AUDIO=1; shift ;;
    --signing-identity) require_value "$1" "${2:-}"; SIGNING_IDENTITY="$2"; shift 2 ;;
    --dest) require_value "$1" "${2:-}"; DEST="$2"; shift 2 ;;
    --easy-mode) EASY=1; shift ;;
    --yes|-y) YES=1; shift ;;
    --no-deps) NO_DEPS=1; shift ;;
    --no-tailscale) NO_TAILSCALE=1; shift ;;
    --no-configure) NO_CONFIGURE=1; shift ;;
    --offline) require_value "$1" "${2:-}"; OFFLINE_TARBALL="$2"; shift 2 ;;
    --artifact-url) require_value "$1" "${2:-}"; ARTIFACT_URL="$2"; shift 2 ;;
    --checksum) require_value "$1" "${2:-}"; CHECKSUM="$2"; shift 2 ;;
    --no-verify) NO_VERIFY=1; shift ;;
    --verify) VERIFY=1; shift ;;
    --force) FORCE=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --keep-cache) KEEP_CACHE=1; shift ;;
    --quiet|-q) QUIET=1; shift ;;
    --no-gum) NO_GUM=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) err "Unknown option: $1"; usage >&2; exit 2 ;;
  esac
done

if [ "$CLIENT_ONLY" -eq 1 ] && [ "$HOST" -eq 1 ]; then
  err "--client-only and --host are mutually exclusive"
  exit 2
fi
if [ -n "$OFFLINE_TARBALL" ] && [ "$FROM_SOURCE" -eq 1 ]; then
  err "--offline installs a release tarball and cannot be combined with --from-source or --ref"
  exit 2
fi
case "$DEST" in
  /*) ;;
  *) DEST="$PWD/$DEST" ;;
esac

# ═══════════════════════════════════════════════════════════════════════════════
# Platform
# ═══════════════════════════════════════════════════════════════════════════════

OS=""
ARCH=""
TARGET=""
IS_WSL=0
PKG=""

detect_platform() {
  OS=$(uname -s | tr '[:upper:]' '[:lower:]')
  ARCH=$(uname -m)
  case "$ARCH" in
    x86_64|amd64) ARCH="x86_64" ;;
    arm64|aarch64) ARCH="aarch64" ;;
    *) die "Unsupported CPU architecture: $ARCH" ;;
  esac
  case "$OS" in
    darwin)
      # FrankenRemote's Linux binaries link the distribution's X11, PipeWire,
      # FFmpeg and Opus libraries, so they are glibc builds (not static musl).
      TARGET="${ARCH}-apple-darwin"
      ;;
    linux)
      TARGET="${ARCH}-unknown-linux-gnu"
      if grep -qi microsoft /proc/version 2>/dev/null; then
        IS_WSL=1
      fi
      ;;
    *) die "Unsupported operating system: $OS (Windows: use install.ps1)" ;;
  esac
}

detect_package_manager() {
  PKG=""
  if [ "$OS" = "darwin" ]; then
    command -v brew >/dev/null 2>&1 && PKG="brew"
    return 0
  fi
  if command -v apt-get >/dev/null 2>&1; then PKG="apt"
  elif command -v dnf >/dev/null 2>&1; then PKG="dnf"
  elif command -v pacman >/dev/null 2>&1; then PKG="pacman"
  elif command -v zypper >/dev/null 2>&1; then PKG="zypper"
  fi
}

# Program folder, command directory, cache and manifest.
set_paths() {
  if [ "$OS" = "darwin" ]; then
    APP_DIR="${FR_INSTALL_ROOT:-$HOME/Library/Application Support/FrankenRemote}"
    CACHE_DIR="${FR_INSTALL_CACHE:-$HOME/Library/Caches/FrankenRemote}"
    COMPLETION_BASH="${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion/completions/fr"
  else
    APP_DIR="${FR_INSTALL_ROOT:-${XDG_DATA_HOME:-$HOME/.local/share}/franken_remote}"
    CACHE_DIR="${FR_INSTALL_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/franken_remote}"
    COMPLETION_BASH="${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion/completions/fr"
  fi
  BIN_DIR="$APP_DIR/bin"
  MANIFEST="$APP_DIR/install-manifest"
  SRC_DIR="$CACHE_DIR/src"
  BUILD_TARGET_DIR="$CACHE_DIR/target"
  COMPLETION_ZSH="${XDG_DATA_HOME:-$HOME/.local/share}/zsh/site-functions/_fr"
  COMPLETION_FISH="${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions/fr.fish"
  LAUNCH_AGENT="$HOME/Library/LaunchAgents/com.frankenremote.frd.plist"
}

# The binaries each install mode carries, in the order they are installed.
select_binaries() {
  BINARIES=()
  if [ "$OS" = "darwin" ]; then
    if [ "$CLIENT_ONLY" -eq 1 ]; then
      BINARIES=(fr)
    else
      BINARIES=(frd fr-media-worker fr-input-agent fr-session-monitor fr-observation-indicator fr)
    fi
    if [ "$AUDIO" -eq 1 ]; then BINARIES+=(fr-opus-worker); fi
  else
    if [ "$CLIENT_ONLY" -eq 1 ]; then
      BINARIES=(fr fr-media-worker fr-opus-worker)
    else
      BINARIES=(frd fr fr-media-worker fr-input-agent fr-opus-worker fr-session-monitor)
    fi
  fi
}

# ═══════════════════════════════════════════════════════════════════════════════
# Network
# ═══════════════════════════════════════════════════════════════════════════════

PROXY_ARGS=()
setup_proxy() {
  PROXY_ARGS=()
  local proxy="${HTTPS_PROXY:-${https_proxy:-${HTTP_PROXY:-${http_proxy:-}}}}"
  if [ -n "$proxy" ]; then
    PROXY_ARGS=(--proxy "$proxy")
    local bypass="${NO_PROXY:-${no_proxy:-}}"
    if [ -n "$bypass" ]; then PROXY_ARGS+=(--noproxy "$bypass"); fi
    info "Using proxy $proxy"
  fi
}

# fetch URL FILE : HTTPS only, retried, through the configured proxy.
fetch() {
  curl -fsSL --proto '=https' --tlsv1.2 --retry 3 --retry-delay 2 --connect-timeout 15 \
    ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} "$1" -o "$2"
}
fetch_stdout() {
  curl -fsSL --proto '=https' --tlsv1.2 --retry 2 --connect-timeout 15 \
    ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} "$@"
}

normalize_version() {
  local v="$1"
  case "$v" in v*) ;; *) v="v$v" ;; esac
  if [[ "$v" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]]; then
    printf '%s\n' "$v"
  else
    return 1
  fi
}

# Sets VERSION to the latest release tag, or leaves it empty when the
# repository has published no release yet.
resolve_version() {
  if [ -n "$VERSION" ]; then
    local requested="$VERSION"
    VERSION=$(normalize_version "$requested") || { err "Invalid version '$requested' (expected vX.Y.Z)"; exit 2; }
    return 0
  fi
  if [ -n "$OFFLINE_TARBALL" ] || [ -n "$ARTIFACT_URL" ]; then return 0; fi
  if [ "$FROM_SOURCE" -eq 1 ] && [ -n "$REF" ]; then return 0; fi
  local tag=""
  tag=$(fetch_stdout -H "Accept: application/vnd.github+json" \
    "https://api.github.com/repos/${OWNER}/${REPO}/releases/latest" 2>/dev/null |
    grep -o '"tag_name": *"[^"]*"' | head -n 1 | sed -E 's/.*"([^"]*)"$/\1/' || true)
  if [ -z "$tag" ]; then
    # The API is rate limited; the release page redirects to the tag.
    tag=$(curl -fsSL -o /dev/null -w '%{url_effective}' ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} \
      "${REPO_URL}/releases/latest" 2>/dev/null | sed -nE 's|.*/releases/tag/([^/?#]+).*|\1|p' || true)
  fi
  if [ -n "$tag" ] && VERSION=$(normalize_version "$tag"); then
    info "Latest release: $VERSION"
  else
    VERSION=""
    info "No published release yet; $PRODUCT will be built from source"
    FROM_SOURCE=1
  fi
}

# ═══════════════════════════════════════════════════════════════════════════════
# Preflight
# ═══════════════════════════════════════════════════════════════════════════════

free_kb() {
  local path="$1"
  while [ ! -d "$path" ]; do path=$(dirname "$path"); done
  df -Pk "$path" 2>/dev/null | awk 'NR==2 {print $4}'
}

check_disk_space() {
  local need="$MIN_BINARY_KB" where="$APP_DIR" avail
  if [ "$FROM_SOURCE" -eq 1 ]; then need="$MIN_SOURCE_BUILD_KB"; where="$CACHE_DIR"; fi
  avail=$(free_kb "$where" || true)
  if [ -z "$avail" ]; then
    warn "Could not read free disk space for $where"
    return 0
  fi
  if [ "$avail" -lt "$need" ]; then
    if [ "$FROM_SOURCE" -eq 1 ]; then
      warn "A source build needs about $((need / 1024 / 1024)) GB in $where; $((avail / 1024 / 1024)) GB is free"
      ask_yn "Build anyway?" n || die "Not enough disk space for a source build (set FR_INSTALL_CACHE to another disk)"
    else
      die "Not enough disk space in $where (need $((need / 1024)) MB)"
    fi
  fi
}

check_write_permissions() {
  local dir
  for dir in "$DEST" "$APP_DIR"; do
    mkdir -p "$dir" 2>/dev/null || die "Cannot create $dir"
    [ -w "$dir" ] || die "No write permission to $dir (choose another --dest or FR_INSTALL_ROOT)"
  done
}

check_network() {
  if [ -n "$OFFLINE_TARBALL" ]; then
    info "Offline install; skipping the network check"
    return 0
  fi
  if ! curl -fsS -o /dev/null --connect-timeout 5 --max-time 10 ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} "https://github.com" 2>/dev/null; then
    warn "github.com did not answer; downloads may fail (proxy settings: HTTPS_PROXY)"
  fi
}

manifest_get() {
  [ -f "$MANIFEST" ] || return 1
  sed -n "s/^$1=//p" "$MANIFEST" | head -n 1
}

check_existing_install() {
  local current
  current=$(manifest_get version 2>/dev/null || true)
  if [ -n "$current" ]; then
    info "Existing install: $current ($(manifest_get source 2>/dev/null || echo unknown))"
  fi
}

preflight_checks() {
  info "Running preflight checks"
  check_write_permissions
  check_disk_space
  check_network
  check_existing_install
  if [ "$IS_WSL" -eq 1 ]; then
    warn "WSL detected. For the native Windows app, run install.ps1 from PowerShell."
    warn "Continuing with the Linux X11 client; hosting from WSL is not supported."
    if [ "$HOST" -eq 1 ]; then die "--host is not supported inside WSL"; fi
  fi
  if [ "$(id -u)" -eq 0 ] && [ -z "${FR_INSTALL_ALLOW_ROOT:-}" ]; then
    warn "Running as root installs FrankenRemote for root only."
    warn "Run the installer as your desktop user; it asks for sudo when it needs it."
    ask_yn "Install for root anyway?" n || exit 1
  fi
  ok "Preflight checks passed"
}

# Cross-platform mutual exclusion: mkdir is atomic everywhere.
LOCK_DIR="/tmp/franken_remote-install-$(id -u).lock.d"
LOCKED=0
TMP=""
acquire_lock() {
  if mkdir "$LOCK_DIR" 2>/dev/null; then
    LOCKED=1
  else
    local old=""
    old=$(cat "$LOCK_DIR/pid" 2>/dev/null || true)
    if [ -n "$old" ] && ! kill -0 "$old" 2>/dev/null; then
      rm -rf "$LOCK_DIR"
      mkdir "$LOCK_DIR" 2>/dev/null && LOCKED=1
    fi
  fi
  [ "$LOCKED" -eq 1 ] || die "Another FrankenRemote installer is running (lock $LOCK_DIR)"
  echo $$ >"$LOCK_DIR/pid"
}

cleanup() {
  if [ -n "$TMP" ] && [ -d "$TMP" ]; then rm -rf "$TMP"; fi
  if [ "$LOCKED" -eq 1 ]; then rm -rf "$LOCK_DIR"; fi
}

# ═══════════════════════════════════════════════════════════════════════════════
# Privileged helpers and packages
# ═══════════════════════════════════════════════════════════════════════════════

as_root() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    err "This step needs root and sudo is not installed: $*"
    return 1
  fi
}

# Package names per manager. Ubuntu/Debian names follow the repository's CI
# lane; the dnf, pacman and zypper mappings are best effort.
linux_packages() {
  local kind="$1"
  case "$PKG:$kind" in
    apt:build) echo "build-essential pkg-config git curl ca-certificates python3 libpipewire-0.3-dev libavcodec-dev libavutil-dev libswscale-dev libx11-dev libxcb-render0-dev libxcb1-dev libx11-xcb1 libxtst6 libxi6 libxdamage1 libxrandr2 libxfixes3 libxcursor1 libopus0 libpulse0" ;;
    apt:runtime) echo "ca-certificates libavcodec-dev libavutil-dev libswscale-dev libpipewire-0.3-0t64 libx11-6 libx11-xcb1 libxcb1 libxcb-render0 libxtst6 libxi6 libxdamage1 libxrandr2 libxfixes3 libxcursor1 libopus0 libpulse0" ;;
    apt:host) echo "nftables xvfb xauth python3" ;;
    dnf:build) echo "gcc gcc-c++ make pkgconf-pkg-config git curl python3 pipewire-devel libavcodec-free-devel libavutil-free-devel libswscale-free-devel libX11-devel libxcb-devel libXtst libXi libXdamage libXrandr libXfixes libXcursor opus pulseaudio-libs" ;;
    dnf:runtime) echo "libavcodec-free libavutil-free libswscale-free pipewire-libs libX11 libX11-xcb libxcb libXtst libXi libXdamage libXrandr libXfixes libXcursor opus pulseaudio-libs" ;;
    dnf:host) echo "nftables xorg-x11-server-Xvfb xorg-x11-xauth python3" ;;
    pacman:build) echo "base-devel pkgconf git curl python pipewire ffmpeg libx11 libxcb libxtst libxi libxdamage libxrandr libxfixes libxcursor opus libpulse" ;;
    pacman:runtime) echo "pipewire ffmpeg libx11 libxcb libxtst libxi libxdamage libxrandr libxfixes libxcursor opus libpulse" ;;
    pacman:host) echo "nftables xorg-server-xvfb xorg-xauth python" ;;
    zypper:build) echo "gcc gcc-c++ make pkg-config git curl python3 pipewire-devel ffmpeg-devel libX11-devel libxcb-devel libXtst6 libXi6 libXdamage1 libXrandr2 libXfixes3 libXcursor1 libopus0 libpulse0" ;;
    zypper:runtime) echo "pipewire libX11-6 libX11-xcb1 libxcb1 libXtst6 libXi6 libXdamage1 libXrandr2 libXfixes3 libXcursor1 libopus0 libpulse0" ;;
    zypper:host) echo "nftables xorg-x11-server-Xvfb xauth python3" ;;
    *) echo "" ;;
  esac
}

# Prints the packages of a list that are not installed yet.
missing_packages() {
  local pkg missing=""
  for pkg in "$@"; do
    case "$PKG" in
      apt) dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null | grep -q "install ok installed" || missing+="$pkg " ;;
      dnf|zypper) rpm -q "$pkg" >/dev/null 2>&1 || missing+="$pkg " ;;
      pacman) pacman -Q "$pkg" >/dev/null 2>&1 || missing+="$pkg " ;;
      *) missing+="$pkg " ;;
    esac
  done
  printf '%s' "$missing"
}

install_linux_packages() {
  local kind="$1" list missing
  list=$(linux_packages "$kind")
  if [ -z "$list" ]; then
    warn "No package list for this distribution; install the $kind dependencies listed in the README yourself"
    return 1
  fi
  # shellcheck disable=SC2086
  missing=$(missing_packages $list)
  if [ -z "$missing" ]; then
    ok "System packages ($kind) are installed"
    return 0
  fi
  info "Missing $kind packages: $missing"
  if [ "$NO_DEPS" -eq 1 ]; then
    err "--no-deps: install them yourself, then run the installer again"
    return 1
  fi
  ask_yn "Install them now with $PKG (needs sudo)?" y || return 1
  # Word splitting of the package list is intended.
  # shellcheck disable=SC2086
  case "$PKG" in
    apt) run_logged "Installing $kind packages" as_root env DEBIAN_FRONTEND=noninteractive sh -c "apt-get update -qq && apt-get install -y -qq $missing" ;;
    dnf) run_logged "Installing $kind packages" as_root dnf install -y $missing ;;
    pacman) run_logged "Installing $kind packages" as_root pacman -S --needed --noconfirm $missing ;;
    zypper) run_logged "Installing $kind packages" as_root zypper --non-interactive install $missing ;;
  esac
}

# ═══════════════════════════════════════════════════════════════════════════════
# Tailscale
# ═══════════════════════════════════════════════════════════════════════════════

TAILSCALE_STATE="missing"

tailscale_cli() {
  if command -v tailscale >/dev/null 2>&1; then
    command -v tailscale
  elif [ -x "/Applications/Tailscale.app/Contents/MacOS/Tailscale" ]; then
    echo "/Applications/Tailscale.app/Contents/MacOS/Tailscale"
  else
    return 1
  fi
}

check_tailscale() {
  local cli state=""
  if cli=$(tailscale_cli); then
    state=$("$cli" status --json 2>/dev/null | grep -o '"BackendState": *"[^"]*"' | head -n 1 | sed -E 's/.*"([^"]*)"$/\1/' || true)
    case "$state" in
      Running) TAILSCALE_STATE="running"; ok "Tailscale is running" ;;
      NeedsLogin|NoState|Stopped|Starting) TAILSCALE_STATE="installed"; warn "Tailscale is installed but not connected ($state)" ;;
      *) TAILSCALE_STATE="installed"; warn "Tailscale is installed; its state could not be read" ;;
    esac
    return 0
  fi
  if [ "$OS" = "darwin" ] && [ -d "/Applications/Tailscale.app" ]; then
    TAILSCALE_STATE="installed"
    warn "Tailscale.app is installed; open it and sign in"
    return 0
  fi
  TAILSCALE_STATE="missing"
  warn "Tailscale is not installed. $PRODUCT uses it for every connection and identity."
  if [ "$NO_DEPS" -eq 1 ] || [ "$NO_TAILSCALE" -eq 1 ]; then return 0; fi
  if [ "$OS" = "darwin" ]; then
    if [ "$PKG" = "brew" ] && ask_yn "Install Tailscale with Homebrew (brew install --cask tailscale)?" y; then
      run_with_spinner "Installing Tailscale" brew install --cask tailscale && TAILSCALE_STATE="installed"
      open -a Tailscale 2>/dev/null || true
    else
      info "Install Tailscale from https://tailscale.com/download/mac, then sign in"
    fi
  else
    if ask_yn "Install Tailscale with its official installer (needs sudo)?" y; then
      fetch "https://tailscale.com/install.sh" "$TMP/tailscale-install.sh" &&
        sh "$TMP/tailscale-install.sh" && TAILSCALE_STATE="installed"
    else
      info "Install Tailscale from https://tailscale.com/download/linux"
    fi
  fi
  if [ "$TAILSCALE_STATE" = "installed" ] && [ "$OS" = "linux" ]; then
    info "Connect this machine with: sudo tailscale up"
  fi
}

# ═══════════════════════════════════════════════════════════════════════════════
# Verification
# ═══════════════════════════════════════════════════════════════════════════════

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    return 1
  fi
}

verify_checksum() {
  local file="$1" expected="$2" actual
  actual=$(sha256_of "$file") || { err "No SHA-256 tool (sha256sum or shasum) found"; return 1; }
  if [ "$actual" != "$expected" ]; then
    err "Checksum verification FAILED for $(basename "$file")"
    err "Expected: $expected"
    err "Got:      $actual"
    # Never delete it here: an --offline tarball is the user's own file, and
    # downloads live in the temporary directory removed on exit.
    return 1
  fi
  ok "Checksum verified: ${actual:0:16}…"
}

cosign_version_ok() {
  local major="$1" minor="$2" patch="$3"
  [ "$major" -ge 4 ] && return 0
  [ "$major" -eq 3 ] && { [ "$minor" -gt 0 ] || [ "$patch" -ge 4 ]; } && return 0
  [ "$major" -eq 2 ] && { [ "$minor" -gt 6 ] || { [ "$minor" -eq 6 ] && [ "$patch" -ge 2 ]; }; } && return 0
  return 1
}

# Best effort: cosign absent skips; cosign present and a bad bundle fails.
verify_sigstore() {
  local file="$1" bundle_url="$2" cosign_bin
  cosign_bin=$(command -v cosign 2>/dev/null || true)
  if [ -z "$cosign_bin" ]; then
    info "cosign not found; skipping the Sigstore signature check (the checksum was verified)"
    return 0
  fi
  if ! fetch "$bundle_url" "$TMP/bundle.sigstore.json" 2>/dev/null; then
    warn "No Sigstore bundle published for this artifact; skipping the signature check"
    return 0
  fi
  # Releases before 2.6.2 / 3.0.4 lack the bundle verification fixes for
  # CVE-2026-22703, so their verdict is not worth trusting either way.
  local version major minor patch format_args=()
  version=$("$cosign_bin" version 2>&1 | sed -nE 's/.*GitVersion:[[:space:]]*v?([0-9]+)\.([0-9]+)\.([0-9]+).*/\1 \2 \3/p' | head -n 1 || true)
  read -r major minor patch <<<"${version:-0 0 0}"
  if ! cosign_version_ok "$major" "$minor" "$patch"; then
    warn "cosign ${major}.${minor}.${patch} predates the 2.6.2 / 3.0.4 bundle fixes; skipping the signature check"
    return 0
  fi
  if "$cosign_bin" verify-blob --help 2>&1 | grep -q -- '--new-bundle-format'; then
    format_args=(--new-bundle-format)
  fi
  if "$cosign_bin" verify-blob ${format_args[@]+"${format_args[@]}"} --bundle "$TMP/bundle.sigstore.json" \
    --certificate-identity-regexp "$COSIGN_IDENTITY_RE" \
    --certificate-oidc-issuer "$COSIGN_OIDC_ISSUER" "$file" >/dev/null 2>&1; then
    ok "Signature verified (Sigstore, ${OWNER}/${REPO} release workflow)"
  else
    err "The Sigstore bundle does not verify against the release workflow's identity"
    return 1
  fi
}

# ═══════════════════════════════════════════════════════════════════════════════
# Release artifacts
# ═══════════════════════════════════════════════════════════════════════════════

ARCHIVE=""
UNPACK_COUNT=0
PAYLOAD_DIR=""
INSTALLED_SOURCE=""
INSTALLED_VERSION=""
INSTALLED_COMMIT=""

# Release artifact names for this machine, most likely first. Linux builds
# link the distribution's FFmpeg and PipeWire, so there is one per Ubuntu
# release; ldd decides which (if any) runs here.
artifact_candidates() {
  local base="franken_remote-${VERSION}-${TARGET}" id="" version_id="" seen=" " variant
  local os_release="${FR_INSTALL_OS_RELEASE:-/etc/os-release}"
  if [ "$OS" = "linux" ]; then
    if [ -r "$os_release" ]; then
      # shellcheck source=/dev/null
      id=$(. "$os_release" && printf '%s' "${ID:-}")
      # shellcheck source=/dev/null
      version_id=$(. "$os_release" && printf '%s' "${VERSION_ID:-}")
    fi
    for variant in "${id}${version_id}" ubuntu26.04 ubuntu24.04; do
      case "$variant" in ubuntu[0-9]*) ;; *) continue ;; esac
      case "$seen" in *" $variant "*) continue ;; esac
      seen+="$variant "
      printf '%s-%s.tar.gz\n' "$base" "$variant"
    done
  fi
  printf '%s.tar.gz\n' "$base"
}

SUMS_FETCHED=0
# expected_checksum NAME BASE_URL : from --checksum, else the release's SHA256SUMS.
expected_checksum() {
  local name="$1" base_url="$2"
  if [ -n "$CHECKSUM" ]; then printf '%s' "$CHECKSUM"; return 0; fi
  if [ "$SUMS_FETCHED" -eq 0 ]; then
    SUMS_FETCHED=1
    fetch "$base_url/SHA256SUMS" "$TMP/SHA256SUMS" 2>/dev/null || : >"$TMP/SHA256SUMS"
  fi
  awk -v want="$name" '$2 == want || $2 == "*" want {print $1; exit}' "$TMP/SHA256SUMS"
}

# Download, verify, unpack and library-check one artifact. Returns non-zero
# when it is absent or cannot run here; a checksum or signature failure stops.
try_release_artifact() {
  local url="$1" name="$2" expected
  ARCHIVE="$TMP/$name"
  if ! fetch "$url" "$ARCHIVE" 2>/dev/null; then
    rm -f "$ARCHIVE"
    return 1
  fi
  info "Downloaded $name"
  if [ "$NO_VERIFY" -eq 1 ]; then
    warn "Verification skipped (--no-verify)"
  else
    expected=$(expected_checksum "$name" "${url%/*}")
    [ -n "$expected" ] || die "The release publishes no SHA-256 for $name (use --checksum, or --no-verify for testing)"
    verify_checksum "$ARCHIVE" "$expected" || die "Installation aborted: checksum mismatch"
    verify_sigstore "$ARCHIVE" "${url}.sigstore.json" || die "Installation aborted: signature verification failed"
  fi
  unpack_release "$ARCHIVE" || return 1
  release_libraries_resolve || return 1
  return 0
}

# Installs from the offline tarball, --artifact-url or the release, leaving
# PAYLOAD_DIR set. Returns non-zero when no artifact fits, so the caller builds.
acquire_release() {
  local name manifest candidate
  if [ -n "$OFFLINE_TARBALL" ]; then
    [ -f "$OFFLINE_TARBALL" ] || die "Offline tarball not found: $OFFLINE_TARBALL"
    ARCHIVE="$OFFLINE_TARBALL"
    name=$(basename "$ARCHIVE")
    if [ -z "$CHECKSUM" ]; then
      for manifest in "$(dirname "$ARCHIVE")/SHA256SUMS" "$(dirname "$ARCHIVE")/SHA256SUMS.txt"; do
        if [ -f "$manifest" ]; then
          CHECKSUM=$(awk -v want="$name" '$2 == want || $2 == "*" want {print $1; exit}' "$manifest")
          [ -n "$CHECKSUM" ] && break
        fi
      done
    fi
    if [ "$NO_VERIFY" -eq 0 ]; then
      if [ -n "$CHECKSUM" ]; then
        verify_checksum "$ARCHIVE" "$CHECKSUM" || die "Installation aborted: checksum mismatch"
      else
        warn "No checksum for the offline tarball (pass --checksum or keep SHA256SUMS beside it)"
        ask_yn "Install the unverified tarball?" n || die "Aborted: unverified offline tarball"
      fi
    fi
    unpack_release "$ARCHIVE" && release_libraries_resolve
    return
  fi
  if [ -n "$ARTIFACT_URL" ]; then
    name=$(basename "${ARTIFACT_URL%%\?*}")
    try_release_artifact "$ARTIFACT_URL" "$name" && return 0
    warn "The artifact at $ARTIFACT_URL cannot be installed here"
    return 1
  fi
  [ -n "$VERSION" ] || return 1
  while IFS= read -r candidate; do
    if try_release_artifact "${REPO_URL}/releases/download/${VERSION}/${candidate}" "$candidate"; then
      return 0
    fi
  done < <(artifact_candidates)
  warn "No $VERSION release artifact runs on this $OS/$ARCH system"
  return 1
}

# Accept only plain relative members under one top-level folder.
unpack_release() {
  local archive="$1" members top bad
  members=$(tar -tzf "$archive") || die "Cannot read the release archive"
  top=$(printf '%s\n' "$members" | head -n 1 | cut -d/ -f1)
  [ -n "$top" ] || die "The release archive is empty"
  bad=$(printf '%s\n' "$members" | awk -v top="$top" '
    $0 ~ /^\// || $0 ~ /(^|\/)\.\.(\/|$)/ || (index($0, top "/") != 1 && $0 != top) { print; exit }')
  [ -z "$bad" ] || die "Unsafe path in the release archive: $bad"
  if tar -tvzf "$archive" | cut -c1 | grep -qv '^[-d]'; then
    die "The release archive contains links or special files"
  fi
  UNPACK_COUNT=$((UNPACK_COUNT + 1))
  local into="$TMP/unpacked.$UNPACK_COUNT"
  mkdir -p "$into"
  tar -xzf "$archive" -C "$into"
  PAYLOAD_DIR="$into/$top/bin"
  [ -d "$PAYLOAD_DIR" ] || die "The release archive has no bin/ folder"
  if [ "$OS" = "darwin" ]; then
    xattr -dr com.apple.quarantine "$into" 2>/dev/null || true
  fi
  local name
  for name in "${BINARIES[@]}"; do
    [ -f "$PAYLOAD_DIR/$name" ] || { warn "The release lacks $name for this install mode"; return 1; }
  done
  # The macOS release carries the client's Opus decoder for `fr connect --audio`.
  if [ "$OS" = "darwin" ] && [ -f "$PAYLOAD_DIR/fr-opus-worker" ]; then
    case " ${BINARIES[*]} " in *" fr-opus-worker "*) ;; *) BINARIES+=(fr-opus-worker) ;; esac
  fi
  INSTALLED_SOURCE="release"
  INSTALLED_VERSION="${VERSION:-$(basename "$archive" .tar.gz)}"
  INSTALLED_COMMIT=$(cat "$into/$top/COMMIT" 2>/dev/null || true)
  ok "Release unpacked"
}

# Linux release binaries link distribution libraries; check they resolve
# here and install the runtime packages when they do not.
release_libraries_resolve() {
  [ "$OS" = "linux" ] || return 0
  command -v ldd >/dev/null 2>&1 || return 0
  local missing name
  missing=""
  for name in "${BINARIES[@]}"; do
    missing+=$(ldd "$PAYLOAD_DIR/$name" 2>/dev/null | awk '/not found/ {printf "%s ", $1}')
  done
  [ -z "$missing" ] && return 0
  warn "Libraries missing for the release binaries: $missing"
  if [ -n "$PKG" ] && install_linux_packages runtime; then
    missing=""
    for name in "${BINARIES[@]}"; do
      missing+=$(ldd "$PAYLOAD_DIR/$name" 2>/dev/null | awk '/not found/ {printf "%s ", $1}')
    done
    [ -z "$missing" ] && return 0
    warn "Still missing: $missing (this distribution ships different library versions)"
  fi
  return 1
}

# ═══════════════════════════════════════════════════════════════════════════════
# Source build
# ═══════════════════════════════════════════════════════════════════════════════

ensure_build_prerequisites() {
  if [ "$OS" = "darwin" ]; then
    if ! xcode-select -p >/dev/null 2>&1; then
      warn "The Xcode Command Line Tools are required to build $PRODUCT"
      xcode-select --install 2>/dev/null || true
      die "Finish the Command Line Tools installation that just opened, then run the installer again"
    fi
    if [ "$AUDIO" -eq 1 ] && [ ! -f "$(brew --prefix opus 2>/dev/null)/lib/libopus.a" ]; then
      if [ "$PKG" = "brew" ] && [ "$NO_DEPS" -eq 0 ] && ask_yn "--audio needs Homebrew's opus. Install it?" y; then
        run_with_spinner "Installing opus" brew install opus
      else
        die "--audio needs a static libopus (brew install opus)"
      fi
    fi
  else
    [ -n "$PKG" ] || die "No supported package manager found; install the build packages from the README, then rerun with --no-deps"
    install_linux_packages build || die "Build prerequisites are missing"
    if [ "$PKG" != "apt" ]; then
      warn "The $PKG package list is best effort (CI builds on Ubuntu); report failures upstream"
    fi
  fi
  command -v git >/dev/null 2>&1 || die "git is required to build from source"
  ensure_rustup
}

rustup_triple() {
  case "$OS-$ARCH" in
    linux-x86_64) echo "x86_64-unknown-linux-gnu" ;;
    linux-aarch64) echo "aarch64-unknown-linux-gnu" ;;
    darwin-x86_64) echo "x86_64-apple-darwin" ;;
    darwin-aarch64) echo "aarch64-apple-darwin" ;;
  esac
}

ensure_rustup() {
  local cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
  if command -v rustup >/dev/null 2>&1; then return 0; fi
  if [ -x "$cargo_bin/rustup" ]; then
    export PATH="$cargo_bin:$PATH"
    return 0
  fi
  if [ "$NO_DEPS" -eq 1 ]; then die "rustup is required (https://rustup.rs); --no-deps forbids installing it"; fi
  info "$PRODUCT builds with the Rust toolchain pinned in its rust-toolchain.toml"
  ask_yn "Install rustup into ${CARGO_HOME:-~/.cargo}?" y || die "rustup is required to build from source"
  local triple url
  triple=$(rustup_triple)
  url="https://static.rust-lang.org/rustup/dist/${triple}/rustup-init"
  fetch "$url" "$TMP/rustup-init" || die "Could not download rustup-init"
  fetch "${url}.sha256" "$TMP/rustup-init.sha256" || die "Could not download rustup-init's checksum"
  verify_checksum "$TMP/rustup-init" "$(awk '{print $1; exit}' "$TMP/rustup-init.sha256")" || die "rustup-init failed verification"
  chmod 0755 "$TMP/rustup-init"
  run_with_spinner "Installing rustup" "$TMP/rustup-init" -y --no-modify-path --profile minimal --default-toolchain none
  export PATH="$cargo_bin:$PATH"
  ok "rustup installed"
}

# Default source ref: the release tag when one was chosen, else main.
source_ref() {
  if [ -n "$REF" ]; then echo "$REF"
  elif [ -n "$VERSION" ]; then echo "$VERSION"
  else echo "main"
  fi
}

checkout_source() {
  local ref="$1"
  mkdir -p "$CACHE_DIR"
  if [ -d "$SRC_DIR/.git" ] && [ "$(git -C "$SRC_DIR" remote get-url origin 2>/dev/null || true)" = "${REPO_URL}.git" ]; then
    run_with_spinner "Fetching $ref" git -C "$SRC_DIR" fetch --depth 1 --quiet origin "$ref" ||
      die "Could not fetch $ref from $REPO_URL"
  else
    if [ -e "$SRC_DIR" ]; then
      die "$SRC_DIR exists and is not this installer's checkout; move it or set FR_INSTALL_CACHE"
    fi
    run_with_spinner "Cloning $REPO_URL" git init --quiet "$SRC_DIR"
    git -C "$SRC_DIR" remote add origin "${REPO_URL}.git"
    run_with_spinner "Fetching $ref" git -C "$SRC_DIR" fetch --depth 1 --quiet origin "$ref" ||
      die "Could not fetch $ref from $REPO_URL"
  fi
  git -C "$SRC_DIR" -c advice.detachedHead=false checkout --quiet --force --detach FETCH_HEAD
  INSTALLED_COMMIT=$(git -C "$SRC_DIR" rev-parse HEAD)
  ok "Source at $ref ($(printf '%s' "$INSTALLED_COMMIT" | cut -c1-10))"
}

run_cargo() {
  if [ "$QUIET" -eq 1 ]; then
    (cd "$SRC_DIR" && cargo "$@") >>"$CACHE_DIR/build.log" 2>&1 || {
      err "Build failed; see $CACHE_DIR/build.log"
      return 1
    }
  else
    (cd "$SRC_DIR" && cargo "$@")
  fi
}

build_from_source() {
  local ref
  ref=$(source_ref)
  ensure_build_prerequisites
  checkout_source "$ref"
  # rustup 1.28+ installs the toolchain rust-toolchain.toml names; older
  # versions install it on first use.
  if ! (cd "$SRC_DIR" && run_with_spinner "Installing the pinned Rust toolchain" rustup toolchain install); then
    (cd "$SRC_DIR" && rustup show active-toolchain) || die "Could not install the pinned Rust toolchain"
  fi
  export CARGO_TARGET_DIR="$BUILD_TARGET_DIR"
  # Cargo's built-in git client fetched the asupersync dependency at about
  # 100 KB/s where the git CLI took 11 s; git is already required here.
  export CARGO_NET_GIT_FETCH_WITH_CLI=true
  : >"$CACHE_DIR/build.log"
  info "Building $PRODUCT (release profile; a first build takes a while, later updates reuse $BUILD_TARGET_DIR)"
  if [ "$OS" = "darwin" ]; then
    local features="macos-desktop"
    if [ "$CLIENT_ONLY" -eq 0 ]; then
      features="macos-media,macos-input,macos-desktop,macos-session-monitor"
      [ "$AUDIO" -eq 1 ] && features+=",macos-host-audio"
    fi
    [ "$AUDIO" -eq 1 ] && features+=",macos-audio"
    if [ "$AUDIO" -eq 1 ]; then
      FR_NATIVE_OPUS_LIBRARY_DIR="$(brew --prefix opus)/lib"
      export FR_NATIVE_OPUS_LIBRARY_DIR
    fi
    local bins=(--bin fr)
    if [ "$CLIENT_ONLY" -eq 0 ]; then
      bins+=(--bin fr-media-worker --bin fr-input-agent --bin fr-session-monitor)
      run_cargo build --release --locked -p frd --features macos-host --bin frd
    fi
    [ "$AUDIO" -eq 1 ] && bins+=(--bin fr-opus-worker)
    run_cargo build --release --locked -p fr-native --features "$features" "${bins[@]}"
    if [ "$CLIENT_ONLY" -eq 0 ]; then
      run_cargo build --release --locked -p fr-native --no-default-features \
        --features macos-observation-indicator --bin fr-observation-indicator
    fi
  else
    local features="linux-desktop,linux-displays,linux-input-monitors,linux-input-agent,linux-clipboard,linux-audio,linux-session-monitor"
    local bins=(--bin fr --bin fr-media-worker --bin fr-opus-worker)
    if [ "$CLIENT_ONLY" -eq 0 ]; then
      bins+=(--bin fr-input-agent --bin fr-session-monitor)
      run_cargo build --release --locked -p frd --bin frd
    fi
    run_cargo build --release --locked -p fr-native --features "$features" "${bins[@]}"
  fi
  PAYLOAD_DIR="$BUILD_TARGET_DIR/release"
  INSTALLED_SOURCE="source"
  INSTALLED_VERSION="source:${ref}@$(printf '%s' "$INSTALLED_COMMIT" | cut -c1-10)"
  ok "Build complete"
}

# ═══════════════════════════════════════════════════════════════════════════════
# Installation
# ═══════════════════════════════════════════════════════════════════════════════

SIGNED_WITH=""

choose_signing_identity() {
  [ "$OS" = "darwin" ] || return 0
  if [ -z "$SIGNING_IDENTITY" ]; then
    SIGNING_IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null |
      sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -n 1 || true)
  fi
  if [ -n "$SIGNING_IDENTITY" ] && [ "$SIGNING_IDENTITY" != "-" ]; then
    SIGNED_WITH="$SIGNING_IDENTITY"
  else
    SIGNED_WITH="ad-hoc"
    if [ "$CLIENT_ONLY" -eq 0 ]; then
      warn "No Developer ID signing identity: the binaries get an ad-hoc signature."
      warn "macOS then asks for Screen Recording and Accessibility again after each update."
    fi
  fi
}

sign_binary() {
  local file="$1" name="$2"
  [ "$OS" = "darwin" ] || return 0
  local identity="-" timestamp="--timestamp=none"
  if [ "$SIGNED_WITH" != "ad-hoc" ]; then
    identity="$SIGNING_IDENTITY"
    timestamp="--timestamp"
  fi
  # Both must succeed: --verify alone also passes on the linker's own ad-hoc
  # signature that a failed signing leaves in place.
  codesign --force --options runtime "$timestamp" --identifier "com.frankenremote.$name" \
    --sign "$identity" "$file" >/dev/null 2>&1 &&
    codesign --verify --strict "$file" >/dev/null 2>&1
}

# A POSIX-sh single-quoted literal.
sh_quote() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }

WRAPPER_MARK="# franken_remote installer wrapper"

write_wrapper() {
  local name="$1" target="$DEST/$1" staged
  if [ -e "$target" ] && ! grep -q "$WRAPPER_MARK" "$target" 2>/dev/null; then
    warn "$target exists and was not written by this installer; leaving it (use $BIN_DIR/$name)"
    return 0
  fi
  staged="$DEST/.$name.franken_remote.new"
  {
    echo '#!/bin/sh'
    echo "$WRAPPER_MARK (remove with install.sh --uninstall)"
    echo "exec $(sh_quote "$BIN_DIR/$name") \"\$@\""
  } >"$staged"
  chmod 0755 "$staged"
  mv -f "$staged" "$target"
}

# Stage each binary beside its destination and rename it into place, so a
# running frd keeps its open file and nothing sees a partial copy.
install_payload() {
  local name staged
  mkdir -p "$BIN_DIR"
  chmod 0755 "$APP_DIR" "$BIN_DIR" 2>/dev/null || true
  [ "$OS" = "darwin" ] && chmod 0700 "$BIN_DIR"
  choose_signing_identity
  for name in "${BINARIES[@]}"; do
    [ -f "$PAYLOAD_DIR/$name" ] || die "Missing built binary: $PAYLOAD_DIR/$name"
    staged="$BIN_DIR/.$name.new"
    cp "$PAYLOAD_DIR/$name" "$staged"
    chmod 0755 "$staged"
    if ! sign_binary "$staged" "$name"; then
      # A Developer ID whose keychain is locked (an ssh session, for one)
      # fails with errSecInternalComponent; an ad-hoc signature still runs.
      local failed_identity="$SIGNED_WITH"
      SIGNED_WITH="ad-hoc"
      if [ "$failed_identity" = "ad-hoc" ] || ! sign_binary "$staged" "$name"; then
        rm -f "$staged"
        die "Could not sign $name"
      fi
      warn "Signing with \"$failed_identity\" failed (is the keychain locked? Over ssh, run: security unlock-keychain)"
      warn "Using an ad-hoc signature instead; macOS then asks for its permissions again after each update."
    fi
    if ! "$staged" --help >/dev/null 2>&1 && [ "$name" = "fr" ]; then
      rm -f "$staged"
      die "The new fr binary does not run on this machine; nothing was replaced"
    fi
    mv -f "$staged" "$BIN_DIR/$name"
  done
  write_wrapper fr
  if [ "$CLIENT_ONLY" -eq 0 ]; then write_wrapper frd; fi
  ok "Installed ${#BINARIES[@]} program(s) to $(tilde "$BIN_DIR")"
  if [ "$OS" = "darwin" ]; then info "Signature: $SIGNED_WITH"; fi
}

write_manifest() {
  local staged="$MANIFEST.new"
  {
    echo "# $PRODUCT installer manifest; used by install.sh --uninstall"
    echo "version=$INSTALLED_VERSION"
    echo "source=$INSTALLED_SOURCE"
    echo "commit=$INSTALLED_COMMIT"
    echo "target=$TARGET"
    echo "mode=$([ "$CLIENT_ONLY" -eq 1 ] && echo client || echo full)"
    echo "installed_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "bin_dir=$BIN_DIR"
    echo "binaries=${BINARIES[*]}"
    echo "dest=$DEST"
    echo "host=$HOST_SERVICE"
    echo "host_unit=$HOST_UNIT"
    echo "ingress_helper=$INGRESS_HELPER"
    echo "completions=${INSTALLED_COMPLETIONS[*]+${INSTALLED_COMPLETIONS[*]}}"
    echo "skills=${INSTALLED_SKILLS[*]+${INSTALLED_SKILLS[*]}}"
    echo "path_rc=${PATH_RC_FILES[*]+${PATH_RC_FILES[*]}}"
    echo "launcher=$APP_LAUNCHER"
    echo "cache=$CACHE_DIR"
  } >"$staged"
  mv -f "$staged" "$MANIFEST"
}

# The desktop launcher (~/Applications/FrankenRemote.app on macOS, an
# applications-menu entry on Linux) runs `fr app`: the machine picker with
# its connection choices. fr writes and removes it itself; a build without a
# desktop viewer refuses, and the install continues without one.
APP_LAUNCHER="no"
install_launcher() {
  if "$BIN_DIR/fr" app --install >/dev/null 2>&1; then
    APP_LAUNCHER="yes"
    if [ "$OS" = "darwin" ]; then
      ok "Added FrankenRemote to ~/Applications"
    else
      ok "Added FrankenRemote to the applications menu"
    fi
  else
    info "No desktop launcher: this fr has no desktop viewer (fr app is unavailable)"
  fi
}

# ═══════════════════════════════════════════════════════════════════════════════
# Host service
# ═══════════════════════════════════════════════════════════════════════════════

HOST_SERVICE="none"
HOST_UNIT=""
INGRESS_HELPER="no"
HOST_STATUS=""

# A LaunchAgent this installer may manage: absent, or running our frd.
launch_agent_is_ours() {
  [ -f "$LAUNCH_AGENT" ] || return 0
  grep -qF "$BIN_DIR/frd" "$LAUNCH_AGENT"
}

setup_macos_host() {
  local domain label="com.frankenremote.frd" args=()
  domain="gui/$(id -u)"
  if ! launch_agent_is_ours; then
    warn "$LAUNCH_AGENT runs another frd; leaving it unchanged"
    HOST_STATUS="skipped: another FrankenRemote host agent is installed"
    return 0
  fi
  args=(--encoder videotoolbox --fps 60 --bitrate 30000000)
  if [ "$HOST_CONTROL" -eq 1 ]; then args+=(--input-agent "$BIN_DIR/fr-input-agent"); fi
  if [ "$HOST_CLIPBOARD" -eq 0 ]; then args+=(--no-clipboard); fi
  if [ "$AUDIO" -eq 1 ]; then args+=(--audio); fi
  info "Registering the host LaunchAgent (sharing with your own devices only)"
  "$BIN_DIR/frd" install --approval none --sharing own-user -- "${args[@]}" >/dev/null ||
    die "frd install failed"
  launchctl bootout "$domain/$label" 2>/dev/null || true
  # bootout returns while the old frd is still ending its share; bootstrapping
  # before launchd lets it go fails with an I/O error, so wait (bounded).
  local i
  for ((i = 0; i < 100; i++)); do
    launchctl print "$domain/$label" >/dev/null 2>&1 || break
    sleep 0.2
  done
  launchctl bootstrap "$domain" "$LAUNCH_AGENT" || die "launchctl bootstrap failed"
  HOST_SERVICE="launchagent"
  HOST_UNIT="$LAUNCH_AGENT"
  sleep 2
  if launchctl print "$domain/$label" 2>/dev/null | grep -q "state = running"; then
    HOST_STATUS="running (log: ~/Library/Logs/FrankenRemote/frd.log)"
    ok "Host agent running"
  else
    HOST_STATUS="registered; see ~/Library/Logs/FrankenRemote/frd.log"
    warn "The host agent is registered but not running yet"
  fi
  info "macOS asks you to allow Screen Recording$([ "$HOST_CONTROL" -eq 1 ] && echo " and Accessibility") for frd."
  if ask_yn "Open those Privacy & Security settings now?" y; then
    open "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture" 2>/dev/null || true
    if [ "$HOST_CONTROL" -eq 1 ]; then
      open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility" 2>/dev/null || true
    fi
  fi
}

# Reads one field of frd install --json output.
json_field() {
  python3 -c 'import json,sys
doc=json.load(sys.stdin)
for key in sys.argv[1].split("."):
    doc = doc.get(key) if isinstance(doc, dict) else None
print("" if doc is None else doc)' "$1"
}

# The first encoder this display can actually use, probed with fr doctor.
linux_encoder_args() {
  local encoder
  if [ -n "${DISPLAY:-}" ]; then
    for encoder in nvenc vaapi; do
      if "$BIN_DIR/fr" doctor --probe-native --worker "$BIN_DIR/fr-media-worker" \
        --encoder "$encoder" --x-display "$DISPLAY" --json >/dev/null 2>&1; then
        echo "-- --encoder $encoder"
        return 0
      fi
    done
  fi
  echo "--software-explicit --"
}

setup_linux_host() {
  local doc unit_path unit_name helper_unit helper_config helper_exec helper_copy enc run_args=()
  command -v systemctl >/dev/null 2>&1 || die "--host on Linux needs systemd"
  command -v python3 >/dev/null 2>&1 || install_linux_packages host || die "--host needs python3"
  install_linux_packages host || warn "Host packages (nftables) are missing; frd cannot enforce its ingress rule without them"
  if [ "$TAILSCALE_STATE" != "running" ]; then
    warn "Tailscale is not running; the host starts only after you connect this machine"
  fi
  enc=$(linux_encoder_args)
  if [ "$HOST_CONTROL" -eq 1 ]; then
    run_args+=(--input-agent "$BIN_DIR/fr-input-agent")
    if [ "$HOST_CLIPBOARD" -eq 1 ]; then run_args+=(--clipboard); else run_args+=(--no-clipboard); fi
  fi
  if [ "$AUDIO" -eq 1 ]; then run_args+=(--audio); fi
  info "Registering the host as a systemd user service (sharing with your own devices only)"
  # shellcheck disable=SC2086
  doc=$("$BIN_DIR/frd" install --user --json --approval none --sharing own-user $enc ${run_args[@]+"${run_args[@]}"}) ||
    die "frd install failed: $doc"
  unit_path=$(printf '%s' "$doc" | json_field unit_path)
  unit_name=$(basename "$unit_path")
  HOST_SERVICE="systemd-user"
  HOST_UNIT="$unit_name"
  # The ingress rule needs root: frd renders the helper's unit and config.
  helper_unit=$(printf '%s' "$doc" | json_field ingress_helper.unit_path)
  if [ -n "$helper_unit" ]; then
    helper_config=$(printf '%s' "$doc" | json_field ingress_helper.config_path)
    helper_exec=$(printf '%s' "$doc" | json_field ingress_helper.exec_path)
    helper_copy=$(printf '%s' "$doc" | json_field ingress_helper.copy_from)
    info "frd's tailnet-only firewall rule needs a small root service (frd ingress-helper)."
    if ask_yn "Install the root ingress helper with sudo?" y; then
      printf '%s' "$doc" | json_field ingress_helper.unit_content >"$TMP/helper.service"
      printf '%s' "$doc" | json_field ingress_helper.config_content >"$TMP/helper.json"
      if [ -n "$helper_copy" ]; then
        as_root install -D -m 0755 -o root -g root "$helper_copy" "$helper_exec"
      fi
      as_root install -D -m 0644 -o root -g root "$TMP/helper.json" "$helper_config" &&
        as_root install -D -m 0644 -o root -g root "$TMP/helper.service" "$helper_unit" &&
        as_root systemctl daemon-reload &&
        as_root systemctl enable --now "$(basename "$helper_unit")" &&
        INGRESS_HELPER="yes"
      if [ "$INGRESS_HELPER" = "yes" ]; then
        ok "Ingress helper running ($(basename "$helper_unit"))"
        if tailscale_cli >/dev/null 2>&1; then
          as_root "$(tailscale_cli)" set --operator="$(id -un)" >/dev/null 2>&1 &&
            ok "Tailscale operator set to $(id -un) (frd reads certificates without root)"
        fi
      else
        warn "The ingress helper was not installed; frd refuses to listen without it"
      fi
    fi
  fi
  systemctl --user daemon-reload
  if systemctl --user enable --now "$unit_name" >/dev/null 2>&1; then
    sleep 2
    if systemctl --user is-active --quiet "$unit_name"; then
      HOST_STATUS="running ($unit_name; journalctl --user -u $unit_name)"
      ok "Host service running"
    else
      HOST_STATUS="enabled, not running yet (journalctl --user -u $unit_name)"
      warn "The host service is enabled but not running; check journalctl --user -u $unit_name"
    fi
  else
    HOST_STATUS="registered; start it with systemctl --user enable --now $unit_name"
    warn "Could not start $unit_name from this session (no graphical session bus?)"
  fi
}

# After an update, restart a host service this installer registered.
restart_existing_host() {
  local previous
  previous=$(manifest_get host 2>/dev/null || echo none)
  # A LaunchAgent from scripts/macos/install-host.sh runs the same folder's frd.
  if [ "$previous" = "none" ] && [ "$OS" = "darwin" ] && [ "$CLIENT_ONLY" -eq 0 ] &&
    [ -f "$LAUNCH_AGENT" ] && launch_agent_is_ours; then
    previous="launchagent"
  fi
  case "$previous" in
    launchagent)
      if [ -f "$LAUNCH_AGENT" ] && launch_agent_is_ours; then
        launchctl kickstart -k "gui/$(id -u)/com.frankenremote.frd" >/dev/null 2>&1 &&
          ok "Restarted the host agent on the new binaries"
        HOST_SERVICE="launchagent"
        HOST_UNIT="$LAUNCH_AGENT"
        HOST_STATUS="restarted on the new binaries"
      fi
      ;;
    systemd-user)
      HOST_UNIT=$(manifest_get host_unit 2>/dev/null || true)
      INGRESS_HELPER=$(manifest_get ingress_helper 2>/dev/null || echo no)
      if [ -n "$HOST_UNIT" ] && systemctl --user restart "$HOST_UNIT" >/dev/null 2>&1; then
        ok "Restarted $HOST_UNIT on the new binaries"
      fi
      HOST_SERVICE="systemd-user"
      HOST_STATUS="restarted on the new binaries"
      ;;
  esac
}

# ═══════════════════════════════════════════════════════════════════════════════
# Shell integration
# ═══════════════════════════════════════════════════════════════════════════════

PATH_RC_FILES=()
INSTALLED_COMPLETIONS=()
INSTALLED_SKILLS=()
INSTALLED_SKILL_AGENTS=()
PATH_STATUS=""

maybe_add_path() {
  case ":$PATH:" in
    *:"$DEST":*) PATH_STATUS="already on PATH"; return 0 ;;
  esac
  if [ "$EASY" -eq 1 ]; then
    local rc line
    for rc in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile"; do
      [ -e "$rc" ] && [ -w "$rc" ] || continue
      if ! grep -q "# >>> franken_remote PATH >>>" "$rc" 2>/dev/null; then
        {
          echo ""
          echo "# >>> franken_remote PATH >>>"
          echo "export PATH=\"$DEST:\$PATH\""
          echo "# <<< franken_remote PATH <<<"
        } >>"$rc"
      fi
      PATH_RC_FILES+=("$rc")
    done
    if [ -d "${XDG_CONFIG_HOME:-$HOME/.config}/fish" ]; then
      line="${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/franken_remote.fish"
      mkdir -p "$(dirname "$line")"
      echo "fish_add_path -g $(sh_quote "$DEST")" >"$line"
      PATH_RC_FILES+=("$line")
    fi
    PATH_STATUS="added to ${PATH_RC_FILES[*]+${PATH_RC_FILES[*]}} (open a new shell)"
  else
    PATH_STATUS="not on PATH: add $(tilde "$DEST") to PATH (or rerun with --easy-mode)"
    warn "$DEST is not on PATH"
  fi
}

COMPLETION_MARK="franken_remote installer completion"

completion_bash() {
  cat <<'EOF_BASH'
# franken_remote installer completion for fr (bash)
_fr() {
  local cur prev cmd
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"
  cmd="${COMP_WORDS[1]:-}"
  if [ "$COMP_CWORD" -eq 1 ]; then
    COMPREPLY=($(compgen -W "hosts peers status doctor inspect disconnect displays connect robot send help --help" -- "$cur"))
    return
  fi
  case "$prev" in
    --display) COMPREPLY=($(compgen -W "only choose" -- "$cur")); return ;;
    --receive|--send|--trust-roots|--worker|--socket|--audio-server) COMPREPLY=($(compgen -f -- "$cur")); return ;;
  esac
  case "$cmd" in
    connect) COMPREPLY=($(compgen -W "--view-only --control --experimental-native --display --choose-host --saved --by-name --attempts --no-clipboard --send --choose-files --file-drop --receive --audio --microphone --ipv6 --json --help" -- "$cur")) ;;
    hosts) COMPREPLY=($(compgen -W "save saved forget --probe --experimental-native --probe-limit --json --socket --help" -- "$cur")) ;;
    robot) COMPREPLY=($(compgen -W "session observe input open status close --help" -- "$cur")) ;;
    help) COMPREPLY=($(compgen -W "connect robot send" -- "$cur")) ;;
    *) COMPREPLY=($(compgen -W "--json --help" -- "$cur")) ;;
  esac
}
complete -F _fr fr
EOF_BASH
}

completion_zsh() {
  cat <<'EOF_ZSH'
#compdef fr
# franken_remote installer completion for fr (zsh)
_fr() {
  local -a commands
  commands=(
    'hosts:list machines from the installed Tailscale'
    'peers:list peers without probing'
    'status:this node'"'"'s Tailscale identity'
    'doctor:local readiness checks'
    'inspect:authenticated capability check of a host'
    'disconnect:end the foreground connection'
    'displays:list a host'"'"'s displays'
    'connect:view or control a host'
    'robot:agent session, observation and input commands'
    'send:later file selections for a connection'
    'help:detailed help'
  )
  if (( CURRENT == 2 )); then
    _describe 'command' commands
    return
  fi
  case $words[2] in
    connect)
      _arguments '*:: :->args' \
        '--view-only[observe only]' '--control[request control]' \
        '--experimental-native[native desktop session]' \
        '--display[display handle]:display:(only choose)' \
        '--choose-host[pick the machine in a window]' '--no-clipboard[no clipboard]' \
        '--audio[host playback audio]' '--json[JSON completion]' ;;
    *) _arguments '--json[JSON output]' '--help[help]' ;;
  esac
}
_fr "$@"
EOF_ZSH
}

completion_fish() {
  cat <<'EOF_FISH'
# franken_remote installer completion for fr (fish)
complete -c fr -f
complete -c fr -n __fish_use_subcommand -a hosts -d 'List machines from the installed Tailscale'
complete -c fr -n __fish_use_subcommand -a peers -d 'List peers without probing'
complete -c fr -n __fish_use_subcommand -a status -d "This node's Tailscale identity"
complete -c fr -n __fish_use_subcommand -a doctor -d 'Local readiness checks'
complete -c fr -n __fish_use_subcommand -a inspect -d 'Authenticated capability check'
complete -c fr -n __fish_use_subcommand -a disconnect -d 'End the foreground connection'
complete -c fr -n __fish_use_subcommand -a displays -d "List a host's displays"
complete -c fr -n __fish_use_subcommand -a connect -d 'View or control a host'
complete -c fr -n __fish_use_subcommand -a robot -d 'Agent session, observation and input'
complete -c fr -n __fish_use_subcommand -a send -d 'File selections for a connection'
complete -c fr -n __fish_use_subcommand -a help -d 'Detailed help'
complete -c fr -n '__fish_seen_subcommand_from connect' -l view-only -d 'Observe only'
complete -c fr -n '__fish_seen_subcommand_from connect' -l control -d 'Request control'
complete -c fr -n '__fish_seen_subcommand_from connect' -l experimental-native -d 'Native desktop session'
complete -c fr -n '__fish_seen_subcommand_from connect' -l display -xa 'only choose'
complete -c fr -n '__fish_seen_subcommand_from connect' -l choose-host -d 'Pick the machine in a window'
complete -c fr -n '__fish_seen_subcommand_from connect' -l no-clipboard -d 'No clipboard sharing'
complete -c fr -l json -d 'JSON output'
EOF_FISH
}

install_completion_file() {
  local path="$1" generator="$2"
  if [ -e "$path" ] && ! grep -q "$COMPLETION_MARK" "$path" 2>/dev/null; then
    return 0
  fi
  mkdir -p "$(dirname "$path")"
  "$generator" >"$path"
  INSTALLED_COMPLETIONS+=("$path")
}

install_completions() {
  install_completion_file "$COMPLETION_BASH" completion_bash
  install_completion_file "$COMPLETION_ZSH" completion_zsh
  if command -v fish >/dev/null 2>&1 || [ -d "${XDG_CONFIG_HOME:-$HOME/.config}/fish" ]; then
    install_completion_file "$COMPLETION_FISH" completion_fish
  fi
  ok "Shell completions installed (bash, zsh$([ -f "$COMPLETION_FISH" ] && echo ", fish"))"
}

# ═══════════════════════════════════════════════════════════════════════════════
# Agent integration
# ═══════════════════════════════════════════════════════════════════════════════

DETECTED_AGENTS=()
SKILL_MARK=".franken_remote_installer"

detect_agents() {
  DETECTED_AGENTS=()
  if [ -d "${CLAUDE_CONFIG_DIR:-$HOME/.claude}" ] || command -v claude >/dev/null 2>&1; then DETECTED_AGENTS+=("claude"); fi
  if [ -d "$HOME/.codex" ] || command -v codex >/dev/null 2>&1; then DETECTED_AGENTS+=("codex"); fi
  if [ -d "$HOME/.gemini" ] || command -v gemini >/dev/null 2>&1; then DETECTED_AGENTS+=("gemini"); fi
  if [ -d "$HOME/.cursor" ] || command -v cursor >/dev/null 2>&1; then DETECTED_AGENTS+=("cursor"); fi
}

# Fallback copy of docs/agent-skill/SKILL.md (keep the two identical).
skill_text() {
  cat <<'EOF_SKILL'
---
name: franken-remote
description: View or control a remote desktop on the user's tailnet with FrankenRemote's fr CLI. Use when asked to look at, screenshot or operate another machine, list tailnet machines, or check a FrankenRemote host.
---

# FrankenRemote (`fr`)

FrankenRemote is a remote desktop over Tailscale. Tailscale supplies
connectivity and identity; there are no FrankenRemote accounts or passwords.
The host (`frd`) decides who may view or control it.

## Look before acting

```bash
fr status --json          # this machine's Tailscale identity
fr hosts --json           # machines on the tailnet (not proof they host)
fr inspect NODE_ID --probe --experimental-native --json   # what a host offers
```

Use a peer's `node_id` from `fr hosts --json` as NODE_ID.

## People: view or control

```bash
fr connect NODE_ID --view-only --experimental-native --display only
fr connect NODE_ID --control --experimental-native --display choose
fr connect --choose-host --view-only --experimental-native --display only
```

## Agents: foreground robot sessions

Robot sessions stay in the foreground and need explicit owner modes:
`--publish-observations` to publish views and `--accept-input` to accept
robot input. Read `fr help robot` first; it is the authoritative syntax.

```bash
fr help robot
fr robot session open NODE_ID --foreground --role view --publish-observations --experimental-native --display only
fr robot session open NODE_ID --foreground --role control --accept-input --experimental-native --display only
fr robot session status NODE_ID --json      # connection ID and lease
fr robot observe NODE_ID --connection-id ID --screenshot /tmp/screen.png --json
fr robot input NODE_ID --connection-id ID --status --json
fr robot session close NODE_ID --connection-id ID --json
```

Rules:
- Never control a machine the user did not name. View-only is the default.
- Input needs the original lease, a fresh observation ID and an age bound;
  a submitted action is not proof the application did what you wanted.
  Observe again to confirm. Uncertain effects are never retried for you.
- Clipboard text, keystrokes and pixels are private: do not paste them into
  logs or other tools without being asked.

Windows has the `FrankenRemote` desktop app and `fr-windows` (`hosts`,
`view NODE_ID`); robot commands exist only in the macOS/Linux `fr`.
EOF_SKILL
}

install_agent_skills() {
  local agent root dest
  [ "${#DETECTED_AGENTS[@]}" -gt 0 ] || return 0
  for agent in "${DETECTED_AGENTS[@]}"; do
    case "$agent" in
      claude) root="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/skills" ;;
      codex) root="$HOME/.codex/skills" ;;
      *) continue ;;
    esac
    dest="$root/franken-remote"
    if [ -d "$dest" ] && [ ! -f "$dest/$SKILL_MARK" ]; then
      info "Keeping the existing $dest (not installed by this installer)"
      continue
    fi
    mkdir -p "$dest"
    # Prefer the skill shipped with the installed source, else the inline copy.
    if [ "$INSTALLED_SOURCE" = "source" ] && [ -f "$SRC_DIR/docs/agent-skill/SKILL.md" ]; then
      cp "$SRC_DIR/docs/agent-skill/SKILL.md" "$dest/SKILL.md"
    elif ! fetch "${RAW_BASE}/${INSTALLED_COMMIT:-main}/docs/agent-skill/SKILL.md" "$dest/SKILL.md" 2>/dev/null; then
      skill_text >"$dest/SKILL.md"
    fi
    : >"$dest/$SKILL_MARK"
    INSTALLED_SKILLS+=("$dest")
    INSTALLED_SKILL_AGENTS+=("$agent")
  done
  if [ "${#INSTALLED_SKILLS[@]}" -gt 0 ]; then ok "Agent skill installed for: ${INSTALLED_SKILL_AGENTS[*]}"; fi
}

# ═══════════════════════════════════════════════════════════════════════════════
# Uninstall
# ═══════════════════════════════════════════════════════════════════════════════

uninstall() {
  [ -f "$MANIFEST" ] || die "No $PRODUCT installation recorded at $MANIFEST"
  local host unit helper item dir binaries completions skills rcs dest cache
  host=$(manifest_get host || echo none)
  unit=$(manifest_get host_unit || true)
  helper=$(manifest_get ingress_helper || echo no)
  binaries=$(manifest_get binaries || true)
  completions=$(manifest_get completions || true)
  skills=$(manifest_get skills || true)
  rcs=$(manifest_get path_rc || true)
  dest=$(manifest_get dest || echo "$DEST")
  cache=$(manifest_get cache || echo "$CACHE_DIR")
  info "Removing $PRODUCT ($(manifest_get version || echo unknown))"
  case "$host" in
    launchagent)
      if launch_agent_is_ours; then
        launchctl bootout "gui/$(id -u)/com.frankenremote.frd" 2>/dev/null || true
        "$BIN_DIR/frd" uninstall >/dev/null 2>&1 || true
        ok "Host LaunchAgent removed"
      fi
      ;;
    systemd-user)
      if [ -n "$unit" ]; then
        systemctl --user disable --now "$unit" >/dev/null 2>&1 || true
        "$BIN_DIR/frd" uninstall --user >/dev/null 2>&1 || true
        systemctl --user daemon-reload >/dev/null 2>&1 || true
        ok "Host service $unit removed"
      fi
      if [ "$helper" = "yes" ]; then
        warn "The root ingress helper stays installed. Remove it with:"
        warn "  sudo systemctl disable --now frd-ingress-helper.service && sudo frd uninstall --system"
      fi
      ;;
  esac
  for item in fr frd; do
    if [ -f "$dest/$item" ] && grep -q "$WRAPPER_MARK" "$dest/$item" 2>/dev/null; then
      rm -f "$dest/$item"
    fi
  done
  # fr removes only the launcher it wrote; do this while fr still exists.
  if [ "$(manifest_get launcher || echo no)" = "yes" ] && [ -x "$BIN_DIR/fr" ]; then
    "$BIN_DIR/fr" app --uninstall >/dev/null 2>&1 && ok "Desktop launcher removed"
  fi
  for item in $binaries; do rm -f "$BIN_DIR/$item"; done
  rmdir "$BIN_DIR" 2>/dev/null || true
  for item in $completions; do
    if [ -f "$item" ] && grep -q "$COMPLETION_MARK" "$item" 2>/dev/null; then rm -f "$item"; fi
  done
  for dir in $skills; do
    if [ -f "$dir/$SKILL_MARK" ]; then rm -f "$dir/SKILL.md" "$dir/$SKILL_MARK"; rmdir "$dir" 2>/dev/null || true; fi
  done
  for item in $rcs; do
    case "$item" in
      *.fish) rm -f "$item" ;;
      *)
        if [ -f "$item" ] && grep -q "# >>> franken_remote PATH >>>" "$item"; then
          sed '/# >>> franken_remote PATH >>>/,/# <<< franken_remote PATH <<</d' "$item" >"$item.franken_remote.tmp" &&
            cat "$item.franken_remote.tmp" >"$item" && rm -f "$item.franken_remote.tmp"
        fi
        ;;
    esac
  done
  rm -f "$MANIFEST"
  rmdir "$APP_DIR" 2>/dev/null || true
  if [ -d "$cache" ] && [ "$KEEP_CACHE" -eq 0 ]; then
    if ask_yn "Remove the source build cache at $cache?" y; then
      rm -rf "$cache"
      ok "Build cache removed"
    fi
  fi
  ok "$PRODUCT removed. Tailscale and your tailnet settings were not changed."
}

# ═══════════════════════════════════════════════════════════════════════════════
# Summary
# ═══════════════════════════════════════════════════════════════════════════════

NODE_NAME=""
MACHINE_COUNT=""

post_install_checks() {
  local doc
  # Display only: a missing field leaves the summary line shorter.
  if doc=$("$BIN_DIR/fr" status --json 2>/dev/null); then
    NODE_NAME=$(printf '%s' "$doc" | sed -n 's/.*"local_node_name": *"\([^"]*\)".*/\1/p' | head -n 1) || NODE_NAME=""
  fi
  if doc=$("$BIN_DIR/fr" hosts --json 2>/dev/null); then
    MACHINE_COUNT=$(printf '%s' "$doc" | grep -o '"node_id"' | wc -l | tr -d ' ') || MACHINE_COUNT=""
  fi
  if [ "$VERIFY" -eq 1 ]; then
    info "Running fr doctor"
    "$BIN_DIR/fr" doctor || warn "fr doctor reported problems (see above)"
  fi
}

# A path for display, with the home directory as ~ (a literal, never expanded).
# shellcheck disable=SC2088
tilde() {
  case "$1" in
    "$HOME") printf '~' ;;
    "$HOME"/*) printf '~/%s' "${1#"$HOME"/}" ;;
    *) printf '%s' "$1" ;;
  esac
}

print_summary() {
  [ "$QUIET" -eq 1 ] && return 0
  local lines=() dest_shown
  dest_shown=$(tilde "$DEST")
  lines+=("Version:    $INSTALLED_VERSION ($INSTALLED_SOURCE)")
  lines+=("Programs:   $(tilde "$BIN_DIR")")
  if [ "$CLIENT_ONLY" -eq 0 ]; then
    lines+=("Commands:   $dest_shown/fr, $dest_shown/frd")
  else
    lines+=("Commands:   $dest_shown/fr")
  fi
  lines+=("PATH:       $PATH_STATUS")
  case "$TAILSCALE_STATE" in
    running) lines+=("Tailscale:  running${NODE_NAME:+ as $NODE_NAME}${MACHINE_COUNT:+, $MACHINE_COUNT machines visible}") ;;
    installed) lines+=("Tailscale:  installed, not connected (sign in, then run fr hosts)") ;;
    *) lines+=("Tailscale:  NOT installed (https://tailscale.com/download)") ;;
  esac
  if [ "$HOST_SERVICE" != "none" ]; then
    lines+=("Host:       $HOST_STATUS")
  elif [ "$CLIENT_ONLY" -eq 0 ]; then
    lines+=("Host:       not set up (rerun with --host to share this desktop)")
  fi
  if [ "$APP_LAUNCHER" = "yes" ]; then
    if [ "$OS" = "darwin" ]; then
      lines+=("App:        FrankenRemote in ~/Applications (Spotlight, Launchpad)")
    else
      lines+=("App:        FrankenRemote in the applications menu")
    fi
  fi
  if [ "$NO_CONFIGURE" -eq 0 ]; then
    lines+=("Completions: ${#INSTALLED_COMPLETIONS[@]} shell(s)")
    if [ "${#INSTALLED_SKILLS[@]}" -gt 0 ]; then
      lines+=("Agent skill: ${INSTALLED_SKILL_AGENTS[*]}")
    else
      lines+=("Agent skill: no Claude Code or Codex found (skipped)")
    fi
  fi
  if use_gum; then
    {
      gum style --foreground 42 --bold "$PRODUCT is installed"
      echo ""
      for line in "${lines[@]}"; do gum style --foreground 245 "$line"; done
      echo ""
      gum style --foreground 39 "Next:"
      gum style --foreground 245 "  fr app                                     # choose a machine and connect"
      gum style --foreground 245 "  fr hosts                                   # machines on your tailnet"
      gum style --foreground 245 "  fr connect --choose-host --view-only --experimental-native --display only"
    } | gum style --border normal --border-foreground 42 --padding "1 2"
    gum style --foreground 245 --italic "To uninstall: curl -fsSL ${RAW_BASE}/main/install.sh | bash -s -- --uninstall"
  else
    echo ""
    draw_box "0;32" "\033[1;32m$PRODUCT is installed\033[0m" "" "${lines[@]}" "" \
      "Next:" \
      "  fr app                      # choose a machine and connect" \
      "  fr hosts                    # machines on your tailnet" \
      "  fr connect --choose-host --view-only --experimental-native --display only"
    echo -e "\033[0;90mTo uninstall: curl -fsSL ${RAW_BASE}/main/install.sh | bash -s -- --uninstall\033[0m"
  fi
}

# ═══════════════════════════════════════════════════════════════════════════════
# Main
# ═══════════════════════════════════════════════════════════════════════════════

main() {
  detect_platform
  detect_package_manager
  set_paths
  setup_proxy
  show_header
  acquire_lock
  TMP=$(mktemp -d "${TMPDIR:-/tmp}/franken_remote-install.XXXXXX")
  trap cleanup EXIT

  if [ "$UNINSTALL" -eq 1 ]; then
    uninstall
    return 0
  fi

  select_binaries
  info "Platform: $OS/$ARCH ($TARGET), install mode: $([ "$CLIENT_ONLY" -eq 1 ] && echo client || echo "client + host")"

  resolve_version
  preflight_checks
  check_tailscale

  local current_version current_commit acquired=0
  current_version=$(manifest_get version 2>/dev/null || true)
  current_commit=$(manifest_get commit 2>/dev/null || true)

  if [ "$FROM_SOURCE" -eq 0 ]; then
    if [ "$FORCE" -eq 0 ] && [ -n "$VERSION" ] && [ "$current_version" = "$VERSION" ] && [ -x "$BIN_DIR/fr" ]; then
      ok "$PRODUCT $VERSION is already installed (use --force to reinstall)"
      INSTALLED_VERSION="$VERSION"
      INSTALLED_SOURCE=$(manifest_get source || echo release)
      INSTALLED_COMMIT="$current_commit"
      acquired=2
    elif acquire_release; then
      acquired=1
    else
      [ -n "$OFFLINE_TARBALL" ] && die "The offline tarball cannot be installed on this machine"
      warn "Falling back to a source build"
      FROM_SOURCE=1
      check_disk_space
    fi
  fi
  if [ "$acquired" -eq 0 ]; then
    if [ "$FORCE" -eq 0 ] && [ -n "$current_commit" ] && [ -x "$BIN_DIR/fr" ]; then
      local remote_commit=""
      remote_commit=$(git ls-remote "${REPO_URL}.git" "$(source_ref)" "refs/tags/$(source_ref)" 2>/dev/null | awk 'NR==1 {print $1}' || true)
      if [ -n "$remote_commit" ] && [ "$remote_commit" = "$current_commit" ]; then
        ok "$PRODUCT is already built from $(source_ref) at ${current_commit:0:10} (use --force to rebuild)"
        INSTALLED_VERSION="$current_version"
        INSTALLED_SOURCE="source"
        INSTALLED_COMMIT="$current_commit"
        acquired=2
      fi
    fi
    if [ "$acquired" -eq 0 ]; then
      build_from_source
      acquired=1
    fi
  fi

  if [ "$acquired" -eq 1 ]; then
    install_payload
    restart_existing_host
  else
    restart_existing_host >/dev/null 2>&1 || true
  fi

  if [ "$HOST" -eq 1 ]; then
    if [ "$OS" = "darwin" ]; then setup_macos_host; else setup_linux_host; fi
  fi

  if [ "$NO_CONFIGURE" -eq 0 ]; then
    maybe_add_path
    install_completions
    install_launcher
    detect_agents
    install_agent_skills
  else
    PATH_STATUS="not changed (--no-configure)"
  fi
  write_manifest
  post_install_checks
  print_summary
}

# FR_INSTALL_LIB_ONLY=1 loads the functions without installing (tests).
if [ "${FR_INSTALL_LIB_ONLY:-0}" != "1" ]; then
  main
fi

#!/usr/bin/env bash
# FrankenRemote Phase 0 Spike: macOS CoreAudio Server Plugin Virtual Microphone Probe Runner
# Provenance: plan sections 15.4, 23 Phase 0; bead fr-p0-virtual-mic-xyh
set -euo pipefail

TARGET_HOST="${1:-100.68.51.94}"
TARGET_USER="${2:-jemanuel}"
SSH_KEY="${3:-$HOME/.ssh/mmini_ed25519}"

echo "=== FrankenRemote macOS Virtual Microphone Probe ==="
echo "Target: ${TARGET_USER}@${TARGET_HOST}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROBE_SOURCE="${SCRIPT_DIR}/probe_coreaudio.c"
RESULTS_DIR="${SCRIPT_DIR}/results/m4-pro-macos-26.2"
mkdir -p "${RESULTS_DIR}"

echo "1. Deploying probe source to remote macOS host..."
scp -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${PROBE_SOURCE}" "${TARGET_USER}@${TARGET_HOST}:/tmp/probe_coreaudio.c"

echo "2. Compiling probe on remote host using Apple Clang..."
ssh -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${TARGET_USER}@${TARGET_HOST}" \
  "clang -O2 -x objective-c -framework CoreAudio -framework AudioToolbox -framework AVFoundation -framework Foundation /tmp/probe_coreaudio.c -o /tmp/probe_coreaudio"

echo "3. Collecting environment details..."
ssh -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${TARGET_USER}@${TARGET_HOST}" \
  "sw_vers && sysctl -n machdep.cpu.brand_string && clang --version | head -n 1" > "${RESULTS_DIR}/environment.txt"

echo "4. Validating code signature of CoreAudio server plugin..."
ssh -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${TARGET_USER}@${TARGET_HOST}" \
  "codesign -dvvv /Library/Audio/Plug-Ins/HAL/BlackHole2ch.driver 2>&1" > "${RESULTS_DIR}/codesign.txt"

echo "5. Executing probe..."
ssh -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${TARGET_USER}@${TARGET_HOST}" \
  "/tmp/probe_coreaudio /tmp/probe_result.json"

echo "6. Retrieving result JSON..."
scp -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${TARGET_USER}@${TARGET_HOST}:/tmp/probe_result.json" "${RESULTS_DIR}/probe_result.json"

echo "7. Result summary:"
cat "${RESULTS_DIR}/probe_result.json"
echo ""
echo "=== Probe complete. Evidence stored in ${RESULTS_DIR} ==="

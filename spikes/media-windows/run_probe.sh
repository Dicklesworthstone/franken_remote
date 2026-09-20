#!/usr/bin/env bash
# FrankenRemote Phase 0 Spike: Windows Desktop Duplication, D3D11 Video, and Hardware HEVC Probe Runner
# Provenance: plan sections 8.3, 9.1, 10.3, 23 Phase 0; bead fr-p0-media-windows-fbt
set -euo pipefail

TARGET_HOST="${1:-100.68.2.11}"
TARGET_USER="${2:-jeffr}"
SSH_KEY="${3:-$HOME/.ssh/threadripper_to_surfacebookje}"

echo "=== FrankenRemote Windows Media Probe ==="
echo "Target: ${TARGET_USER}@${TARGET_HOST}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROBE_SOURCE="${SCRIPT_DIR}/probe_windows_media.cpp"

echo "1. Deploying probe source to remote host..."
scp -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${PROBE_SOURCE}" "${TARGET_USER}@${TARGET_HOST}:C:/rch/probe_windows_media.cpp"

echo "2. Compiling probe on remote host using MSVC cl.exe..."
ssh -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${TARGET_USER}@${TARGET_HOST}" \
  "MSYS2_ARG_CONV_EXCL=\"*\" cl.exe -nologo -O2 -W3 -EHsc C:/rch/probe_windows_media.cpp -Fe:C:/rch/probe_windows_media.exe"

echo "3. Executing Session 0 (service-session) probe..."
ssh -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${TARGET_USER}@${TARGET_HOST}" \
  "C:/rch/probe_windows_media.exe C:/rch/probe_session0.json"

echo "4. Executing Session 1 (interactive-session) probe via task scheduler..."
ssh -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${TARGET_USER}@${TARGET_HOST}" \
  "MSYS2_ARG_CONV_EXCL=\"*\" schtasks /create /tn \"FRProbe\" /tr \"C:\\rch\\probe_windows_media.exe C:\\rch\\probe_session1.json\" /sc once /st 00:00 /it /f && MSYS2_ARG_CONV_EXCL=\"*\" schtasks /run /tn \"FRProbe\""

sleep 2

echo "5. Retrieving results and cleaning up scheduled task..."
ssh -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${TARGET_USER}@${TARGET_HOST}" \
  "MSYS2_ARG_CONV_EXCL=\"*\" schtasks /delete /tn \"FRProbe\" /f"

echo "6. Session 0 Output:"
ssh -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${TARGET_USER}@${TARGET_HOST}" "cat C:/rch/probe_session0.json"

echo "7. Session 1 Output:"
ssh -o BatchMode=yes -o ConnectTimeout=10 -i "${SSH_KEY}" "${TARGET_USER}@${TARGET_HOST}" "cat C:/rch/probe_session1.json"

echo "=== Probe Complete ==="

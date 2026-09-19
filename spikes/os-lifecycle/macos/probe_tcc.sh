#!/usr/bin/env bash
# FrankenRemote Phase 0 OS Lifecycle Spike: macOS TCC & Session Discovery
# Usage: spikes/os-lifecycle/macos/probe_tcc.sh
set -euo pipefail

echo "=== FrankenRemote macOS OS Lifecycle & Permission Probe ==="
DATE_UTC="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
echo "Timestamp (UTC): ${DATE_UTC}"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "Notice: Not running on macOS (current OS: $(uname -s))."
    echo "This script provides the exact commands and checks executed on Darwin hosts."
    echo "Exiting with diagnostic summary."
    exit 0
fi

echo "macOS Version: $(sw_vers -productVersion) (Build $(sw_vers -buildVersion))"
echo "Hardware:      $(uname -m)"

echo -e "\n--- Screen Recording Permission (TCC) ---"
python3 - << 'EOF' || echo "Python CGPreflightScreenCaptureAccess check failed"
import ctypes
import sys

try:
    core_graphics = ctypes.cdll.LoadLibrary('/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics')
    can_record = core_graphics.CGPreflightScreenCaptureAccess()
    print(f"CGPreflightScreenCaptureAccess: {bool(can_record)}")
except Exception as e:
    print(f"Error checking Screen Capture access: {e}")
EOF

echo -e "\n--- Accessibility Permission (TCC) ---"
python3 - << 'EOF' || echo "Python AXIsProcessTrusted check failed"
import ctypes

try:
    app_services = ctypes.cdll.LoadLibrary('/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices')
    is_trusted = app_services.AXIsProcessTrusted()
    print(f"AXIsProcessTrusted: {bool(is_trusted)}")
except Exception as e:
    print(f"Error checking Accessibility access: {e}")
EOF

echo -e "\n--- Active Power Management Assertions (pmset) ---"
pmset -g assertions 2>/dev/null | grep -E "PreventUserIdle|NoDisplaySleep" || echo "No active idle-sleep assertions"

echo -e "\n--- Window Server Session State ---"
python3 - << 'EOF' || echo "Python WindowServer session check failed"
import ctypes

try:
    cg = ctypes.cdll.LoadLibrary('/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics')
    cg.CGSessionCopyCurrentDictionary.restype = ctypes.c_void_p
    session_dict = cg.CGSessionCopyCurrentDictionary()
    print(f"CGSessionCopyCurrentDictionary handle: {session_dict is not None}")
except Exception as e:
    print(f"Error inspecting session dictionary: {e}")
EOF

echo -e "\n=== End of macOS Probe ==="

#!/usr/bin/env bash
# FrankenRemote Phase 0 OS Lifecycle Spike: Linux Portal & Session Discovery
# Usage: spikes/os-lifecycle/linux/probe_portal.sh
set -euo pipefail

echo "=== FrankenRemote Linux OS Lifecycle & Portal Probe ==="
DATE_UTC="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
echo "Timestamp (UTC): ${DATE_UTC}"
echo "Hostname:        $(hostname)"
echo "Kernel:          $(uname -r)"

echo -e "\n--- Desktop & Session Environment ---"
echo "XDG_SESSION_TYPE:    ${XDG_SESSION_TYPE:-<unset>}"
echo "XDG_CURRENT_DESKTOP: ${XDG_CURRENT_DESKTOP:-<unset>}"
echo "WAYLAND_DISPLAY:     ${WAYLAND_DISPLAY:-<unset>}"
echo "DISPLAY:             ${DISPLAY:-<unset>}"
echo "DBUS_SESSION_BUS:    ${DBUS_SESSION_BUS_ADDRESS:-<unset>}"

echo -e "\n--- systemd-logind Session State ---"
if command -v loginctl >/dev/null 2>&1; then
    loginctl list-sessions --no-legend 2>/dev/null || echo "loginctl list-sessions failed"
    CURRENT_SESSION="$(loginctl session-status 2>/dev/null | head -n 1 | awk '{print $1}' || echo "")"
    if [[ -n "${CURRENT_SESSION}" ]]; then
        echo "Current session ID: ${CURRENT_SESSION}"
        loginctl show-session "${CURRENT_SESSION}" -p Type -p Class -p Seat -p Active -p State -p CanLock -p LockedHint 2>/dev/null || true
    fi
else
    echo "loginctl not available"
fi

echo -e "\n--- Desktop Portal Availability (D-Bus) ---"
HAS_BUS=0
if command -v busctl >/dev/null 2>&1; then
    if busctl --user status >/dev/null 2>&1; then
        HAS_BUS=1
        echo "User D-Bus session bus: ACTIVE"
        
        PORTAL_OWNED="$(busctl --user call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus NameHasOwner s org.freedesktop.portal.Desktop 2>/dev/null | awk '{print $2}' || echo "false")"
        echo "org.freedesktop.portal.Desktop owner active: ${PORTAL_OWNED}"

        if [[ "${PORTAL_OWNED}" == "true" ]]; then
            echo -e "\nRemoteDesktop Portal Introspection:"
            busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.RemoteDesktop 2>/dev/null || echo "RemoteDesktop interface introspection failed"

            echo -e "\nScreenCast Portal Introspection:"
            busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.ScreenCast 2>/dev/null || echo "ScreenCast interface introspection failed"

            echo -e "\nRemoteDesktop version property:"
            busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.RemoteDesktop version 2>/dev/null || echo "Failed to read version"
            
            echo -e "\nRemoteDesktop AvailableDeviceTypes:"
            busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.RemoteDesktop AvailableDeviceTypes 2>/dev/null || echo "Failed to read AvailableDeviceTypes"
        else
            echo "Desktop portal service not running on user bus."
        fi
    else
        echo "User D-Bus session bus: INACTIVE or UNREACHABLE"
    fi
else
    echo "busctl not available"
fi

echo -e "\n--- Installed Portal & PipeWire Packages ---"
if command -v dpkg-query >/dev/null 2>&1; then
    dpkg-query -W -f='${binary:Package} ${Version} (${Status})\n' "xdg-desktop-portal*" "pipewire*" "libei*" 2>/dev/null || echo "None found via dpkg"
elif command -v pacman >/dev/null 2>&1; then
    pacman -Q | grep -E "xdg-desktop-portal|pipewire|libei" || echo "None found via pacman"
elif command -v rpm >/dev/null 2>&1; then
    rpm -qa | grep -E "xdg-desktop-portal|pipewire|libei" || echo "None found via rpm"
fi

echo -e "\n=== End of Probe ==="

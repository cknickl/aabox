#!/usr/bin/env bash
# One-shot installer: copies the AABox daemon + watchdog + init.rc into
# /system on the CM5 so the daemon auto-starts on every boot.
#
# Run from the build VM with the CM5 reachable over wireless ADB.
# Requires the ADB shell to have root (run `adb root` once before this).
#
# Usage:
#   CM5_ADDR=172.16.10.111:NEW_PORT tools/install-init-service.sh
set -euo pipefail
cd "$(dirname "$0")/.."

: "${CM5_ADDR:?Set CM5_ADDR=ip:port (after adb root cycled the port)}"
PATH="/aosp/aosp/out/host/linux-x86/bin:$PATH"

A=(adb -s "$CM5_ADDR")

# Sanity: must be root
SHELL_ID=$("${A[@]}" shell id)
echo "[install] shell uid: $SHELL_ID"
[[ "$SHELL_ID" =~ uid=0 ]] || { echo "ERROR: not root. Run \`adb root\` first."; exit 1; }

echo "[install] remounting / rw..."
"${A[@]}" shell "mount -o rw,remount /"

echo "[install] cross-compiling latest daemon..."
./tools/build-android.sh > /dev/null

echo "[install] pushing binary + scripts + init.rc to /system..."
"${A[@]}" push target/aarch64-linux-android/debug/aabox-aapd /system/bin/aabox-aapd
"${A[@]}" push tools/aabox-aapd-watchdog-systembin.sh        /system/bin/aabox-aapd-watchdog.sh
"${A[@]}" push tools/aabox-aapd.rc                            /system/etc/init/aabox-aapd.rc

echo "[install] setting permissions + selinux labels..."
"${A[@]}" shell "chmod 755 /system/bin/aabox-aapd /system/bin/aabox-aapd-watchdog.sh"
"${A[@]}" shell "chmod 644 /system/etc/init/aabox-aapd.rc"
"${A[@]}" shell "restorecon -F /system/bin/aabox-aapd /system/bin/aabox-aapd-watchdog.sh /system/etc/init/aabox-aapd.rc || true"

echo "[install] killing any current aabox-aapd processes..."
"${A[@]}" shell "pidof aabox-aapd && pkill -9 -f aabox-aapd; pidof aabox-aapd-watchdog && pkill -9 -f aabox-aapd-watchdog; true" >/dev/null 2>&1 || true

echo "[install] flush + remount /system ro..."
"${A[@]}" shell "sync; mount -o ro,remount /"

echo "[install] reloading init parser (or fall back to reboot)..."
# Android's init re-reads /system/etc/init/*.rc on startup. For an immediate
# trigger without reboot, write to ctl.start property. Init also reloads .rc
# files when "load_persist_props_action" fires, which we don't have here.
# Easiest: reboot.

echo
echo "[install] OK. To activate the auto-start: reboot the CM5:"
echo "    adb -s $CM5_ADDR shell reboot"
echo
echo "After reboot, daemon should auto-start. Verify with:"
echo "    adb -s <NEW_PORT_AFTER_REBOOT> shell 'cat /data/local/tmp/aabox-aapd.status; ps -A | grep aabox'"

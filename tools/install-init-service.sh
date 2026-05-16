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

# Use `su 0` for individual commands. Keeping the adb shell unprivileged
# avoids the wireless-TLS rotation that `adb root` causes; userdebug builds
# expose `su 0 CMD` for escalation per-command.
SU=(su 0)

# Sanity check: ensure `su 0` works.
ROOT_ID=$("${A[@]}" shell "su 0 id")
echo "[install] su 0 id: $ROOT_ID"
[[ "$ROOT_ID" =~ uid=0 ]] || { echo "ERROR: \`su 0\` failed. Are you on a userdebug build?"; exit 1; }

echo "[install] remounting / rw..."
"${A[@]}" shell "su 0 mount -o rw,remount /"

echo "[install] cross-compiling latest daemon..."
./tools/build-android.sh > /dev/null

echo "[install] pushing binary + scripts to /data/local/tmp/ (adb daemon is uid=shell)..."
"${A[@]}" push target/aarch64-linux-android/debug/aabox-aapd /data/local/tmp/aabox-aapd
"${A[@]}" push tools/aabox-aapd-watchdog-systembin.sh        /data/local/tmp/aabox-aapd-watchdog.sh
"${A[@]}" push tools/aabox-aapd.rc                            /data/local/tmp/aabox-aapd.rc

echo "[install] su 0 copy to /system, set perms + selinux labels..."
"${A[@]}" shell "su 0 sh -c '
    cp /data/local/tmp/aabox-aapd            /system/bin/aabox-aapd                    &&
    cp /data/local/tmp/aabox-aapd-watchdog.sh /system/bin/aabox-aapd-watchdog.sh       &&
    mkdir -p /system/etc/init                                                          &&
    cp /data/local/tmp/aabox-aapd.rc          /system/etc/init/aabox-aapd.rc           &&
    chmod 755 /system/bin/aabox-aapd /system/bin/aabox-aapd-watchdog.sh                 &&
    chmod 644 /system/etc/init/aabox-aapd.rc                                            &&
    restorecon -F /system/bin/aabox-aapd /system/bin/aabox-aapd-watchdog.sh /system/etc/init/aabox-aapd.rc 2>/dev/null
    echo install-step OK'"

echo "[install] killing any current aabox-aapd processes..."
"${A[@]}" shell "su 0 sh -c 'pidof aabox-aapd && pkill -9 -f aabox-aapd; pidof aabox-aapd-watchdog && pkill -9 -f aabox-aapd-watchdog; true'" >/dev/null 2>&1 || true

echo "[install] flush + remount /system ro..."
"${A[@]}" shell "su 0 sh -c 'sync; mount -o ro,remount /'"

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

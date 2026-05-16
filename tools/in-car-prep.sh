#!/usr/bin/env bash
# One-shot prep before driving to the car.
#
# Run from the build VM with the CM5 reachable over wireless ADB at $CM5_ADDR.
# Pushes the latest cross-compiled daemon, installs an autostart shim that
# restarts the daemon on boot, and clears any old log so the in-car capture
# starts fresh.
#
# Usage:
#   CM5_ADDR=172.16.10.111:42925 tools/in-car-prep.sh
#
# After running, you can disconnect ADB and drive to the car. On boot the CM5
# will auto-start aabox-aapd in usb-run mode; when the car plugs in, the
# kernel's f_accessory driver handles AOAv2 and the daemon catches the open.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${CM5_ADDR:?Set CM5_ADDR=ip:port (from Wireless Debugging on the CM5)}"

PATH="/aosp/aosp/out/host/linux-x86/bin:$PATH"
[[ -f "${HOME}/.cargo/env" ]] && source "${HOME}/.cargo/env"

A=(adb -s "$CM5_ADDR")

echo "[in-car-prep] cross-compiling aabox-aapd for aarch64-linux-android..."
./tools/build-android.sh > /dev/null

BIN=target/aarch64-linux-android/debug/aabox-aapd
[[ -f "$BIN" ]] || { echo "missing $BIN"; exit 1; }

echo "[in-car-prep] pushing binary + helper scripts..."
"${A[@]}" push "$BIN"                          /data/local/tmp/ >/dev/null
"${A[@]}" push tools/aabox-aapd-watchdog.sh    /data/local/tmp/ >/dev/null
"${A[@]}" shell "chmod 755 /data/local/tmp/aabox-aapd /data/local/tmp/aabox-aapd-watchdog.sh"

echo "[in-car-prep] clearing previous logs..."
"${A[@]}" shell "rm -f /data/local/tmp/aabox-aapd.log /data/local/tmp/aabox-aapd.status"

echo "[in-car-prep] killing any running daemon..."
"${A[@]}" shell "pkill -f aabox-aapd 2>/dev/null; pkill -f aabox-aapd-watchdog 2>/dev/null; true"

echo "[in-car-prep] starting watchdog (which starts the daemon and respawns it on crash)..."
"${A[@]}" shell "nohup /data/local/tmp/aabox-aapd-watchdog.sh > /data/local/tmp/watchdog.log 2>&1 < /dev/null &"

sleep 1

echo
echo "[in-car-prep] OK. Current daemon status:"
"${A[@]}" shell "cat /data/local/tmp/aabox-aapd.status 2>/dev/null; echo; ps -A | grep -E 'aabox-aapd|watchdog' | grep -v grep"
echo
echo "[in-car-prep] Daemon is now running in usb-run mode, ready for car connection."
echo "[in-car-prep] To check after the test: tools/in-car-pull-logs.sh"
echo "[in-car-prep] You can disconnect ADB and drive to the car now."

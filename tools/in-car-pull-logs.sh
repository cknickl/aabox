#!/usr/bin/env bash
# After the in-car test, run this from the build VM (CM5 reconnected over
# wireless ADB) to pull the captured logs.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${CM5_ADDR:?Set CM5_ADDR=ip:port}"
PATH="/aosp/aosp/out/host/linux-x86/bin:$PATH"

A=(adb -s "$CM5_ADDR")

STAMP=$(date +%Y%m%d_%H%M%S)
DEST="captures/in-car-${STAMP}"
mkdir -p "$DEST"

echo "[pull-logs] pulling daemon log + status..."
"${A[@]}" pull /data/local/tmp/aabox-aapd.log    "$DEST/aabox-aapd.log"   || true
"${A[@]}" pull /data/local/tmp/aabox-aapd.status "$DEST/aabox-aapd.status" || true
"${A[@]}" pull /data/local/tmp/watchdog.log      "$DEST/watchdog.log"     || true

echo "[pull-logs] dropping kernel ring buffer (last 1000 lines)..."
"${A[@]}" shell "dmesg 2>/dev/null | tail -1000" > "$DEST/dmesg.txt" 2>&1 || true
"${A[@]}" shell "logcat -d -t 2000 2>&1 | grep -iE 'usb_accessory|f_accessory|aoa|gadget'" > "$DEST/logcat-usb.txt" 2>&1 || true

echo
echo "[pull-logs] Logs saved under $DEST/"
ls -lh "$DEST/"
echo
echo "[pull-logs] Current daemon status on the device:"
"${A[@]}" shell "cat /data/local/tmp/aabox-aapd.status 2>/dev/null"

#!/bin/bash
# deploy-aabox.sh — full fresh deploy of aabox-aapd to Rock 5B+ Android 12.
# Safe to re-run after any reflash. Writes persist.adb.tcp.port=5555 so
# :5555 is always the connection point after the first reboot.
#
# Usage:
#   ./deploy-aabox.sh                        # connect to 172.16.10.137:5555
#   ./deploy-aabox.sh 172.16.10.137:5555
#   ./deploy-aabox.sh 172.16.10.137:42035 585477   # pair first, then deploy

set -e

AABOX_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="${AABOX_DIR}/target/aarch64-linux-android/debug/aabox-aapd"
ADB_BIN="${ADB_BIN:-adb}"
DEVICE="${1:-172.16.10.137:5555}"
PAIR_CODE="$2"

# ── Step 0: build ──────────────────────────────────────────────────────────────
echo "=== Building aabox-aapd ==="
bash "${AABOX_DIR}/tools/build-android.sh"

# ── Step 1: pair (if pairing code given) ──────────────────────────────────────
if [[ -n "$PAIR_CODE" ]]; then
    echo "=== Pairing with code $PAIR_CODE ==="
    $ADB_BIN pair "$DEVICE" "$PAIR_CODE"
    echo "Discovering connection port via mDNS..."
    sleep 2
    MDNS_PORT=$($ADB_BIN mdns services 2>&1 | grep '_adb-tls-connect' | awk '{print $3}' | head -1 | cut -d: -f2)
    if [[ -n "$MDNS_PORT" ]]; then
        DEVICE="172.16.10.137:${MDNS_PORT}"
        echo "Connecting to $DEVICE"
        $ADB_BIN connect "$DEVICE"
    else
        echo "ERROR: could not discover connection port via mDNS"; exit 1
    fi
fi

A="$ADB_BIN -s $DEVICE"

# ── Step 2: root + remount ─────────────────────────────────────────────────────
echo "=== Rooting and remounting ==="
$A root
sleep 3
$ADB_BIN connect "$DEVICE"
A="$ADB_BIN -s $DEVICE"
$A remount

# ── Step 3: persist ADB on :5555 ──────────────────────────────────────────────
echo "=== Setting persistent ADB port 5555 ==="
$A shell "grep -q 'persist.adb.tcp.port' /vendor/build.prop || echo 'persist.adb.tcp.port=5555' >> /vendor/build.prop"
$A shell "grep -q 'service.adb.tcp.port'  /vendor/build.prop || echo 'service.adb.tcp.port=5555'  >> /vendor/build.prop"

# ── Step 4: push daemon ────────────────────────────────────────────────────────
echo "=== Pushing aabox-aapd ==="
$A push "$BINARY" /system/bin/aabox-aapd
$A shell chmod 755 /system/bin/aabox-aapd

# ── Step 5: push init RC files + bind script ─────────────────────────────────
echo "=== Pushing init RC files and bind script ==="
$A push "${AABOX_DIR}/tools/aabox-aapd.rc"     /vendor/etc/init/aabox-aapd.rc
$A push "${AABOX_DIR}/tools/aabox-usb.rc"      /vendor/etc/init/aabox-usb.rc
$A push "${AABOX_DIR}/tools/aabox-usb-bind.sh" /system/bin/aabox-usb-bind.sh
$A shell chmod 755 /system/bin/aabox-usb-bind.sh

# ── Step 6: reboot so persist.adb.tcp.port and init RCs take effect ───────────
echo "=== Rebooting ==="
$A shell "setprop sys.powerctl reboot,force"

echo ""
echo "Waiting for device on :5555..."
until $ADB_BIN connect 172.16.10.137:5555 2>&1 | grep -q "connected"; do sleep 5; done
echo ""
echo "=== Done. Device reachable at 172.16.10.137:5555 ==="
echo "aabox-aapd will auto-start via init (class main)."

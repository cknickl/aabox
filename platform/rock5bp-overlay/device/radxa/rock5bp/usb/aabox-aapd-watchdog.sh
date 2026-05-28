#!/system/bin/sh
# /system/bin/aabox-aapd-watchdog.sh — invoked by init.rc as root at boot.
# Runs the daemon in usb-run mode and restarts it on exit. Persists logs.
set -u

DAEMON=/system/bin/aabox-aapd
LOG=/data/local/tmp/aabox-aapd.log
STATUS=/data/local/tmp/aabox-aapd.status
WD_LOG=/data/local/tmp/watchdog.log

echo "[watchdog] init service start at $(date)" >> "$WD_LOG"

while true; do
    echo "[watchdog] starting aabox-aapd usb-run at $(date)" >> "$LOG"
    RUST_LOG=debug "$DAEMON" --log-file "$LOG" --status-file "$STATUS" usb-run \
        || echo "[watchdog] daemon exited at $(date) with status $?" >> "$LOG"
    sleep 2
done

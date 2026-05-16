#!/system/bin/sh
# Watchdog wrapper for aabox-aapd. Runs the daemon in usb-run mode and
# restarts it if it exits — so the user can plug into the car, unplug,
# plug into a different port, etc. without manual intervention.
#
# Persistent across reboots ONLY if invoked from init.rc — for the in-car
# test today we launch this via `nohup` from the prep script, which keeps
# the process alive across the ADB disconnect (but not across a CM5 reboot,
# which we don't expect during a single car drive).
DAEMON=/data/local/tmp/aabox-aapd
LOG=/data/local/tmp/aabox-aapd.log
STATUS=/data/local/tmp/aabox-aapd.status

while true; do
    echo "[watchdog] starting aabox-aapd usb-run at $(date)" >> "$LOG"
    RUST_LOG=debug "$DAEMON" --log-file "$LOG" --status-file "$STATUS" usb-run \
        || echo "[watchdog] daemon exited at $(date) with status $?" >> "$LOG"
    sleep 2
done

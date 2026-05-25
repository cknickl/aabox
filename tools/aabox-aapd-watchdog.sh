#!/system/bin/sh
# Watchdog wrapper for aabox-aapd. Managed by init's aabox_aapd service.
#
# Two responsibilities:
#   1. Ensure the USB gadget is in accessory.gs2 mode (run aabox-usb-bind.sh
#      if function0 isn't already pointing at accessory.gs2). This survives
#      adbd being killed because init owns this script's lifecycle, not adbd.
#   2. Loop the daemon — if it crashes (e.g., car disconnect), restart it.

DAEMON=/system/bin/aabox-aapd
LOG=/data/local/tmp/aabox-aapd.log
STATUS=/data/local/tmp/aabox-aapd.status

echo "" >> "$LOG"
# Bump the system clock to within the live Carlinkit Client.crt validity
# window. The cert is the qcache-pulled one (notAfter Aug 5 2026), which is
# the renewed cert Carlinkit actually uses at runtime (not the expired
# /system/etc/Client.crt). Window: 2014-07-04 to 2026-08-05.
#
# Without an RTC battery on this Rock 5B+, every cold boot resets the
# clock to the kernel's CONFIG_RTC_HCTOSYS default (~2021-01-01). Only
# bump if the clock is clearly wrong.
CURRENT_YEAR=$(date +%Y)
if [ "$CURRENT_YEAR" -lt 2024 ] 2>/dev/null; then
    # 2026-06-01 is safely inside the cert window and recent
    date -u 060112002026.00 2>/dev/null
    echo "[watchdog] bumped system date to $(date) (was pre-2024)" >> "$LOG"
fi

echo "==== watchdog started at $(date) uid=$(id -u) ====" >> "$LOG"
# Forensic: log uptime + boot_id so we can tell across watchdog restarts whether
# the kernel itself rebooted (boot_id changes per boot, uptime resets) vs just
# our userspace process being restarted.
echo "[watchdog] uptime=$(cat /proc/uptime 2>/dev/null) boot_id=$(cat /proc/sys/kernel/random/boot_id 2>/dev/null)" >> "$LOG"

# Enable kernel debug logging for f_accessory so dmesg shows when the host
# sends AOAv2 vendor requests (51 GetProtocol, 52 SendString, 53 Start) and
# when ACCESSORY=START is fired. Idempotent; safe to re-run.
if [ -w /sys/kernel/debug/dynamic_debug/control ]; then
    echo 'file drivers/usb/gadget/function/f_accessory.c +p' \
        > /sys/kernel/debug/dynamic_debug/control 2>/dev/null \
        && echo "[watchdog] enabled f_accessory dynamic_debug" >> "$LOG" \
        || echo "[watchdog] dynamic_debug enable failed (non-fatal)" >> "$LOG"
fi

# Always run the bind script — it's idempotent and we want descriptor changes
# (idVendor/idProduct/strings) to be picked up on every restart. The bind
# script unbinds the UDC, swaps function0 to accessory.gs2 (a no-op if
# already there), rewrites the descriptor strings, and rebinds.
echo "[watchdog] running aabox-usb-bind.sh (was function0=$(readlink /config/usb_gadget/g1/configs/b.1/function0))" >> "$LOG"
/system/bin/aabox-usb-bind.sh
sleep 3
echo "[watchdog] function0 = $(readlink /config/usb_gadget/g1/configs/b.1/function0)" >> "$LOG"
echo "[watchdog] /dev/usb_accessory = $(ls -la /dev/usb_accessory 2>/dev/null || echo missing)" >> "$LOG"

FIRST_LOOP=1
while true; do
    # Re-bump only if the clock somehow drifted to a wildly wrong value.
    # With the qcache cert (notAfter Aug 5 2026), present-day clocks should
    # be fine.
    CURRENT_YEAR=$(date +%Y)
    if [ "$CURRENT_YEAR" -lt 2024 ] 2>/dev/null; then
        date -u 060112002026.00 2>/dev/null
        echo "[watchdog] re-bumped clock to $(date) before daemon launch" >> "$LOG"
    fi

    # Re-bind the USB gadget on every iteration except the first (which the
    # pre-loop bind script already handled). Without this, the gadget keeps
    # whatever state it had after the previous KIA disconnect — empirically,
    # after 2 failed TLS cycles the accessory.gs2 function gets wedged and KIA
    # stops sending VersionRequest until we power-cycle the box. Forcing a fresh
    # UDC unbind/rebind here simulates that power-cycle from the gadget side.
    if [ "$FIRST_LOOP" -eq 0 ]; then
        echo "[watchdog] re-running aabox-usb-bind.sh to reset gadget for retry" >> "$LOG"
        /system/bin/aabox-usb-bind.sh
        sleep 2
    fi
    FIRST_LOOP=0

    echo "[watchdog] starting aabox-aapd usb-run at $(date)" >> "$LOG"
    RUST_LOG=debug "$DAEMON" --log-file "$LOG" --status-file "$STATUS" usb-run \
        || echo "[watchdog] daemon exited at $(date) with status $?" >> "$LOG"
    sleep 2
done

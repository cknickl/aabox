#!/system/bin/sh
# go-car.sh — set up for KIA Carnival test from a single adb-shell invocation.
#
# The aabox-usb-bind.sh that we call runs `stop adbd` mid-script, which kills
# the parent adb shell session and would normally take this script down with
# it. Must be invoked via `nohup setsid` so it survives.
#
# Usage from Mac:
#   adb push go-car.sh /data/local/tmp/go-car.sh
#   adb shell chmod 755 /data/local/tmp/go-car.sh
#   adb shell 'nohup setsid /data/local/tmp/go-car.sh </dev/null >/dev/null 2>&1 &'
#
# Then unplug and walk to car. After test, recover ADB via HDMI + toggle USB
# Debugging, then `adb pull /data/local/tmp/aabox-kia.log`.
# (Rock 5B+ AOSP image has no /sdcard mount yet, so logs go to /data/local/tmp.)

exec >> /data/local/tmp/go-car.log 2>&1
echo
echo "==== go-car.sh starting at $(date) ===="

stop aabox_aapd
killall aabox-aapd 2>/dev/null
killall -9 aabox-aapd-watchdog.sh 2>/dev/null
sleep 1

rm -f /data/local/tmp/aabox-kia.log

echo "--- running aabox-usb-bind.sh ---"
/system/bin/aabox-usb-bind.sh
echo "bind script returned rc=$?"

sleep 3

echo "--- function0 after bind ---"
readlink /config/usb_gadget/g1/configs/b.1/function0
echo "--- udc state ---"
cat /sys/class/udc/*/state 2>/dev/null

echo "--- starting daemon at $(date) ---"
RUST_LOG=debug exec /system/bin/aabox-aapd --log-file /data/local/tmp/aabox-kia.log usb-run

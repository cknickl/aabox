#!/system/bin/sh
# aabox-usb-bind.sh — configure USB gadget at boot for AOAv2/Android Auto.
#
# Triggered by `on property:sys.usb.ffs.ready=1` (or fallback start at boot).
# By the time sys.usb.ffs.ready=1 fires, dwc2 has registered the UDC and the
# Pi vendor init.rpi5.usb.rc has set up /config/usb_gadget/g1 and mounted
# functionfs. We swap function0 → accessory.gs2 and bind UDC ourselves.
#
# Why: vendor.usb_gadget-rpi HAL relies on sys.usb.configfs=1 triggers that
# the Pi vendor init never satisfies (sets configfs=2). Result on a fresh
# boot: g1 is set up but never bound, USB-C looks dead. See memory
# project-cm5-usb-gadget for full background.

set -u
LOG=/data/local/tmp/aabox-usb-bind.log
exec >> "$LOG" 2>&1
echo
echo "==== aabox-usb-bind at $(date) ===="

GADGET=/config/usb_gadget/g1

# UDC name is board-specific. CM5/Pi 5 uses dwc2 at "1000480000.usb"; RK3588
# uses DWC3 at "fc000000.usb"; other boards differ. Auto-detect by enumerating
# /sys/class/udc/ — there's only ever one entry on these single-OTG boards.
# Fall back to sys.usb.controller property if /sys/class/udc/ is empty at
# script start (race against driver probe).
UDC_NAME=""

# Wait for everything we need to be in place:
#   1. /config/usb_gadget/g1/configs/b.1 (vendor init `on boot`)
#   2. /sys/class/udc/<entry>             (OTG driver probe complete)
i=0
while [ "$i" -lt 60 ]; do
    if [ -d "$GADGET/configs/b.1" ]; then
        # Pick the first UDC node that appeared.
        for u in /sys/class/udc/*; do
            [ -d "$u" ] && UDC_NAME=$(basename "$u") && break
        done
        # If sysfs didn't give us a name yet, fall back to the property the
        # Android USB HAL sets when it knows the controller node.
        if [ -z "$UDC_NAME" ]; then
            UDC_NAME=$(getprop sys.usb.controller 2>/dev/null)
        fi
        if [ -n "$UDC_NAME" ] && [ -d "/sys/class/udc/$UDC_NAME" ]; then
            echo "[wait] all prereqs present after ${i}s; UDC=$UDC_NAME"
            break
        fi
    fi
    sleep 1
    i=$((i+1))
done

UDC_NODE=/sys/class/udc/$UDC_NAME

if [ -z "$UDC_NAME" ] || [ ! -d "$GADGET/configs/b.1" ] || [ ! -d "$UDC_NODE" ]; then
    echo "ERROR: prereqs missing after 60s"
    echo "  UDC_NAME (auto-detected): '$UDC_NAME'"
    echo "  configs/b.1 exists:        $([ -d "$GADGET/configs/b.1" ] && echo yes || echo no)"
    echo "  $UDC_NODE exists:          $([ -d "$UDC_NODE" ] && echo yes || echo no)"
    echo "  /sys/class/udc/ contents:  $(ls /sys/class/udc/ 2>/dev/null)"
    exit 1
fi

cd "$GADGET" || exit 1
echo "BEFORE: UDC=$(cat UDC 2>/dev/null), function0=$(readlink configs/b.1/function0 2>/dev/null)"

# Swap function0 to accessory.gs2. Removing the symlink fails silently if
# there's nothing to remove; that's fine.
rm -f configs/b.1/function0
ln -s "$GADGET/functions/accessory.gs2" configs/b.1/function0

# IMPORTANT: car/AA head units sniff device descriptors before deciding
# whether to enumerate USB or to ignore the device as "not a phone." We need:
#   - idProduct in the Pixel/Nexus range, not bare ADB-class
#   - manufacturer/product strings that look like a real Android phone
# The kernel's f_accessory function (linked into configs/b.1 via function0
# above) intercepts AOAv2 ep0 vendor requests because it's in the active
# config. When the car sends Accessory_Start (0x53), f_accessory materialises
# /dev/usb_accessory and the daemon unblocks.
#
# Empirical findings (Kia Carnival, 2026-05-17):
#   - idProduct=0x2D00 + manufacturer=Raspberry → "Reading USB", no AA popup,
#     no USB Reset from car (car treats us as "already-an-accessory-without-
#     session" and bails)
#   - idProduct=0x4EE7 + manufacturer=Raspberry → same (no enumeration at all
#     — car appears not to recognise "Raspberry/Pi 5" as a phone)
# Try idProduct=0x4EE2 (Nexus phone, on most AA whitelists) + Google/Pixel
# strings next.
echo 0x4ee2 > idProduct
echo "Google" > strings/0x409/manufacturer
echo "Pixel" > strings/0x409/product
echo "Android Accessory" > configs/b.1/strings/0x409/configuration

# Bind. accessory.gs2 has kernel-side descriptors so no userspace prep
# (no adbd handshake) is needed.
echo "$UDC_NAME" > UDC 2>&1
RC=$?
echo "UDC write rc=$RC"

sleep 1
echo "AFTER:  UDC=$(cat UDC 2>/dev/null), udc_state=$(cat $UDC_NODE/state 2>/dev/null), function=$(cat $UDC_NODE/function 2>/dev/null)"
echo "==== done ===="

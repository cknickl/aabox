#!/bin/sh
# AABox USB gadget bring-up: creates the 'default' and 'accessory' ConfigFS
# gadgets used by aabox-aapd. Run once at boot (init.rc on Android, systemd
# on plain Linux) BEFORE the daemon starts.
#
# Gadgets are created in a "disabled" state (UDC not bound). aabox-aapd flips
# them on/off via the UDC file at runtime.
#
# Requirements: root + a UDC visible under /sys/class/udc + configfs mounted.
set -e

# Auto-detect configfs root (Android = /config, plain Linux = /sys/kernel/config).
for c in /config/usb_gadget /sys/kernel/config/usb_gadget; do
    if [ -d "$c" ]; then
        CONFIGFS="$c"
        break
    fi
done
if [ -z "$CONFIGFS" ]; then
    echo "ERROR: no usb_gadget configfs directory found"
    exit 1
fi
echo "Using configfs root: $CONFIGFS"

# Must be root (Android has $USER unset, so check id instead).
if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: must run as root"
    exit 1
fi

# ---- 'default' gadget: minimal Android-phone-shaped USB device ---------------
mkdir -p "$CONFIGFS/default"
cd "$CONFIGFS/default"
echo 0x18D1 > idVendor                   # Google
echo 0x4E11 > idProduct                  # Generic Android-ish
echo 0x0100 > bcdDevice
echo 0x0200 > bcdUSB
mkdir -p strings/0x409
echo "AABox" > strings/0x409/manufacturer
echo "AABox CM5" > strings/0x409/product
echo "AABOX0001" > strings/0x409/serialnumber
mkdir -p configs/c.1/strings/0x409
echo "AABox default config" > configs/c.1/strings/0x409/configuration
echo 250 > configs/c.1/MaxPower
mkdir -p functions/acm.gs0
if [ ! -L configs/c.1/acm.gs0 ]; then
    ln -s functions/acm.gs0 configs/c.1/acm.gs0
fi

# ---- 'accessory' gadget: VID/PID 0x18D1:0x2D00 + f_accessory function -------
mkdir -p "$CONFIGFS/accessory"
cd "$CONFIGFS/accessory"
echo 0x18D1 > idVendor                   # Google
echo 0x2D00 > idProduct                  # Android Accessory (no-ADB variant)
echo 0x0100 > bcdDevice
echo 0x0200 > bcdUSB
mkdir -p strings/0x409
echo "AABox" > strings/0x409/manufacturer
echo "AABox CM5" > strings/0x409/product
echo "AABOX0001" > strings/0x409/serialnumber
mkdir -p configs/c.1/strings/0x409
echo "AABox accessory config" > configs/c.1/strings/0x409/configuration
echo 250 > configs/c.1/MaxPower
mkdir -p functions/accessory.gs0
if [ ! -L configs/c.1/accessory.gs0 ]; then
    ln -s functions/accessory.gs0 configs/c.1/accessory.gs0
fi

echo "[usb-gadget-setup] OK — gadgets staged under $CONFIGFS"
echo "[usb-gadget-setup] Available UDCs: $(ls /sys/class/udc 2>/dev/null)"

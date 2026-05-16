#!/usr/bin/env bash
# AABox USB gadget bring-up: creates the 'default' and 'accessory' ConfigFS
# gadgets used by aabox-aapd. Run once at boot (init.rc on Android, systemd
# on plain Linux) BEFORE the daemon starts.
#
# The gadgets are created in a "disabled" state (UDC not bound). aabox-aapd
# flips them on/off via the UDC file at runtime.
#
# Requires:
#   - Kernel built with CONFIG_USB_F_ACCESSORY=y (or =m, modprobed first)
#   - Kernel built with CONFIG_USB_CONFIGFS=y + CONFIG_USB_CONFIGFS_F_ACC=y
#   - Kernel built with CONFIG_USB_CONFIGFS_SERIAL=y (for the 'default' gadget's
#     ACM function — change function below if your kernel uses MTP or mass-storage)
#   - configfs mounted at /sys/kernel/config (Android init.rc usually does this)
#   - root (this script must be run as root)
set -euo pipefail

CONFIGFS=/sys/kernel/config/usb_gadget
[[ -d "$CONFIGFS" ]] || { echo "ERROR: $CONFIGFS missing — is configfs mounted? is the kernel built with CONFIG_USB_CONFIGFS?"; exit 1; }
[[ $EUID -eq 0 ]] || { echo "ERROR: must run as root"; exit 1; }

# ---- 'default' gadget: minimal Android-phone-shaped USB device ---------------
#
# The car (AOAv2 host) tries ACCESSORY_GET_PROTOCOL on any USB device. We need
# to be enumerable as a regular USB device so the car gets that far. We use
# ACM (serial) here because it's the smallest useful default. Mass-storage or
# MTP would also work; pick whatever your kernel config has.
mkdir -p "$CONFIGFS/default"
cd "$CONFIGFS/default"
echo 0x18D1 > idVendor                   # Google
echo 0x4E11 > idProduct                  # Generic Android device-ish
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
# Link function into config (idempotent — symlink may already exist)
[[ -L configs/c.1/acm.gs0 ]] || ln -s functions/acm.gs0 configs/c.1/acm.gs0
# Leave UDC unbound — aabox-aapd will write it at runtime.

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
[[ -L configs/c.1/accessory.gs0 ]] || ln -s functions/accessory.gs0 configs/c.1/accessory.gs0
# Leave UDC unbound — aabox-aapd flips this when ACCESSORY=START arrives.

echo "[usb-gadget-setup] OK — gadgets staged under $CONFIGFS. Available UDCs:"
ls /sys/class/udc

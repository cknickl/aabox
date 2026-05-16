#!/usr/bin/env bash
# Diagnostic — run on the CM5 (adb shell) to verify the kernel + device tree
# support everything aabox-aapd needs. Read-only; safe to run any time.
set -u

echo "=== USB gadget config in running kernel ==="
if [[ -r /proc/config.gz ]]; then
    zcat /proc/config.gz | grep -E \
        "CONFIG_USB_GADGET=|CONFIG_USB_CONFIGFS=|CONFIG_USB_LIBCOMPOSITE=|CONFIG_USB_F_FS=|CONFIG_ANDROID_USB_F_ACC=|CONFIG_USB_CONFIGFS_F_FS=|CONFIG_ANDROID_USB_CONFIGFS_F_ACC=|CONFIG_USB_DWC2_|CONFIG_USB_DWC3_" \
        | sort
else
    echo "(no /proc/config.gz — kernel built without IKCONFIG_PROC)"
fi

echo
echo "=== configfs mounted? ==="
mount | grep -E "configfs|/sys/kernel/config" || echo "NOT MOUNTED (mount -t configfs none /sys/kernel/config)"

echo
echo "=== UDCs visible to kernel ==="
if [[ -d /sys/class/udc ]]; then
    ls /sys/class/udc
    [[ -z "$(ls /sys/class/udc)" ]] && echo "(empty — DWC2/3 not in peripheral mode? check dtoverlay)"
else
    echo "/sys/class/udc missing"
fi

echo
echo "=== DWC mode hints ==="
for f in /sys/bus/platform/devices/*usb*/dr_mode \
         /sys/bus/platform/drivers/dwc2/*/dr_mode \
         /sys/bus/platform/drivers/dwc3-of-simple/*/dr_mode; do
    [[ -r "$f" ]] && echo "$f: $(cat "$f")"
done

echo
echo "=== existing aabox gadgets staged in configfs? ==="
for g in default accessory; do
    p=/sys/kernel/config/usb_gadget/$g
    if [[ -d $p ]]; then
        echo "$g: present (UDC=$(cat $p/UDC 2>/dev/null), iVendor=$(cat $p/idVendor 2>/dev/null), iProduct=$(cat $p/idProduct 2>/dev/null))"
    else
        echo "$g: not staged — run sudo tools/usb-gadget-setup.sh"
    fi
done

echo
echo "=== f_accessory char device present? ==="
ls -la /dev/usb_accessory 2>&1 || echo "(only appears after the gadget is enabled in accessory mode)"

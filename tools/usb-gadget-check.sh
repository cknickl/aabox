#!/bin/sh
# Diagnostic — read-only. Run on the CM5 (adb shell) to verify the kernel +
# device tree support everything aabox-aapd needs.

echo "=== USB gadget config in running kernel ==="
if [ -r /proc/config.gz ]; then
    zcat /proc/config.gz | grep -E \
        "CONFIG_USB_GADGET=|CONFIG_USB_CONFIGFS=|CONFIG_USB_LIBCOMPOSITE=|CONFIG_USB_F_FS=|CONFIG_ANDROID_USB_F_ACC=|CONFIG_USB_CONFIGFS_F_FS=|CONFIG_ANDROID_USB_CONFIGFS_F_ACC=|CONFIG_USB_DWC2_|CONFIG_USB_DWC3_" \
        | sort
else
    echo "(no /proc/config.gz)"
fi

echo
echo "=== configfs mount + usb_gadget root ==="
mount | grep -E "configfs"
for c in /config/usb_gadget /sys/kernel/config/usb_gadget; do
    if [ -d "$c" ]; then
        echo "$c: present"
        CONFIGFS="$c"
    fi
done
[ -z "$CONFIGFS" ] && echo "no usb_gadget configfs directory found"

echo
echo "=== UDCs visible to kernel ==="
if [ -d /sys/class/udc ]; then
    UDCS=$(ls /sys/class/udc 2>/dev/null)
    [ -z "$UDCS" ] && echo "(empty — DWC2/3 not in peripheral mode? check dtoverlay)"
    [ -n "$UDCS" ] && echo "$UDCS"
else
    echo "/sys/class/udc missing"
fi

echo
echo "=== DWC mode hints ==="
for f in /sys/bus/platform/devices/*usb*/dr_mode; do
    [ -r "$f" ] && echo "$f: $(cat $f)"
done

echo
echo "=== aabox gadgets staged in $CONFIGFS ? ==="
for g in default accessory; do
    p="$CONFIGFS/$g"
    if [ -d "$p" ]; then
        UDC=$(cat "$p/UDC" 2>/dev/null)
        VID=$(cat "$p/idVendor" 2>/dev/null)
        PID=$(cat "$p/idProduct" 2>/dev/null)
        echo "$g: staged (UDC=$UDC, VID=$VID, PID=$PID)"
    else
        echo "$g: not staged — run: sh /data/local/tmp/usb-gadget-setup.sh"
    fi
done

echo
echo "=== f_accessory char device ==="
ls -la /dev/usb_accessory 2>&1 || echo "(only appears once accessory gadget is enabled in accessory mode)"

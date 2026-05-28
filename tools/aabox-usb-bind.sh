#!/system/bin/sh
# aabox-usb-bind.sh — configure USB gadget at boot for AOAv2/Android Auto.
#
# Triggered at sys.boot_completed=1 (after the Rockchip USB Gadget HAL has
# finished its setCurrentUsbFunctions("adb") call, which would otherwise
# overwrite an earlier bind).
#
# Sequence:
#   1. Wait for /config/usb_gadget/g1/configs/b.1 and /sys/class/udc/ to exist.
#   2. UNBIND the gadget from the UDC — required before modifying function
#      symlinks (Linux configfs returns EBUSY if the gadget is active).
#   3. Stop adbd so it releases its FunctionFS fd (prevents the 1 Hz
#      "read descriptors" spam and ensures a clean ep0 state).
#   4. Swap function0 → accessory.gs2.
#   5. Set idProduct/strings so the car's AOA probe sees a phone-like device.
#   6. Rebind the UDC.

set -u
LOG=/data/local/tmp/aabox-usb-bind.log
exec >> "$LOG" 2>&1
echo
echo "==== aabox-usb-bind at $(date) ===="

GADGET=/config/usb_gadget/g1

# UDC name is board-specific. Auto-detect from /sys/class/udc/; fall back to
# sys.usb.controller property if sysfs isn't ready yet.
UDC_NAME=""

# Wait for /config/usb_gadget/g1/configs/b.1 and /sys/class/udc/<entry>.
i=0
while [ "$i" -lt 60 ]; do
    if [ -d "$GADGET/configs/b.1" ]; then
        for u in /sys/class/udc/*; do
            [ -d "$u" ] && UDC_NAME=$(basename "$u") && break
        done
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

if [ ! -d "$GADGET/functions/accessory.gs2" ]; then
    echo "ERROR: $GADGET/functions/accessory.gs2 does not exist"
    echo "  functions/ contents: $(ls "$GADGET/functions/" 2>/dev/null)"
    exit 1
fi

cd "$GADGET" || exit 1
echo "BEFORE: UDC=$(cat UDC 2>/dev/null), function0=$(readlink configs/b.1/function0 2>/dev/null)"

# ── Step 0a: Wait for host disconnect before touching UDC ────────────────────
# CRITICAL SAFETY: unbinding the UDC while a host is actively enumerating
# (UDC state in {default, configured}) has deadlocked the kernel on Rock
# 5B+ A12 in past runs (DWC3 + Rockchip USB HAL race). State "not attached"
# means no host on VBUS — safe to manipulate the gadget. If a host is
# present we wait up to 30s; if it doesn't go away, we BAIL and leave the
# default ffs.adb-only gadget alone. Operator can re-run the bind script
# manually once they've unplugged the cable.
i=0
while [ "$i" -lt 30 ]; do
    st=$(cat "$UDC_NODE/state" 2>/dev/null)
    if [ "$st" = "not attached" ] || [ -z "$st" ]; then
        echo "[wait] UDC state=$st — safe to proceed (after ${i}s)"
        break
    fi
    if [ "$i" = 0 ]; then
        echo "[wait] UDC state=$st — host appears connected; waiting up to 30s for disconnect"
    fi
    sleep 1
    i=$((i+1))
done
st=$(cat "$UDC_NODE/state" 2>/dev/null)
if [ "$st" != "not attached" ] && [ -n "$st" ]; then
    echo "ABORT: UDC state=$st after 30s — refusing to unbind while host is enumerating."
    echo "       Unplug USB-C from Rock, then run: start aabox_usb_bind"
    echo "       (Daemon will remain idle until aabox.usb.ready=1 is set.)"
    exit 2
fi

# ── Step 0b: Park the Rockchip USB HAL ───────────────────────────────────────
# init.usb.configfs.rc only fires on sys.usb.config in {adb,mtp,ptp,rndis,…}.
# Setting it to a value that doesn't match disables the HAL's "rewrite the
# UDC every state change" actions, so our unbind/modify/rebind below proceeds
# without the HAL racing us back to ffs.adb mid-swap.
setprop sys.usb.config aabox

# ── Step 1: Unbind the gadget ────────────────────────────────────────────────
# configfs does not allow removing or adding function symlinks while the gadget
# is active — the kernel returns EBUSY. The rm -f below would silently fail
# (and ln -s would then fail with EEXIST), leaving the gadget stuck in ADB
# mode. Unbind first so the config is mutable. Safe here: Step 0a confirmed
# no host on VBUS.
echo "" > UDC
sleep 0.3

# ── Step 2: Single-function gadget = accessory.gs2 only ──────────────────────
# 2026-05-28: tried composite (ffs.adb + accessory.gs2) — the SET_CONFIG #1
# request from the host on the laptop side returned -EPROTO (-71) because
# the kernel's set_alt(0) on ffs.adb fails when adbd's ep0 handshake is
# racing the host enumeration. With ffs.adb removed and accessory.gs2 as the
# sole interface, SET_CONFIG has no FFS dependency and enumeration succeeds
# in one shot. Trade-off: USB-ADB stops working over this gadget. WiFi ADB
# (persist.adb.tcp.port=5555) keeps the device reachable regardless.
echo "BEFORE function slots:"
ls configs/b.1/ | grep -E '^(f[0-9]|function[0-9])' || echo "  (none)"

# Remove every existing function symlink slot we know about. Rockchip's
# vendor init uses "f1", "f2", …; the configfs-gadget-rs convention uses
# "function0", "function1", … Cover both so we land in a known state.
for s in f0 f1 f2 f3 function0 function1 function2 function3; do
    [ -L "configs/b.1/$s" ] && rm -f "configs/b.1/$s"
done

ln -s "$GADGET/functions/accessory.gs2" configs/b.1/f1
echo "f1 → $(readlink configs/b.1/f1 2>/dev/null)"

# NOTE: do NOT touch adbd here. Its init triggers require
# sys.usb.config=adb, which we just parked to "aabox". Restarting adbd
# in this state leaves it stopped and kills WiFi ADB (TCP listener dies
# with adbd). The "read descriptors / read strings" 1Hz kernel log spam
# from adbd's stranded FunctionFS instance is annoying but harmless —
# accept it for now.

# (No ffs.adb wait needed — single-function gadget; set_alt has no FFS
# function to wait on.)
if false; then
adbd_has_ep0() {
    ls -l /proc/*/fd/ 2>/dev/null | grep -q 'usb-ffs/adb/ep0'
}
i=0
while [ "$i" -lt 50 ]; do
    if adbd_has_ep0; then
        echo "[wait] adbd has /dev/usb-ffs/adb/ep0 open after $((i*200))ms"
        break
    fi
    # 200 ms ticks → 10 s max
    sleep 0.2
    i=$((i+1))
done
if [ "$i" -ge 50 ]; then
    echo "[warn] adbd never opened ep0 in 10s — proceeding anyway, may flap"
fi
fi  # end of `if false` guard around the disabled adbd-wait block

# ── Step 4: Set device descriptor strings ───────────────────────────────────
# IMPORTANT: car/AA head units sniff device descriptors before deciding
# whether to enumerate USB or to ignore the device as "not a phone." We need:
#   - idProduct in the Pixel/Nexus range, not bare ADB-class
#   - manufacturer/product strings that look like a real Android phone
# The kernel's f_accessory function (linked into configs/b.1 via function0
# above) intercepts AOAv2 ep0 vendor requests because it's in the active
# config. When the car sends Accessory_Start (0x53), f_accessory materialises
# /dev/usb_accessory and the daemon unblocks.
#
# Empirical findings (Kia Carnival):
#   2026-05-17 idProduct=0x2D00 + manufacturer=Raspberry → "Reading USB",
#              no AA popup
#   2026-05-17 idProduct=0x4EE7 + manufacturer=Raspberry → no enumeration
#   2026-05-21 idProduct=0x4EE2 + manufacturer=Google/Pixel 6 → KIA
#              enumerated us once, attempted AOA (CONNECT→ep0 error→
#              DISCONNECT→reconnect pattern at +2.5s), then bailed. The car
#              expected us to re-enumerate with the AOA PID after vendor
#              request 53 Accessory_Start; we didn't, so KIA gave up.
#   2026-05-22 idProduct=0x2D00 + manufacturer=Android + serial number,
#              pre-staging the *post*-Accessory_Start state. This skips the
#              mode-switch handshake KIA was expecting.
#
# AOA active-mode product IDs (post-Accessory_Start):
#   0x2D00  accessory only
#   0x2D01  accessory + ADB
#   0x2D04  accessory + audio
#   0x2D05  accessory + ADB + audio
echo 0x18d1 > idVendor
# IMPORTANT — DO NOT use 0x2D00/0x2D01 here. Those PIDs tell the host
# "I am already in AOA active mode," so the head unit SKIPS sending the
# Accessory_Start vendor request (0x53). But the kernel's f_accessory only
# sets dev->online=1 (which makes /dev/usb_accessory readable) AFTER it
# processes that request. Lying about AOA state → f_accessory stays offline →
# reads return EIO. Confirmed via 2026-05-22 logs.
#
# 0x4EE2 (Pixel 6) makes the KIA do the full AOA dance, which is exactly
# what we need to trigger f_accessory.online=1 and the ACCESSORY=START uevent.
# Combined with the composite gadget above, the KIA sees a normal-looking
# Pixel with ADB + accessory interfaces.
echo 0x4ee2 > idProduct
echo "Google" > strings/0x409/manufacturer
echo "Pixel 6" > strings/0x409/product
# Real phones populate the iSerialNumber descriptor. KIA likely fingerprints
# devices by VID/PID+serial and caches rejections — a stable serial means
# changing the PID is enough to look "new" to the car; an empty serial may
# also have been why the first probe was rejected outright.
echo "AABOX0123456789" > strings/0x409/serialnumber
echo "Android Accessory" > configs/b.1/strings/0x409/configuration

# ── Step 5: Rebind the UDC ───────────────────────────────────────────────────
echo "$UDC_NAME" > UDC
RC=$?
echo "UDC write rc=$RC"

sleep 1
echo "AFTER:  UDC=$(cat UDC 2>/dev/null), udc_state=$(cat $UDC_NODE/state 2>/dev/null), function=$(cat $UDC_NODE/function 2>/dev/null)"
echo "DESCRIPTORS:"
echo "  idVendor=$(cat idVendor 2>/dev/null)"
echo "  idProduct=$(cat idProduct 2>/dev/null)"
echo "  manufacturer=$(cat strings/0x409/manufacturer 2>/dev/null)"
echo "  product=$(cat strings/0x409/product 2>/dev/null)"
echo "  serialnumber=$(cat strings/0x409/serialnumber 2>/dev/null || echo '<node missing>')"
echo "  bcdDevice=$(cat bcdDevice 2>/dev/null)"
echo "  bDeviceClass=$(cat bDeviceClass 2>/dev/null)"

# Step 6 (start adbd) removed: with the composite gadget approach, adbd was
# never stopped, so it's still running. ffs.adb is still active in function0,
# so USB-ADB stays live across this rebind — no recovery dance needed.

# Signal that the gadget is composed and bound. aabox-aapd.rc listens for
# `on property:aabox.usb.ready=1` and starts the daemon at that edge —
# starting earlier just churns restart_period cycles since /dev/usb_accessory
# returns ENODEV until accessory.gs2 is in an active config.
if [ "$RC" = "0" ]; then
    setprop aabox.usb.ready 1
    echo "set aabox.usb.ready=1"
else
    echo "skipping aabox.usb.ready — UDC write failed (rc=$RC)"
fi

echo "==== done ===="

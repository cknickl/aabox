# Phase 2 — USB gadget + AOAv2 handshake

## What we found in the existing rpi5 kernel (good news)

Extracted from `/aosp/aosp/out/target/product/rpi5/kernel` via `IKCFG_ST`/`IKCFG_ED` markers:

| Required | Status |
|---|---|
| `CONFIG_USB_GADGET=y` | ✅ |
| `CONFIG_USB_LIBCOMPOSITE=y` | ✅ |
| `CONFIG_USB_CONFIGFS=y` | ✅ |
| `CONFIG_ANDROID_USB_F_ACC=y` (the f_accessory driver itself) | ✅ |
| `CONFIG_ANDROID_USB_CONFIGFS_F_ACC=y` (ConfigFS exposure of f_accessory) | ✅ |
| `CONFIG_USB_CONFIGFS_F_FS=y` (FunctionFS — fallback path) | ✅ |

**The kernel needs no patches.** The AOAv2 handshake itself is handled entirely by `drivers/usb/gadget/function/f_accessory.c`. Our userspace job is the ConfigFS configuration and the runtime UDC toggle.

## The remaining unknown — peripheral mode on the slave USB-C

Both USB controllers (`DWC2` for legacy USB-A, `DWC3` for the CM5 IO board slave USB-C) are built with `_DUAL_ROLE=y`, not `_PERIPHERAL=y`. That means OTG mode-switching at runtime depending on PHY pin signaling.

**Verify on the CM5**: after boot, run `tools/usb-gadget-check.sh`. If `/sys/class/udc/` is empty, the controller isn't in peripheral mode. Most common fix: append to the FAT32 `boot.img`'s `config.txt`:
```
dtoverlay=dwc2,dr_mode=peripheral
```

For the CM5 specifically the relevant overlay may differ (the BCM2712's USB-C might be on a different controller node). The check script prints `dr_mode` values for every USB platform device — that's how we tell.

## Architecture (what the daemon does on connect)

```
boot
 │  tools/usb-gadget-setup.sh (one-shot, root, init.rc or systemd)
 │     ↳ mkdir /sys/kernel/config/usb_gadget/{default,accessory}
 │     ↳ populate idVendor/idProduct/strings/configs/functions
 │     ↳ leave UDC unbound (gadgets staged but not enabled)
 ▼
aabox-aapd usb-bringup
 │  usb::gadget::UsbGadgetState::autodetect()
 │  usb::bring_up()
 │     ↳ start uevent listener thread (kobject_uevent on netlink group 1)
 │     ↳ disable_all() — clean slate
 │     ↳ enable("default")  — write UDC name to /sys/kernel/config/usb_gadget/default/UDC
 │     ↳ car USB-host enumerates us, issues ACCESSORY_GET_PROTOCOL,
 │       SEND_STRING ×6, ACCESSORY_START — all handled by the kernel
 │       f_accessory driver. We see nothing in userspace.
 │     ↳ kernel emits uevent: DEVNAME=usb_accessory ACCESSORY=START
 │     ↳ uevent thread fires Notify
 │     ↳ disable("default")
 │     ↳ sleep 500ms so the host re-enumerates cleanly
 │     ↳ enable("accessory") — gadget now exposes VID:PID 18D1:2D00 + bulk in/out
 │  usb::stream::open() — open /dev/usb_accessory rw
 ▼
ready for AAP framing (Phase 3)
```

## How to test before plugging into a real car

DHU on the Mac doesn't speak AOAv2 over USB — it expects an ADB-forwarded TCP socket. So `usb-bringup` is hardware-only. To exercise the daemon's TCP path against DHU before the USB plumbing works, use `aabox-aapd dhu 127.0.0.1:5277` (forwarded by `adb forward tcp:5277 tcp:5277`). That's Phase 3 work.

To unit-test the gadget management without a UDC, point `UsbGadgetState` at a tempdir layout that mimics `/sys/kernel/config/usb_gadget/<name>/UDC`. TODO when we add tests.

## Resources

- Kernel source: `drivers/usb/gadget/function/f_accessory.c` (Android's downstream Linux fork)
- AOAv2 spec: https://source.android.com/devices/accessories/aoa2
- ConfigFS gadget interface: `Documentation/usb/gadget_configfs.rst` in the kernel tree
- aa-proxy-rs reference: `references/aa-proxy-rs/src/usb_gadget.rs`

# Rock 5B+ AABox Overlay

Files to merge into the **Khadas Edge2 Android 14** source tree at
`/aosp/rock5bp_a14/` once `repo sync` completes. Khadas Edge2 was chosen as the
base because it's the only fully-public Android 14 source tree for an
RK3588-family board with FUSB302 + USB-C role switching. The port is
mechanical — same SoC family, same FUSB302 wiring topology, same AP6275P WiFi
chip, same RK806 PMIC.

See `/home/nicklc/.claude/projects/-home-nicklc-aabox-work/memory/project-rock5bp-migration.md`
for the decision history (why not Radxa Android 12, why not 5C, why not
FriendlyELEC).

## Layout

- `device/radxa/rock5bp/` — AABox additions that get copied into the new
  `device/radxa/rock5bplus/aabox/` subdirectory by the port script. Contents:
  - `usb/aabox-aapd.rc` — Android init service. Triggers
    `aabox-usb-bind.sh` on `sys.boot_completed=1` and starts the daemon.
  - `usb/aabox-resigner.rc` — init service for the CarCar re-signer.
  - `usb/aabox-usb-bind.sh` — UDC-agnostic bind script (auto-detects
    `/sys/class/udc/*` — works on RK3588's `fc000000.usb` same as CM5's
    `1000480000.usb`).
  - `usb/aabox-aapd-watchdog.sh` — restart helper.
  - `aabox-product.mk` — product makefile fragment. Adds the AABox property
    overrides and PRODUCT_COPY_FILES.

- `vendor/aabox-keys/` — placeholder (the actual keys live at
  `/aosp/aosp/vendor/aabox-keys/`; the port script symlinks them into the
  Edge2 tree so apps signed under the AABox key on CM5 continue to validate).

- `scripts/port-to-rock5bplus.sh` — idempotent porting script. Run after
  `repo sync` completes. Generates `rk3588-rock-5b-plus.dts` from the Edge2
  DTS, creates `device/radxa/rock5bplus/`, merges the AABox overlay, and
  symlinks the keys.

- `reference/` — Reference DTS files used during the port design (5B+ Linux
  kernel 5.10 DTS for the FUSB302 wiring + Edge2 6.1 DTS for diff).

- `POST_SYNC_STEPS.md` — Manual step-by-step procedure (the script automates
  steps 3a-3d). Read this to understand what the script does, or as a fallback
  if the script fails.

## What auto-ports vs needs hardware-specific tweaks

All AABox software auto-ports — the daemon, channels, resigner, init scripts,
UDC-agnostic bind, vendor.prop `persist.adb.tcp.port=5555` — none of it knows
about board specifics.

The Rock-5B+-specific work is in `scripts/port-to-rock5bplus.sh`:
1. Generate `rk3588-rock-5b-plus.dts` (one-line FUSB302 GPIO change vs Edge2)
2. Create `device/radxa/rock5bplus/` product directory (cloned from
   `device/khadas/edge2/`)
3. Update lunch target name, model/compatible strings, DTB path
4. Hook the AABox `aabox-product.mk` into the product

After the port script, the build is:
```
cd /aosp/rock5bp_a14
source build/envsetup.sh
lunch RadxaRock5BPlus-userdebug
m -j$(nproc)
```

## Notable platform deltas vs CM5

| Aspect | CM5 / Pi 5 | Rock 5B+ |
|---|---|---|
| OTG UDC node | `1000480000.usb` (dwc2) | `fc000000.usb` (DWC3) |
| USB-C CC controller | none (bare SoC CC) | **FUSB302 on I²C** ← the whole reason we migrated |
| Boot config | `config.txt` (Pi firmware) | `extlinux.conf` / Rockchip MiniLoader |
| Kernel | raspberry-vanilla 6.12 | Rockchip 6.1 BSP (Khadas Edge2 fork) |
| ROM build | raspberry-vanilla AOSP 16 | Khadas Edge2 AOSP 14 (with Rock 5B+ DTS port) |
| Build tool | `m` (Soong) | `m` (Soong) — same |
| Cross-compile target | `aarch64-linux-android` | `aarch64-linux-android` — same |

## Notable code that already handles the migration

- `tools/aabox-usb-bind.sh` (CM5 + Rock 5B+) — already UDC-agnostic via
  `/sys/class/udc/*` enumeration.
- `crates/aabox-aapd/` — no kernel-specific code. Talks to `/dev/usb_accessory`
  via Linux ioctls that are universal.
- `crates/aabox-resigner/` — pure userspace, no platform-specific code.
- `device/radxa/rock5bp/aabox-product.mk` — vendor.prop override fragment.
  Inherited from the main product makefile.

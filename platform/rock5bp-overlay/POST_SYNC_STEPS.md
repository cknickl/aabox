# Post-sync activation steps — Khadas Edge2 Android 14 base

After `repo sync` completes at `/aosp/rock5bp_a14/` (log at `/aosp/rock5bp_a14/sync.log`).

The chosen base is Khadas Edge2 Android 14 — the only fully-public Android 14
source tree for an RK3588-family board with FUSB302 + USB-C role switching.
The port to Rock 5B+ is mechanical: same SoC family (RK3588 vs RK3588S — full
vs cut-down), same FUSB302 USB-C wiring pattern, same AP6275P WiFi chip, same
RK806 PMIC.

## Step 1 — Verify sync ended cleanly

```bash
cd /aosp/rock5bp_a14
tail -50 sync.log
du -sh .                                       # expect 200-220 GB
# Note: Khadas Edge2 has TWO-LEVEL device directory structure:
#   device/khadas/rk3588/           ← board-level (device.mk, BoardConfig.mk)
#   device/khadas/rk3588/kedge2/    ← product-level (kedge2.mk)
ls device/khadas/rk3588/kedge2/               # base product directory
find . -name 'rk3588s-khadas-edge2.dts' 2>/dev/null
find . -name 'AndroidProducts.mk' -path '*khadas*'  # in device/khadas/rk3588/
```

## Step 2 — Run the port script

The port script automates the steps below (DTS port, product directory creation,
AABox overlay merge, key symlink). Idempotent — re-runnable.

```bash
bash /home/nicklc/aabox-work/rock5bp-overlay/scripts/port-to-rock5bplus.sh /aosp/rock5bp_a14
```

If you want to do it by hand, the manual steps are 3a-3d below.

## Step 3a — Generate the Rock 5B+ DTS from Edge2 DTS

The Edge2 DTS is at `<kernel>/arch/arm64/boot/dts/rockchip/rk3588s-khadas-edge2.dts`.
The Rock 5B+ DTS is a minimal edit of the Edge2 DTS with these changes:

| Change | Edge2 value | Rock 5B+ value | Why |
|---|---|---|---|
| `model` | `"Khadas Edge2"` | `"Radxa ROCK 5B+"` | Identification |
| SoC base dtsi | `rk3588s.dtsi` (RK3588S cut-down) | `rk3588.dtsi` (RK3588 full chip) | Rock 5B+ is full RK3588 |
| `compatible` | `"khadas,edge2","rockchip,rk3588"` | `"radxa,rock-5b-plus","rockchip,rk3588"` | Identification |
| FUSB302 IRQ GPIO | `gpio1 RK_PB5` | `gpio3 RK_PB4` | The whole reason for the migration |
| `usbc0_int` pinctrl | bank 1, pin B5 | bank 3, pin B4 | Pinmux for the IRQ |
| WiFi host_wake_irq | `gpio0 RK_PA0` | `gpio0 RK_PB2` | AP6275P wake pin (different routing on 5B+) |
| BT reset_gpio | `gpio0 RK_PD4` | `gpio0 RK_PD3` | BT chip enable |
| Edge2 board-specific nodes | various | _delete_ | Camera (`kedge2-camera.dtsi`), LCD1 regulator, ES8316 codec — 5B+ doesn't have these |

Apply via the patch in `scripts/dts-port-edge2-to-rock5bplus.patch`. The patch is
narrow — it doesn't modify `&usbdrd_dwc3_0`, `&usbdp_phy0`, `&u2phy0_otg`, or any
of the USB-C role-switching graph because those are identical between Edge2 and
Rock 5B+ (both follow Rockchip's standard FUSB302-on-i2c4 topology).

## Step 3b — Create `device/radxa/rock5bplus/`

The script copies the two-level Edge2 board structure:
- `device/khadas/rk3588/` → `device/radxa/rock5bplus/` (board-level)
- `device/khadas/rk3588/kedge2/` → `device/radxa/rock5bplus/rock5bplus/` (product-level)

The port script handles this automatically.

## Step 3c — Apply AABox overlay

```bash
mkdir -p /aosp/rock5bp_a14/vendor
ln -sfT /aosp/aosp/vendor/aabox-keys /aosp/rock5bp_a14/vendor/aabox-keys

cp -r /home/nicklc/aabox-work/rock5bp-overlay/device/radxa/rock5bp/* \
      /aosp/rock5bp_a14/device/radxa/rock5bplus/aabox/

echo '$(call inherit-product, device/radxa/rock5bplus/aabox/aabox-product.mk)' \
     >> /aosp/rock5bp_a14/device/radxa/rock5bplus/radxa_rock5bplus.mk
```

## Step 3d — Add the new lunch target

`AndroidProducts.mk` declares `RadxaRock5BPlus-userdebug`. The script copies and
edits this from the Edge2 version.

## Step 4 — Build kernel first, then AOSP image

The AOSP `m` step expects two pre-built kernel artifacts:
- `kernel-6.1/arch/arm64/boot/Image` — kernel image
- `kernel-6.1/resource.img` — DTB resource image (packed by `scripts/mkimg`)

Build them with the `rk3588-rock-5b-plus.img` make target (which builds Image, DTBs,
AND resource.img in one step):

```bash
cd /aosp/rock5bp_a14/kernel-6.1
export PATH=$(pwd)/../prebuilts/clang/host/linux-x86/clang-r487747c/bin:$PATH
ARGS="ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- LLVM=1 LLVM_IAS=1"
make $ARGS kedge2_defconfig pcie_wifi.config
make $ARGS rk3588-rock-5b-plus.img -j$(nproc)
# Produces: arch/arm64/boot/Image, resource.img, boot.img, zboot.img
cd ..
```

Then build the AOSP image:

```bash
cd /aosp/rock5bp_a14
export CCACHE_DIR=/aosp/rock5bp_a14/.ccache   # ckati sandbox requires ccache inside tree
source build/envsetup.sh
lunch RadxaRock5BPlus-userdebug
m -j$(nproc)
```

First build: 1-2 hours on 32 cores. Incremental rebuilds: minutes.

## Step 5 — Build AABox Rust binaries (unchanged from CM5)

```bash
cd /home/nicklc/aabox-work/aabox && bash tools/build-android.sh
```

Same `aarch64-linux-android` target. Push to /system/bin on the flashed Rock 5B+
via the standard install-init-service.sh flow.

## Step 6 — Flash

Rockchip `upgrade_tool` (Linux) or `RKDevTool` (Windows). Put the Rock 5B+ into
maskrom mode (hold maskrom button while powering on via USB-C to the build host).

```bash
# Install upgrade_tool from rockchip-linux/rkbin
wget https://raw.githubusercontent.com/rockchip-linux/rkbin/refs/heads/master/tools/upgrade_tool
chmod +x upgrade_tool
sudo apt install libc6:i386                # 32-bit binary, needs 32-bit libc
sudo ./upgrade_tool ld                     # confirm maskrom device visible
sudo ./upgrade_tool uf out/target/product/Rock5BPlus/update.img
```

## Step 7 — Validate

After first boot:
1. Wireless ADB on port 5555 reaches without pairing (`persist.adb.tcp.port=5555` from aabox-product.mk).
2. `aabox-aapd` running via init service.
3. `aabox-resigner` watching /sdcard/Download.
4. The auto-bind script auto-detects `/sys/class/udc/fc000000.usb` (DWC3 on RK3588).
5. Plug into the Carnival via single USB-C. **This is the moment of truth** — if FUSB302 satisfies the car's CC negotiation, AA popup appears within ~5s.

## Risk register

- **DTS port might miss a board-specific quirk**: Rock 5B+ may need PMIC pin
  overrides that Edge2 doesn't. If first boot hangs at u-boot, capture serial
  console output via the RK3588 debug UART (UART2, 1.5Mbaud).
- **GPU/VPU blobs**: Mali G610 Valhall drivers from Khadas may be the same
  vendor blob as Radxa uses — both are signed by ARM. If GPU init fails, swap
  blobs from Radxa's `radxa-firmware` repo.
- **AP6275P firmware**: Both vendors ship the same Broadcom BCM4375 firmware,
  but the NVRAM file (`brcmfmac4375-sdio.<board>.txt`) is board-specific. Pull
  the Rock 5B+ NVRAM from `radxa/radxa-firmware` and copy it to
  `vendor/etc/firmware/brcm/brcmfmac4375-sdio.radxa,rock-5b-plus.txt`.
- **HDMI**: Edge2 has HDMI-TX only; Rock 5B+ has HDMI 2.1 + HDMI-RX. The DTS
  port keeps HDMI-TX; HDMI-RX is unused for AABox so we skip its node.

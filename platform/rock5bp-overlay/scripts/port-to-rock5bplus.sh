#!/usr/bin/env bash
# port-to-rock5bplus.sh — port Khadas Edge2 Android 14 tree to Rock 5B+.
#
# Idempotent. Run after `repo sync` completes at the AOSP root (default
# /aosp/rock5bp_a14). Produces:
#   - kernel-6.1/arch/arm64/boot/dts/rockchip/rk3588-rock-5b-plus.dts  (new)
#   - device/radxa/rock5bplus/                                          (new)
#   - vendor/aabox-keys symlink to /aosp/aosp/vendor/aabox-keys         (new)
#
# Khadas Edge2 directory structure (two-level):
#   device/khadas/rk3588/          ← board-level: device.mk, BoardConfig.mk
#   device/khadas/rk3588/kedge2/   ← product-level: kedge2.mk, product BoardConfig.mk
#
# Does NOT modify Khadas's source. The 5B+ DTS is *added*, not a patch on the
# Edge2 DTS — both targets remain buildable from the same tree.

set -euo pipefail

AOSP_ROOT=${1:-/aosp/rock5bp_a14}
OVERLAY=/home/nicklc/aabox-work/rock5bp-overlay
AABOX_KEYS_SRC=/aosp/aosp/vendor/aabox-keys

if [ ! -d "$AOSP_ROOT" ]; then
    echo "FATAL: $AOSP_ROOT does not exist. Sync first." >&2
    exit 1
fi

cd "$AOSP_ROOT"

echo "=== 1. Locating Edge2 DTS and product dirs ==="
EDGE2_DTS=$(find . -name 'rk3588s-khadas-edge2.dts' -not -path './out/*' | head -1)
# Board-level dir: device/khadas/rk3588  (has device.mk, BoardConfig.mk, wifi_bt.mk, etc.)
EDGE2_BOARD_DIR=$(find . -mindepth 3 -maxdepth 3 -type d -path '*device/khadas/rk3588' | head -1)
# Product-level dir: device/khadas/rk3588/kedge2  (has kedge2.mk, product BoardConfig.mk)
EDGE2_PRODUCT_DIR="${EDGE2_BOARD_DIR}/kedge2"
KERNEL_DIR=$(echo "$EDGE2_DTS" | sed 's|/arch/arm64/.*||')

if [ -z "$EDGE2_DTS" ] || [ -z "$EDGE2_BOARD_DIR" ]; then
    echo "FATAL: could not locate Edge2 DTS or board dir. Tree may not be Khadas Edge2 Android 14."
    echo "  EDGE2_DTS=$EDGE2_DTS"
    echo "  EDGE2_BOARD_DIR=$EDGE2_BOARD_DIR"
    exit 1
fi

if [ ! -d "$EDGE2_PRODUCT_DIR" ]; then
    echo "FATAL: kedge2 product subdir not found at $EDGE2_PRODUCT_DIR"
    exit 1
fi

DTS_DIR=$(dirname "$EDGE2_DTS")
ROCK5BP_DTS="$DTS_DIR/rk3588-rock-5b-plus.dts"

echo "  Edge2 DTS:             $EDGE2_DTS"
echo "  Edge2 board dir:       $EDGE2_BOARD_DIR"
echo "  Edge2 product dir:     $EDGE2_PRODUCT_DIR"
echo "  Kernel root:           $KERNEL_DIR"
echo "  Output Rock 5B+ DTS:   $ROCK5BP_DTS"

echo
echo "=== 2. Generate Rock 5B+ DTS from Edge2 DTS ==="
cp "$EDGE2_DTS" "$ROCK5BP_DTS"

# SoC base: Edge2 uses RK3588S; Rock 5B+ uses RK3588 (full chip, superset).
# rk3588.dtsi adds the second USB3-OTG and additional PCIe/HDMI-RX nodes.
sed -i 's|#include "rk3588s.dtsi"|#include "rk3588.dtsi"|' "$ROCK5BP_DTS"

# Identification
sed -i 's|model = "Khadas Edge2";|model = "Radxa ROCK 5B+";|' "$ROCK5BP_DTS"
sed -i 's|compatible = "khadas,edge2", "rockchip,rk3588";|compatible = "radxa,rock-5b-plus", "rockchip,rk3588";|' "$ROCK5BP_DTS"

# FUSB302 IRQ GPIO: gpio1 RK_PB5 -> gpio3 RK_PB4
sed -i '/usbc0: fusb302@22 {/,/^	};/ {
    s|interrupt-parent = <&gpio1>;|interrupt-parent = <\&gpio3>;|
    s|interrupts = <RK_PB5 IRQ_TYPE_LEVEL_LOW>;|interrupts = <RK_PB4 IRQ_TYPE_LEVEL_LOW>;|
    s|int-n-gpios = <&gpio1 RK_PB5 GPIO_ACTIVE_LOW>;|int-n-gpios = <\&gpio3 RK_PB4 GPIO_ACTIVE_LOW>;|
}' "$ROCK5BP_DTS"

# Pinctrl: usbc0_int pin bank 1 -> bank 3
sed -i '/usbc0_int:/,/^	};/ {
    s|rockchip,pins = <1 RK_PB5 |rockchip,pins = <3 RK_PB4 |
}' "$ROCK5BP_DTS"

# WiFi host_wake_irq: gpio0 RK_PA0 -> gpio0 RK_PB2
sed -i 's|WIFI,host_wake_irq = <&gpio0 RK_PA0 GPIO_ACTIVE_HIGH>;|WIFI,host_wake_irq = <\&gpio0 RK_PB2 GPIO_ACTIVE_HIGH>;|' "$ROCK5BP_DTS"

# BT reset_gpio: gpio0 RK_PD4 -> gpio0 RK_PD3
sed -i 's|BT,reset_gpio    = <&gpio0 RK_PD4 GPIO_ACTIVE_HIGH>;|BT,reset_gpio    = <\&gpio0 RK_PD3 GPIO_ACTIVE_HIGH>;|' "$ROCK5BP_DTS"

# Remove Edge2-specific includes the 5B+ doesn't need
sed -i '/#include "kedge2-camera.dtsi"/d' "$ROCK5BP_DTS"

echo "  Generated $ROCK5BP_DTS"
echo "  Diff against Edge2 base:"
diff -u "$EDGE2_DTS" "$ROCK5BP_DTS" | head -60 || true

echo
echo "=== 3. Patch kernel Makefile to build the new DTB ==="
DTS_MAKEFILE="$DTS_DIR/Makefile"
if ! grep -q 'rk3588-rock-5b-plus.dtb' "$DTS_MAKEFILE"; then
    sed -i '/rk3588s-khadas-edge2.dtb/a dtb-$(CONFIG_ARCH_ROCKCHIP) += rk3588-rock-5b-plus.dtb' "$DTS_MAKEFILE"
    echo "  Added rk3588-rock-5b-plus.dtb to $DTS_MAKEFILE"
else
    echo "  $DTS_MAKEFILE already has rk3588-rock-5b-plus.dtb entry"
fi

echo
echo "=== 4. Create device/radxa/rock5bplus/ product directory ==="
NEW_BOARD_DIR="device/radxa/rock5bplus"
NEW_PRODUCT_DIR="$NEW_BOARD_DIR/rock5bplus"

if [ -d "$NEW_BOARD_DIR" ]; then
    echo "  $NEW_BOARD_DIR exists — preserving (idempotent re-run)"
else
    mkdir -p "$(dirname "$NEW_BOARD_DIR")"
    # Copy the entire board-level dir (device.mk, BoardConfig.mk, wifi_bt.mk, etc.)
    cp -r "$EDGE2_BOARD_DIR" "$NEW_BOARD_DIR"
    echo "  Copied $EDGE2_BOARD_DIR -> $NEW_BOARD_DIR"

    # Rename the kedge2 product subdir to rock5bplus
    mv "$NEW_BOARD_DIR/kedge2" "$NEW_PRODUCT_DIR"
    echo "  Renamed kedge2/ -> rock5bplus/"

    # Remove the copied preinstall directory: it defines the same module names (tts, Chrome)
    # as device/khadas/rk3588/preinstall/ which is still in the tree, causing duplicate
    # MODULE.TARGET.APPS.* errors at build time. We don't need those Khadas APKs in the
    # AABox image.
    rm -rf "$NEW_BOARD_DIR/preinstall"
fi

# Rename the product makefile kedge2.mk -> RadxaRock5BPlus.mk
# The filename must match the lunch target — Android build system derives the product
# name from basename(filename), so RadxaRock5BPlus.mk → product RadxaRock5BPlus.
if [ -f "$NEW_PRODUCT_DIR/kedge2.mk" ] && [ ! -f "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk" ]; then
    mv "$NEW_PRODUCT_DIR/kedge2.mk" "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk"
elif [ -f "$NEW_PRODUCT_DIR/radxa_rock5bplus.mk" ] && [ ! -f "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk" ]; then
    mv "$NEW_PRODUCT_DIR/radxa_rock5bplus.mk" "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk"
fi

# Rename board-level BoardConfig.mk to BoardConfigCommon.mk.
# Android's board_config.mk does: find device -path '*/rock5bplus/BoardConfig.mk'
# Both device/radxa/rock5bplus/BoardConfig.mk AND .../rock5bplus/rock5bplus/BoardConfig.mk
# match that pattern (outer dir is also named rock5bplus). Renaming the board-level
# one avoids the "Multiple board config files" error.
cd "$AOSP_ROOT/$NEW_BOARD_DIR"
if [ -f "BoardConfig.mk" ] && [ ! -f "BoardConfigCommon.mk" ]; then
    mv BoardConfig.mk BoardConfigCommon.mk
fi

# Update references in board-level files
sed -i 's|device/khadas/rk3588|device/radxa/rock5bplus|g'             *.mk 2>/dev/null || true
sed -i 's|PRODUCT_KERNEL_DTS ?= rk3588s-khadas-edge2|PRODUCT_KERNEL_DTS ?= rk3588-rock-5b-plus|g' BoardConfigCommon.mk 2>/dev/null || true

# Update references in product-level files (most specific patterns first)
cd "$AOSP_ROOT/$NEW_PRODUCT_DIR"
sed -i 's|device/khadas/rk3588/kedge2|device/radxa/rock5bplus/rock5bplus|g'  *.mk 2>/dev/null || true
sed -i 's|device/khadas/rk3588|device/radxa/rock5bplus|g'                    *.mk 2>/dev/null || true
# Update the include of board-level BoardConfig.mk (now renamed to BoardConfigCommon.mk)
sed -i 's|include device/radxa/rock5bplus/BoardConfig\.mk|include device/radxa/rock5bplus/BoardConfigCommon.mk|g' *.mk 2>/dev/null || true
sed -i 's|rk3588s-khadas-edge2|rk3588-rock-5b-plus|g'                        *.mk 2>/dev/null || true
sed -i 's|Khadas Edge2|Radxa ROCK 5B+|g'                                      *.mk 2>/dev/null || true
sed -i 's|khadas,edge2|radxa,rock-5b-plus|g'                                  *.mk 2>/dev/null || true
sed -i 's|khadas_edge2|radxa_rock5bplus|g'                                    *.mk 2>/dev/null || true
# NOTE: "kedge2" sed is intentionally scoped — do NOT rename kedge2_defconfig.
# kedge2_defconfig is a kernel config file (in kernel-6.1/arch/arm64/configs/).
# Rock 5B+ and Edge2 share the same RK3588(S) BSP kernel; the defconfig is reusable.
# Only rename non-defconfig kedge2 references.
sed -i 's|PRODUCT_DEVICE := kedge2|PRODUCT_DEVICE := rock5bplus|g'           *.mk 2>/dev/null || true
sed -i 's|PRODUCT_NAME := kedge2|PRODUCT_NAME := RadxaRock5BPlus|g'          *.mk 2>/dev/null || true

# Write new AndroidProducts.mk at the board level
cd "$AOSP_ROOT"
cat > "$NEW_BOARD_DIR/AndroidProducts.mk" <<'EOF'
PRODUCT_MAKEFILES := \
    $(LOCAL_DIR)/rock5bplus/RadxaRock5BPlus.mk

COMMON_LUNCH_CHOICES := \
    RadxaRock5BPlus-userdebug \
    RadxaRock5BPlus-user
EOF

# Fix remaining product identity fields in the product makefile
sed -i 's|PRODUCT_BRAND := rockchip|PRODUCT_BRAND := Radxa|g' \
    "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk" 2>/dev/null || true
sed -i 's|PRODUCT_MANUFACTURER := Khadas|PRODUCT_MANUFACTURER := Radxa|g' \
    "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk" 2>/dev/null || true
sed -i 's|PRODUCT_MODEL := Edge2|PRODUCT_MODEL := ROCK 5B+|g' \
    "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk" 2>/dev/null || true

# Remove preinstall mechanism from the product makefile.
# device/khadas/rk3588/preinstall/ is still in the tree and defines the same
# module names (tts, Chrome). Our copy would cause MODULE.TARGET.APPS.* duplicates.
sed -i '/auto_generator.py preinstall/d' "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk" 2>/dev/null || true
sed -i '/preinstall\/preinstall.mk/d'    "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk" 2>/dev/null || true
sed -i '/preinstall\/preinstall.sh/d'    "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk" 2>/dev/null || true

echo
echo "=== 5. AABox overlay: keys + init scripts + product.mk fragment ==="
mkdir -p vendor
if [ -L vendor/aabox-keys ] || [ -e vendor/aabox-keys ]; then
    rm -f vendor/aabox-keys
fi
ln -sfT "$AABOX_KEYS_SRC" vendor/aabox-keys

mkdir -p "$NEW_PRODUCT_DIR/aabox"
cp -r "$OVERLAY/device/radxa/rock5bp/"* "$NEW_PRODUCT_DIR/aabox/"

# Wire the AABox product.mk fragment into the main product makefile.
if ! grep -q 'aabox-product.mk' "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk"; then
    echo '' >> "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk"
    echo '# AABox additions' >> "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk"
    echo '$(call inherit-product, device/radxa/rock5bplus/rock5bplus/aabox/aabox-product.mk)' >> "$NEW_PRODUCT_DIR/RadxaRock5BPlus.mk"
fi

# Fix paths in AABox product.mk to point at the new location.
sed -i 's|device/radxa/rock5bp/|device/radxa/rock5bplus/rock5bplus/aabox/|g' \
    "$NEW_PRODUCT_DIR/aabox/aabox-product.mk"

echo "  Symlinked $AABOX_KEYS_SRC -> vendor/aabox-keys"
echo "  Copied AABox overlay -> $NEW_PRODUCT_DIR/aabox/"

echo
echo "=== Done ==="
echo "  Board dir:    $NEW_BOARD_DIR"
echo "  Product dir:  $NEW_PRODUCT_DIR"
echo "  DTS:          $ROCK5BP_DTS"
echo
echo "Next:"
echo "  cd $AOSP_ROOT"
echo "  source build/envsetup.sh"
echo "  lunch RadxaRock5BPlus-userdebug"
echo "  m -j\$(nproc)"

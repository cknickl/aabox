# aabox-product.mk — fragment to include in Rock 5B+ product makefile.
#
# Place via:
#   $(call inherit-product, device/radxa/rock5bp/aabox-product.mk)
# in whatever the Rock 5B+ product .mk file ends up being called
# (likely aosp_rock5bp.mk or rock5bp.mk; will know after `repo sync` lands).

# --- AABox: static unauthenticated ADB over TCP port 5555 ---
# So the box is reachable headless in the car without hunting for the
# rotating Wireless Debugging port. Trade-off: no auth on this port, fine
# for car Wi-Fi / phone hotspot, NOT for public networks.
PRODUCT_PROPERTY_OVERRIDES += \
    persist.adb.tcp.port=5555

# --- AABox: ship the daemon + resigner + auto-bind init scripts in /system ---
PRODUCT_COPY_FILES += \
    device/radxa/rock5bp/usb/aabox-aapd.rc:system/etc/init/aabox-aapd.rc \
    device/radxa/rock5bp/usb/aabox-resigner.rc:system/etc/init/aabox-resigner.rc \
    device/radxa/rock5bp/usb/aabox-usb-bind.sh:system/bin/aabox-usb-bind.sh \
    device/radxa/rock5bp/usb/aabox-aapd-watchdog.sh:system/bin/aabox-aapd-watchdog.sh

# The aabox-aapd and aabox-resigner binaries themselves are NOT yet shipped
# from source in this product makefile — they're built out-of-tree via
# tools/build-android.sh (cargo-ndk) and pushed post-flash. A follow-up is
# to either bake them as prebuilts (PRODUCT_COPY_FILES) or wire up Soong
# rules to build them in-tree.

# --- AABox: AABox platform signing keys live at /vendor/aabox-keys/ ---
# Same keys we used on CM5. Apps already signed with AABox keys keep
# working across the migration. Copy is done via PRODUCT_COPY_FILES from a
# vendor/ tree we symlink into the Radxa source.
PRODUCT_COPY_FILES += \
    vendor/aabox-keys/platform.pk8:vendor/aabox-keys/platform.pk8 \
    vendor/aabox-keys/platform.x509.pem:vendor/aabox-keys/platform.x509.pem

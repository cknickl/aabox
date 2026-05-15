#!/usr/bin/env bash
# One-command Android cross-build for AABox crates.
#
# Defaults: arm64-v8a + API 33 (Android 13+). Override via env vars.
#
# Requires:
#   - ANDROID_NDK_HOME pointing at an NDK r27+ install
#   - cargo-ndk installed (cargo install cargo-ndk)
#   - rustup target add aarch64-linux-android
set -euo pipefail

cd "$(dirname "$0")/.."

# Ensure cargo is on PATH even from non-login shells
[[ -f "${HOME}/.cargo/env" ]] && source "${HOME}/.cargo/env"

: "${ANDROID_NDK_HOME:=/opt/android-ndk-r27c}"
: "${ANDROID_PLATFORM:=33}"
: "${ANDROID_ABI:=arm64-v8a}"
: "${BUILD_PROFILE:=debug}"

export ANDROID_NDK_HOME

PROFILE_FLAG=""
[[ "${BUILD_PROFILE}" == "release" ]] && PROFILE_FLAG="--release"

echo "[build-android.sh] NDK=${ANDROID_NDK_HOME}  ABI=${ANDROID_ABI}  API=${ANDROID_PLATFORM}  PROFILE=${BUILD_PROFILE}"

cargo ndk \
    --target "${ANDROID_ABI}" \
    --platform "${ANDROID_PLATFORM}" \
    build ${PROFILE_FLAG} \
    -p aabox-aapd \
    -p aabox-resigner

OUT="target/aarch64-linux-android/${BUILD_PROFILE}"
echo
echo "[build-android.sh] artifacts:"
for f in aabox-aapd libaabox_aapd.so aabox-resigner libaabox_resigner.so; do
    [[ -f "${OUT}/${f}" ]] && file "${OUT}/${f}"
done

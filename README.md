# AABox

Self-hosted Android Auto AI Box for Raspberry Pi CM5. Custom AOSP 16 platform + a from-scratch AAP source daemon that lets the CM5 act as an Android Auto source (replacing your phone) when plugged into a car.

**Status**: Phase 0 (AOSP foundation + GApps + CarCar Launcher) complete. Phase 1 scaffolding in progress.

## Architecture (Path X)

Build from scratch in Rust. Reference but do **not** depend on existing OSS:

- [`aa-proxy-rs`](https://github.com/manio/aa-proxy-rs) — USB gadget + AOAv2 reference (closest fit; Rust)
- [`aasdk`](https://github.com/f1xpl/aasdk) — protobuf definitions (`aasdk_proto/`) + SSL handshake reference (C++; headunit role, we're source role)
- milek7's AA protocol notes — covers channels aasdk doesn't

Why not use aasdk as a library: aasdk targets the **headunit** role (car-side). We need the **source** role (phone-side). Different concerns; cleaner to write a new daemon designed for source from day one.

## Repo layout

```
aabox/
├── Cargo.toml                  # Rust workspace
├── crates/
│   ├── aabox-proto/            # prost-generated AAP message types from aasdk_proto
│   ├── aabox-common/           # shared types (ChannelId, Error)
│   ├── aabox-aapd/             # the AAP source daemon — bin + cdylib for JNI
│   └── aabox-resigner/         # on-device CarCar re-signer service — parallel concern
├── android/
│   ├── jni/                    # JNI glue / native interfaces (Kotlin → Rust)
│   └── service-wrapper/        # Android service that loads the daemon as a privileged process
├── references/                 # git submodules — for reference only, do not link against
├── captures/                   # Wireshark .pcap captures of real AA sessions
├── docs/                       # architecture, phase plan, protocol notes
└── tools/                      # DHU setup, capture scripts, etc.
```

## Phase plan (timeline)

| Phase | Goal | Estimate | Status |
|---|---|---|---|
| 0 | Foundation: AOSP + GApps + CarCar | 1 evening | ✅ 2026-05-15 |
| 1 | Project scaffolding, DHU connectable, baseline capture | 1 weekend | 🚧 |
| 2 | USB gadget + AOAv2 handshake (HIGHEST RISK) | 2-3 weekends | pending |
| 3 | AAP video-only bringup (SSL, Service Discovery, video channel) | 3-4 weekends | pending |
| 4 | Input + audio channels — full bidirectional | 2-3 weekends | pending |
| 5 | Sensor channel proper (GPS, gear, speed) | 1-2 weekends | pending |
| 6 | Navigation Status channel — cluster/HUD turn-by-turn | 4-6 weekends | pending |
| 7 | Polish (boot time, ignition wake, thermals, OTA) | ongoing | pending |

Parallel: on-device CarCar re-signer service. Shares scaffolding with the daemon. ~3-4 days marginal effort once the daemon's project tax is paid.

## Build

**Host (Linux x86_64 dev box)**:

```bash
cargo build --workspace
cargo test  --workspace   # runs the prost-roundtrip smoke test
```

**Android (aarch64-linux-android, API 33+)**:

```bash
# One-time: install cross-compile bits
rustup target add aarch64-linux-android
cargo install cargo-ndk
# Set ANDROID_NDK_HOME=/path/to/ndk (r27+)

./tools/build-android.sh
# → target/aarch64-linux-android/debug/aabox-aapd  (binary)
# → target/aarch64-linux-android/debug/libaabox_aapd.so  (cdylib for JNI)
# (and same pair for aabox-resigner)
```

The build script honors `ANDROID_PLATFORM` (default 33), `ANDROID_ABI` (default `arm64-v8a`), `BUILD_PROFILE` (default `debug`; set to `release` for stripped optimized builds).

## Legal / IP

This project implements the Android Auto Protocol via reverse-engineered protobuf definitions and references. "Android Auto" is a Google trademark; this project uses "AAP" or "Android Auto Protocol" in technical contexts and makes no certification claims. The aasdk "headunit certificate" used during the SSL handshake is in a gray zone (extracted from Pioneer head unit firmware years ago); usage in personal hobby projects has a long unchallenged history but commercial distribution would require generating your own certs from hardware you own. DMCA §1201 exemption for vehicle ECU repair/modification (renewed 2024) covers personal modification but not redistribution.

**This is a personal-use project.**

# Architecture

## Two services, one project

The project contains **two** services that ship together because they share the Soong build scaffolding, JNI/FFI infrastructure, system service registration, and SELinux policy on the Android side:

1. **aabox-aapd** — the AAP source daemon. The product-value service.
2. **aabox-resigner** — the on-device CarCar re-signer. A QoL service that lets the launcher auto-update without breaking our custom platform signing.

Both run as the `system` UID (signed by the AABox platform key on the AOSP build).

## aabox-aapd dataflow

```
  Car (head unit)
       │
       │  USB (FunctionFS / AOAv2)
       ▼
  ┌─────────────────────┐
  │  aabox-aapd (Rust)  │
  │                     │
  │  - AOAv2 handshake  │
  │  - SSL/TLS w/ aasdk │
  │    headunit cert    │
  │  - Channel mux/demux│
  │  - Per-channel impls│
  └────────┬────────────┘
           │ JNI
           ▼
  ┌─────────────────────┐
  │  Android system     │
  │  (CarCar, GMaps,    │
  │  Spotify, etc.)     │
  │                     │
  │  - MediaProjection  │  (video out)
  │  - AudioRecord      │  (audio out)
  │  - InputManager     │  (input in)
  │  - LocationManager  │  (sensor)
  │  - NotificationLstr │  (nav status)
  └─────────────────────┘
```

## Channel implementation order (by value)

Per Path X, we prioritize channels for value, not protocol order:

| Order | Channel(s) | Phase |
|---|---|---|
| 1 | Control + Video + Input | Phase 3-4 — minimum viable AA in the car |
| 2 | Sensor (bare minimum) | Phase 3 — required or car refuses to negotiate |
| 3 | Audio (media + voice) | Phase 4 — music + voice in car speakers |
| 4 | Sensor (proper GPS) | Phase 5 — AA's "moving" detection works |
| 5 | Navigation Status | Phase 6 — turn-by-turn on cluster/HUD |
| 6 | Bluetooth, Speech, others | Phase 4+ |

## aabox-resigner dataflow

```
  CarCar Installer (Play Store app)
       │
       │  PackageInstaller.Session.commit()
       ▼
  Android PackageManager
       │ INSTALL_FAILED_UPDATE_INCOMPATIBLE
       ▼
  ┌─────────────────────────────┐
  │  aabox-resigner (Rust)      │
  │                             │
  │  - PackageInstaller callbck │
  │  - Pull staged APK          │
  │  - dex patch (clamp SDK_INT │
  │    register at gate sites)  │
  │  - Re-sign w/ platform.pk8  │
  │  - Submit new install       │
  └─────────────────────────────┘
       │
       ▼
  CarCar Launcher updated, system UID retained
```

The platform.pk8 lives somewhere only this service (signed by the same key) can read. Probably `/data/aabox-resigner/keys/` with mode 0700 + SELinux label `aabox_resigner_data_file`.

## Why Rust

- Memory safety in protocol parsing (where bugs would be silent byte-level corruption).
- Maps cleanly to aa-proxy-rs reference for the USB/AOAv2 layer.
- Mature on Android: used in the platform's Bluetooth stack, Keystore, and other system services since Android 13.
- `prost` for protobuf, `rustls` for TLS, `tokio` for async — no C++ runtime dependencies.

## SELinux

Both services need custom domains:
- `aabox_aapd` domain — allows USB raw access, InputManager IPC, MediaProjection bind, AudioRecord, network for WiFi projection
- `aabox_resigner` domain — allows PackageInstaller bind, read of own data dir (keys), no network

Policy lives under `device/brcm/rpi5/sepolicy/` in the AOSP tree.

# Phase 3 — AAP protocol bringup (video-only target)

## What landed (commit-level)

| Module | Role |
|---|---|
| `aabox-common::channel_id` | `ChannelId` (Control/Input/Sensor/Video/...) and `ControlMessageId` (VersionRequest/SslHandshake/ServiceDiscoveryResponse/ChannelOpenResponse/...) enums, values from aasdk |
| `aabox-aapd::framing` | AAP wire frame: byte 0 = channel ID, byte 1 = flags (FIRST/LAST/CONTROL/ENCRYPTED), bytes 2–3 = payload length, bytes 4–7 = total length (only on FIRST). Encode/decode + BULK convenience constructor. Multi-fragment reassembly is a Phase 4 add-on. |
| `aabox-aapd::tls` | rustls 0.23 ClientConfig with the embedded aasdk "headunit cert" (re-issued as X.509 v3 since rustls rejects v1; same RSA key, so the car still validates via private-key possession). Custom verifier accepts any server cert (AAP doesn't pin head-unit certs). Crypto provider: ring. |
| `aabox-aapd::services` | `ServiceDiscoveryResponse` builder advertising sensor + video channels. 720p60 video config. Sensor channel declares DrivingStatus / Gear / ParkingBrake (real sensor responses are Phase 5). |
| `aabox-aapd::control` | Version handshake (plaintext, 4-byte body), SSL handshake tunneling (`SslHandshake` control message wraps rustls bytes), async read/write helpers for any tokio AsyncRead/Write. |
| `aabox-aapd` `dhu` CLI | Connect to DHU at TCP `127.0.0.1:5277`, run the version handshake, build the TLS config and the SDR payload. Stops short of running the rustls handshake over the tunnel — that's the next iteration (no DHU on this VM to test against). |
| `tests/version_handshake.rs` | End-to-end: a fake head unit in the same process accepts the daemon's TCP connection, validates the VersionRequest frame, replies with VersionResponse. Proves framing + control logic on real TCP I/O. |

11 tests, 0 failures. Host (`cargo build --workspace`) and Android (`./tools/build-android.sh`) both clean.

## What remains for Phase 3 done state

The plan's Phase 3 done state is *"CarCar's UI shows on Carnival's center screen"*. To get there:

1. **TLS tunnel over AAP frames** — write a small driver loop that:
   - Pulls rustls handshake bytes out of `ConnectionCommon::write_tls()`
   - Wraps each chunk in an `SslHandshake` control frame
   - Sends via the transport
   - On incoming SslHandshake frames, feeds the body into `read_tls()`
   - Repeats until `is_handshaking()` returns false
   - After handshake, all subsequent frames have `ENCRYPTED` flag and are encrypted/decrypted via rustls
2. **AuthComplete + ServiceDiscoveryResponse send** — once the tunnel is up.
3. **Channel Open response** for the video channel.
4. **Sensor bootstrap** — minimum responses (DrivingStatus active=false, Gear=PARK, ParkingBrake=true) so the car accepts the negotiation.
5. **Video frame send** — start with a static color-bars test pattern (raw H.264 elementary stream of one I-frame on loop). Real screen-capture via MediaProjection + MediaCodec is the Android-side bringup; the daemon just forwards bytes.

## Test harness plan

The fake head unit in `tests/version_handshake.rs` is the seed of a local AAP server we can grow into a Phase-3 self-test harness. Add over time:
- Server-side rustls (using the same headunit cert so we don't need a separate CA)
- ServiceDiscoveryRequest sender
- ChannelOpenRequest sender
- Decoder for sensor responses

That gives us a `cargo test` we can run on every commit that exercises the full handshake without DHU or hardware. Real DHU + Carnival are integration tests, not regression tests.

## Legal reminder

The embedded headunit cert + key are derived from aasdk (extracted from Pioneer head unit firmware years ago). Personal-use only. See README's legal note before considering anything else.

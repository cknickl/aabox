# Desktop Head Unit (DHU) setup

DHU is Google's official Android Auto emulator. It runs on your Mac and pretends to be a car head unit, so you can test the daemon against a real AAP endpoint without plugging into a real car. This is the Phase 1 milestone target.

## On the Mac

1. Install Android Studio (or just `sdkmanager`).
2. Install the DHU package:
   ```bash
   sdkmanager "extras;google;auto"
   ```
3. DHU lands at `$ANDROID_HOME/extras/google/auto/desktop-head-unit`.

## Connecting

DHU acts as the head unit; your AAP source (a phone, or eventually our CM5) connects to it over USB or ADB tunnel.

For development against the CM5 — run DHU on the Mac with the CM5 over ADB-forwarded TCP, like:

```bash
# On the Mac, with the CM5's wireless adb connected:
adb forward tcp:5277 tcp:5277
./desktop-head-unit
```

The daemon on the CM5 then connects to `localhost:5277` (forwarded by adb), which DHU is listening on.

For pre-CM5 development on a Linux/Mac dev host, the `aabox-aapd` binary has a `--dhu HOST:PORT` flag that bypasses USB entirely.

## What to verify

- DHU starts and shows its UI
- The daemon can complete the SSL handshake against DHU (Phase 3 milestone — DHU accepts aasdk's headunit cert)
- Service Discovery messages flow both directions

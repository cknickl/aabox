# In-car test procedure (Carnival USB-C)

Goal: plug the CM5 into the Kia Carnival's USB-C port via a single USB-C cable
(same wiring CarLinkit / Picasou boxes use), see what AAP messages flow.

## Before driving

1. **Reachable CM5 over wireless ADB**. Settings → System → Developer
   options → Wireless debugging → note the IP:port. Then on the build VM:
   ```bash
   adb pair 172.16.10.111:<pair-port> <code>      # only if not already paired
   adb connect 172.16.10.111:<connect-port>
   ```

2. **Run the prep script**. From the build VM:
   ```bash
   CM5_ADDR=172.16.10.111:<connect-port> tools/in-car-prep.sh
   ```
   This:
   - Cross-compiles the latest daemon for `aarch64-linux-android`
   - Pushes it + the watchdog script to `/data/local/tmp/`
   - Clears any old log
   - Starts the watchdog under `nohup` (survives ADB disconnect)
   - The watchdog launches `aabox-aapd usb-run` and respawns it if it exits

3. **Verify daemon is alive**:
   ```bash
   adb -s 172.16.10.111:<connect-port> shell "cat /data/local/tmp/aabox-aapd.status; ps -A | grep aabox"
   ```
   Status should read `usb-run: waiting for VersionRequest` (or similar) and
   you should see `aabox-aapd` and `aabox-aapd-watchdog.sh` processes.

4. **Disconnect ADB**. Daemon stays running.

5. **Wire the CM5 for the car**:
   - CM5 IO board slave USB-C → USB-C cable → Carnival's Android Auto USB-C port
   - Single cable. Car sources 5V/3A on VBUS, USB 2.0 data on D+/D-.
   - The board's slave USB-C should pull CC down (Rd resistor) signaling "I'm a
     device, source power to me" — Carnival enables VBUS, CM5 powers up.

## At the car

6. **Park, turn ignition on**, plug the USB-C cable into the Carnival's
   Android Auto port (center console, *not* a charging-only port).

7. **Watch for boot**. The CM5 should boot in ~30s. The car's display may
   say "connecting..." or similar Android Auto branding.

8. **Once running, the watchdog should already be alive** from when you ran
   the prep script. The daemon catches `/dev/usb_accessory` becoming
   readable, runs the version handshake + TLS handshake + frame capture.

9. **Wait ~60s** to let the daemon collect frames, then either:
   - Unplug (the daemon's capture window has elapsed; the watchdog will
     respawn it for the next plug-in).
   - Or leave plugged in for more data.

## After the test

10. **Reconnect wireless ADB** (back home, on the same network as the CM5).
11. **Pull logs**:
    ```bash
    CM5_ADDR=172.16.10.111:<connect-port> tools/in-car-pull-logs.sh
    ```
    Logs land under `captures/in-car-YYYYMMDD_HHMMSS/`:
    - `aabox-aapd.log` — full daemon log with frame hex dumps
    - `aabox-aapd.status` — last status line
    - `watchdog.log` — daemon start/exit/restart events
    - `dmesg.txt` — kernel ring buffer (USB hot-plug events)
    - `logcat-usb.txt` — Android logcat filtered for USB/accessory events

12. **What we're looking for in the log**:
    - `INFO accessory device opened — waiting for host AOAv2 handshake` — kernel side OK
    - `INFO VersionRequest received peer_major=1 peer_minor=N` — car spoke AAP at us
    - `DEBUG RX frame header header=...` / `RX frame payload payload=...` — the car's bytes
    - `INFO TLS handshake complete negotiated=Some(TLSv1_2) cipher=...` — auth done
    - Or: `ERROR TLS handshake failed: ...` — tells us exactly where the car drew the line

## What's different from DHU

The Carnival's AA implementation uses the long-standing permissive cert
validation that real-car AA has always used. It does **not** have DHU 2.0's
strict cert-name allowlist that blocked us in desktop testing. So the
`O = JVC Kenwood / O = Android-Auto-Internal / etc.` debate that mattered
for DHU is irrelevant in the car — the car should accept our self-signed
cert family.

## If the daemon stops responding

Reboot the CM5 (unplug + replug). The watchdog runs only as long as the
`nohup`-launched parent process — it does *not* survive a reboot. For
persistence across reboots we'd add an `init.rc` snippet that launches
the watchdog as a system service, which means rebuilding the system
image. That's a "next AOSP rebuild" task — not needed for today's drive.

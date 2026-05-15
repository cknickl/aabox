# Capturing a baseline AA session

Goal: have a Wireshark / pcap file showing a real AA session between a real phone and a real car (or DHU), so we can compare our daemon's output to ground truth during Phase 3+.

## Option A — USB capture via usbmon (Linux only)

1. On a Linux box, plug a real Android phone via USB.
2. Plug the other end into a real car (or USB to a Mac running DHU).
3. Load usbmon:
   ```bash
   sudo modprobe usbmon
   ```
4. Identify the bus number (`lsusb`).
5. Capture:
   ```bash
   sudo tshark -i usbmon<bus#> -w captures/aa-baseline.pcap
   ```
6. Stop capture after a representative AA session — boot, accept connection, swipe to a few apps, launch nav, take a turn.

## Option B — TCP capture against DHU

1. Run DHU on the Mac.
2. Connect a real phone via USB.
3. Capture loopback traffic on port 5277:
   ```bash
   tcpdump -i lo0 -w captures/aa-dhu-baseline.pcap port 5277
   ```

The SSL-encrypted bytes won't be directly readable, but the framing and channel IDs are. For decryption you can inject `SSLKEYLOGFILE`-style key material if you have access to the SSL session keys (advanced; doable since we know the headunit cert).

## What to capture

- Initial AOAv2 USB handshake (Option A only)
- SSL handshake (both sides' certificates)
- Service Discovery messages
- Channel Open for video, sensor, audio, input
- A few frames on each channel
- A nav session with turn-by-turn — this is the reference for Phase 6

Store small reference captures under `captures/`. The `.gitignore` excludes `*.pcap` by default; commit specific reference captures manually.

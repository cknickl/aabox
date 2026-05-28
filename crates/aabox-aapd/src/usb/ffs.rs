//! Userspace AOA protocol handler via Linux FunctionFS.
//!
//! Replaces the `f_accessory` kernel driver for the AOA handshake so that
//! `ACCESSORY_START` never triggers a USB disconnect/reconnect. Instead, after
//! the KIA finishes the AOA control sequence our daemon opens the bulk
//! endpoints on the same connection and hands them back to the caller.
//!
//! # Gadget layout
//!
//! We create `/config/usb_gadget/aabox` (separate from init's `g1`) with:
//!   - idVendor=0x18d1 / idProduct=0x2D00  (Google Android Accessory)
//!   - functions/ffs.aabox  →  mounted at `/dev/usb-ffs/aabox`
//!   - configs/c.1/ffs.aabox  symlink
//!
//! Before binding the UDC we unbind `g1` so only one gadget is active.
//!
//! # AOA protocol (handled here)
//!
//! The KIA sends three classes of USB control requests before starting AAP:
//!   1. GET_PROTOCOL (bRequest=51, device-to-host) → we reply `[0x00, 0x02]`
//!   2. SEND_STRING  (bRequest=52, host-to-device) → we read + log the string
//!   3. ACCESSORY_START (bRequest=53, host-to-device) → we open bulk endpoints
//!
//! On ACCESSORY_START we do **not** reset the USB bus. We open ep1/ep2 right
//! there on the existing connection and return them to the caller.

#![cfg(any(target_os = "linux", target_os = "android"))]

use anyhow::{Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::Duration;

// ── Paths ──────────────────────────────────────────────────────────────────

const GADGET_PATH: &str = "/config/usb_gadget/aabox";
const FFS_MOUNT:   &str = "/dev/usb-ffs/aabox";
const EP0:         &str = "/dev/usb-ffs/aabox/ep0";
const EP_OUT:      &str = "/dev/usb-ffs/aabox/ep1"; // bulk-OUT: host→device (we read)
const EP_IN:       &str = "/dev/usb-ffs/aabox/ep2"; // bulk-IN:  device→host (we write)
const G1_UDC:      &str = "/config/usb_gadget/g1/UDC";

// ── AOA request codes ──────────────────────────────────────────────────────

const AOA_GET_PROTOCOL: u8 = 51; // 0x33 — IN  (device-to-host), wLength=2
const AOA_SEND_STRING:  u8 = 52; // 0x34 — OUT (host-to-device), wLength=string_len
const AOA_START:        u8 = 53; // 0x35 — OUT (host-to-device), wLength=0

const USB_DIR_IN: u8 = 0x80;    // bRequestType bit 7: direction bit

// ── FunctionFS magic / flags ───────────────────────────────────────────────

const FFS_DESCS_MAGIC_V2: u32 = 3;
const FFS_STRINGS_MAGIC:  u32 = 2;
const FFS_HAS_FS_DESC:    u32 = 1 << 0;
const FFS_HAS_HS_DESC:    u32 = 1 << 1;

// ── FunctionFS event types ─────────────────────────────────────────────────

const FFS_BIND:    u8 = 0;
const FFS_UNBIND:  u8 = 1;
const FFS_ENABLE:  u8 = 2;
const FFS_DISABLE: u8 = 3;
const FFS_SETUP:   u8 = 4;

const FFS_EVENT_SIZE: usize = 12; // sizeof(struct usb_functionfs_event)

// ── USB descriptor builder ─────────────────────────────────────────────────

fn build_descriptors() -> Vec<u8> {
    // Interface: vendor class, 2 bulk endpoints
    let intf: [u8; 9] = [9, 4, 0, 0, 2, 0xFF, 0xFF, 0x00, 0];

    // Bulk-OUT (host→device): EP1, full-speed 64 B, high-speed 512 B
    let ep_out_fs: [u8; 7] = [7, 5, 0x01, 0x02, 0x40, 0x00, 0x00];
    let ep_out_hs: [u8; 7] = [7, 5, 0x01, 0x02, 0x00, 0x02, 0x00];

    // Bulk-IN (device→host): EP1 IN, full-speed 64 B, high-speed 512 B
    let ep_in_fs: [u8; 7]  = [7, 5, 0x81, 0x02, 0x40, 0x00, 0x00];
    let ep_in_hs: [u8; 7]  = [7, 5, 0x81, 0x02, 0x00, 0x02, 0x00];

    let fs: Vec<u8> = [intf.as_ref(), ep_out_fs.as_ref(), ep_in_fs.as_ref()].concat();
    let hs: Vec<u8> = [intf.as_ref(), ep_out_hs.as_ref(), ep_in_hs.as_ref()].concat();

    // Header: magic(4) + length(4) + flags(4) + fs_count(4) + hs_count(4) = 20 B
    let total = 20u32 + fs.len() as u32 + hs.len() as u32;

    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&FFS_DESCS_MAGIC_V2.to_le_bytes());
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&(FFS_HAS_FS_DESC | FFS_HAS_HS_DESC).to_le_bytes());
    out.extend_from_slice(&3u32.to_le_bytes()); // fs_count
    out.extend_from_slice(&3u32.to_le_bytes()); // hs_count
    out.extend_from_slice(&fs);
    out.extend_from_slice(&hs);
    out
}

fn build_strings() -> Vec<u8> {
    // Empty strings (16-byte header, no entries)
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&FFS_STRINGS_MAGIC.to_le_bytes());
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // str_count
    out.extend_from_slice(&0u32.to_le_bytes()); // lang_count
    out
}

// ── Gadget / configfs helpers ──────────────────────────────────────────────

fn udc_name() -> Result<String> {
    fs::read_dir("/sys/class/udc")
        .context("read /sys/class/udc")?
        .filter_map(|e| e.ok())
        .next()
        .and_then(|e| e.file_name().into_string().ok())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no UDC found in /sys/class/udc"))
}

fn write_file(path: &Path, value: &str) -> Result<()> {
    fs::write(path, value).with_context(|| format!("write {}", path.display()))
}

fn setup_configfs_gadget() -> Result<()> {
    let g = Path::new(GADGET_PATH);
    fs::create_dir_all(g).context("create gadget dir")?;

    write_file(&g.join("idVendor"),  "0x18d1\n")?;
    write_file(&g.join("idProduct"), "0x2D00\n")?;
    write_file(&g.join("bcdDevice"), "0x0200\n")?;

    let s = g.join("strings/0x409");
    fs::create_dir_all(&s).ok();
    write_file(&s.join("manufacturer"), "Google, Inc.")?;
    write_file(&s.join("product"),      "Android Auto")?;
    write_file(&s.join("serialnumber"), "000000aabox001")?;

    let cfg = g.join("configs/c.1");
    fs::create_dir_all(&cfg).ok();
    let cs = cfg.join("strings/0x409");
    fs::create_dir_all(&cs).ok();
    write_file(&cs.join("configuration"), "Accessory")?;

    let func = g.join("functions/ffs.aabox");
    if !func.exists() {
        fs::create_dir_all(&func).context("create ffs.aabox function")?;
    }

    let link = cfg.join("ffs.aabox");
    if !link.exists() {
        std::os::unix::fs::symlink(&func, &link)
            .context("symlink ffs.aabox into config")?;
    }

    tracing::info!("configfs gadget 'aabox' ready");
    Ok(())
}

fn mount_functionfs() -> Result<()> {
    fs::create_dir_all(FFS_MOUNT).ok();

    if Path::new(EP0).exists() {
        tracing::debug!("functionfs already mounted at {FFS_MOUNT}");
        return Ok(());
    }

    nix::mount::mount(
        Some("aabox"),
        FFS_MOUNT,
        Some("functionfs"),
        nix::mount::MsFlags::empty(),
        None::<&str>,
    )
    .context("mount -t functionfs aabox /dev/usb-ffs/aabox")?;

    tracing::info!("functionfs mounted at {FFS_MOUNT}");
    Ok(())
}

/// Set an Android system property by invoking the `setprop` binary.
/// We use this to trigger init.rc property actions rather than writing
/// configfs files directly — init's writes bypass the re-bind race that
/// hits us when adbd re-signals sys.usb.ffs.ready after we touch g1/UDC.
fn setprop(key: &str, value: &str) -> Result<()> {
    let status = std::process::Command::new("setprop")
        .args([key, value])
        .status()
        .with_context(|| format!("spawn setprop {key} {value}"))?;
    if !status.success() {
        anyhow::bail!("setprop {key} {value} → {:?}", status.code());
    }
    Ok(())
}

/// Switch sys.usb.config to "aabox" so that the init.rc fragment
/// (aabox-usb.rc) writes "none" to g1/UDC for us.  This keeps adbd
/// running (WiFi ADB survives) because only sys.usb.config=none calls
/// `stop adbd`.  We poll until g1 is actually unbound before returning.
fn release_g1_via_init() -> Result<()> {
    let g1_udc = Path::new(G1_UDC);
    if !g1_udc.exists() {
        return Ok(());
    }
    if fs::read_to_string(g1_udc).unwrap_or_default().trim().is_empty() {
        tracing::debug!("g1 already unbound");
        return Ok(());
    }

    tracing::info!("requesting g1 UDC release via sys.usb.config=aabox");
    setprop("sys.usb.config", "aabox")?;

    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        let val = fs::read_to_string(g1_udc).unwrap_or_default();
        if val.trim().is_empty() || val.trim() == "none" {
            tracing::info!("g1 UDC released");
            return Ok(());
        }
    }
    anyhow::bail!("g1 UDC did not release after 4s — is aabox-usb.rc installed?")
}

fn bind_gadget(udc: &str) -> Result<()> {
    let udc_path = Path::new(GADGET_PATH).join("UDC");
    let current = fs::read_to_string(&udc_path).unwrap_or_default();
    if current.trim() == udc {
        return Ok(());
    }
    // By the time we get here, g1 should be unbound. Retry briefly for the
    // kernel's async gadget teardown to complete.
    let mut last_err = None;
    for attempt in 0..10 {
        match fs::write(&udc_path, format!("{udc}\n")) {
            Ok(_) => {
                tracing::info!(udc, "aabox gadget bound");
                return Ok(());
            }
            Err(e) if e.raw_os_error() == Some(16) => {
                tracing::debug!(attempt, "UDC busy, retrying in 200ms");
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e).context("bind aabox gadget to UDC"),
        }
    }
    Err(last_err.unwrap()).context("bind aabox gadget to UDC: still busy after retries")
}

/// Restore normal ADB mode after the aabox session ends.  Called on both
/// clean exit and error paths so g1 + adbd USB are always restored.
pub fn restore_g1() {
    tracing::info!("restoring sys.usb.config=adb");
    let _ = setprop("sys.usb.config", "adb");
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Bulk endpoint pair returned after a successful AOA handshake.
pub struct FfsEndpoints {
    /// bulk-OUT (host → device): read AAP frames from here.
    pub ep_out: OwnedFd,
    /// bulk-IN  (device → host): write AAP frames here.
    pub ep_in: OwnedFd,
}

/// Set up the FunctionFS gadget and block until the KIA completes the AOA
/// handshake (`ACCESSORY_START`). Returns the bulk endpoint pair for the
/// caller to use as the AAP transport — no USB reset happens.
///
/// Safe to call again after a disconnect; the gadget and mount survive.
pub fn setup_and_wait() -> Result<FfsEndpoints> {
    let udc = udc_name()?;
    tracing::info!(udc, "setting up FunctionFS gadget");

    setup_configfs_gadget()?;
    mount_functionfs()?;

    // Open ep0 — must precede UDC bind so the kernel can exchange setup with us
    let mut ep0 = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(EP0)
        .context("open ep0")?;

    ep0.write_all(&build_descriptors()).context("write descriptors to ep0")?;
    ep0.write_all(&build_strings()).context("write strings to ep0")?;
    tracing::info!("ep0 descriptors written");

    release_g1_via_init()?;
    bind_gadget(&udc)?;

    tracing::info!("waiting for AOA handshake from head unit");
    let mut buf = [0u8; FFS_EVENT_SIZE];

    loop {
        ep0.read_exact(&mut buf).context("read ep0 event")?;

        let ev = buf[8];
        match ev {
            FFS_BIND    => tracing::info!("ffs: BIND"),
            FFS_UNBIND  => tracing::warn!("ffs: UNBIND — host disconnected before AOA_START"),
            FFS_ENABLE  => tracing::info!("ffs: ENABLE — USB connection established"),
            FFS_DISABLE => tracing::info!("ffs: DISABLE"),

            FFS_SETUP => {
                let bm_request_type = buf[0];
                let b_request       = buf[1];
                let _w_value        = u16::from_le_bytes([buf[2], buf[3]]);
                let w_index         = u16::from_le_bytes([buf[4], buf[5]]);
                let w_length        = u16::from_le_bytes([buf[6], buf[7]]);

                tracing::debug!(
                    bm_request_type = format_args!("0x{:02x}", bm_request_type),
                    b_request,
                    w_length,
                    "ffs: SETUP"
                );

                match b_request {
                    AOA_GET_PROTOCOL => {
                        // Reply with AOA version 2 (little-endian u16)
                        ep0.write_all(&[0x00, 0x02]).context("send AOA version")?;
                        tracing::info!("AOA: GET_PROTOCOL → 2");
                    }

                    AOA_SEND_STRING => {
                        // Read string data sent by host in the data phase
                        let s = if w_length > 0 {
                            let mut data = vec![0u8; w_length as usize];
                            ep0.read_exact(&mut data).context("read SEND_STRING data")?;
                            String::from_utf8_lossy(&data)
                                .trim_end_matches('\0')
                                .to_string()
                        } else {
                            String::new()
                        };
                        tracing::info!(index = w_index, value = %s, "AOA: SEND_STRING");
                    }

                    AOA_START => {
                        tracing::info!("AOA: ACCESSORY_START — opening bulk endpoints (no USB reset)");

                        // Status phase: zero-length write acknowledges the control request
                        let _ = ep0.write_all(&[]);

                        let ep_out_file = fs::OpenOptions::new()
                            .read(true)
                            .open(EP_OUT)
                            .context("open ep_out (ep1)")?;
                        let ep_in_file = fs::OpenOptions::new()
                            .write(true)
                            .open(EP_IN)
                            .context("open ep_in (ep2)")?;

                        return Ok(FfsEndpoints {
                            ep_out: OwnedFd::from(ep_out_file),
                            ep_in:  OwnedFd::from(ep_in_file),
                        });
                    }

                    other => {
                        tracing::warn!(b_request = other, "AOA: unrecognised SETUP — ignoring");
                        // For host-to-device requests with data we must drain the data phase
                        if (bm_request_type & USB_DIR_IN) == 0 && w_length > 0 {
                            let mut drain = vec![0u8; w_length as usize];
                            let _ = ep0.read_exact(&mut drain);
                        }
                    }
                }
            }

            other => tracing::debug!(event_type = other, "ffs: unrecognised event"),
        }
    }
}

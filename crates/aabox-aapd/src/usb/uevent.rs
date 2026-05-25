//! Kernel uevent listener — watches for `f_accessory` signaling that the host
//! finished the AOAv2 handshake and we should switch to accessory mode.
//!
//! The signal we care about:
//! ```text
//! ACTION=change
//! DEVNAME=usb_accessory
//! ACCESSORY=START
//! ```
//!
//! Runs on its own blocking thread (netlink-sys is sync); fires
//! `accessory_started.notify_one()` when the signal arrives.

#![cfg(any(target_os = "linux", target_os = "android"))]

use anyhow::{Context, Result};
use netlink_sys::{protocols::NETLINK_KOBJECT_UEVENT, Socket, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Simple uevent parser — splits NUL-separated key=value records and looks for
/// the two fields we care about. Robust against malformed/extra fields that
/// would trip up the `kobject-uevent` crate's strict numeric parser.
fn is_accessory_start(packet: &[u8]) -> bool {
    let mut has_devname = false;
    let mut has_accessory_start = false;
    for record in packet.split(|&b| b == 0) {
        // First record is the header `<action>@<devpath>` (no '='); skip it.
        let Some(eq_pos) = record.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = &record[..eq_pos];
        let val = &record[eq_pos + 1..];
        if key == b"DEVNAME" && val == b"usb_accessory" {
            has_devname = true;
        } else if key == b"ACCESSORY" && val == b"START" {
            has_accessory_start = true;
        }
    }
    has_devname && has_accessory_start
}

pub fn run(accessory_started: Arc<Notify>) -> Result<()> {
    let mut socket = Socket::new(NETLINK_KOBJECT_UEVENT).context("netlink socket")?;
    // Group 1 = kernel uevents (group 2+ are reserved for udev). Multicast group
    // is encoded as a bitmask: `1 << (group - 1)` = 1.
    socket
        .bind(&SocketAddr::new(std::process::id(), 1))
        .context("bind netlink socket")?;

    tracing::info!("uevent listener up");

    let mut buf = vec![0u8; 8192];
    loop {
        match socket.recv(&mut buf, 0) {
            Ok(n) => {
                if is_accessory_start(&buf[..n]) {
                    tracing::info!("ACCESSORY=START received");
                    accessory_started.notify_one();
                }
            }
            Err(e) => {
                tracing::warn!("netlink recv error: {e}; brief backoff");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

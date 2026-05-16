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
use kobject_uevent::UEvent;
use netlink_sys::{protocols::NETLINK_KOBJECT_UEVENT, Socket, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

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
            Ok(_n) => match UEvent::from_netlink_packet(&buf) {
                Ok(ev) => {
                    let is_start = ev
                        .env
                        .get("DEVNAME")
                        .is_some_and(|v| v == "usb_accessory")
                        && ev.env.get("ACCESSORY").is_some_and(|v| v == "START");
                    if is_start {
                        tracing::info!("ACCESSORY=START received");
                        accessory_started.notify_one();
                    }
                }
                Err(e) => tracing::warn!("malformed uevent: {e}"),
            },
            Err(e) => {
                tracing::warn!("netlink recv error: {e}; brief backoff");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

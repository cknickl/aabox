//! USB transport layer for the AAP source daemon.
//!
//! Linux/Android-only. The car (host) drives the AOAv2 handshake via the
//! kernel `f_accessory` driver; our userspace job is:
//!
//! 1. Ensure two ConfigFS gadgets exist on the device: `default` (any plausible
//!    Android-phone-looking gadget — pre-handshake) and `accessory` (the AOAv2
//!    gadget exposing bulk in/out endpoints). See `tools/usb-gadget-setup.sh`.
//! 2. Enable the `default` gadget so the car sees us as a phone.
//! 3. Wait for the kernel uevent with `DEVNAME=usb_accessory` and
//!    `ACCESSORY=START` — that's the handshake's terminal signal.
//! 4. Disable `default`, enable `accessory`, then open `/dev/usb_accessory`.
//! 5. Read/write that fd to exchange AAP framed messages with the car.

#![cfg(any(target_os = "linux", target_os = "android"))]

pub mod gadget;
pub mod stream;
pub mod uevent;

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

use gadget::UsbGadgetState;

/// One-shot bringup: enable default → wait for ACCESSORY=START → switch to accessory.
///
/// Returns once accessory mode is live and `/dev/usb_accessory` is openable.
pub async fn bring_up(state: &mut UsbGadgetState) -> Result<()> {
    let accessory_started = Arc::new(Notify::new());

    // The uevent listener runs on a dedicated blocking thread because netlink
    // sockets in `netlink-sys` are sync-only.
    let notify = accessory_started.clone();
    std::thread::Builder::new()
        .name("aabox-uevent".into())
        .spawn(move || {
            if let Err(e) = uevent::run(notify) {
                tracing::error!("uevent listener exited: {e:#}");
            }
        })?;

    state.disable_all()?;
    state.enable("default")?;
    tracing::info!("default gadget enabled; waiting for ACCESSORY=START");

    // Cap the wait so we don't hang forever if the car never enters AOA mode.
    let waited = tokio::time::timeout(
        Duration::from_secs(60),
        accessory_started.notified(),
    )
    .await;

    match waited {
        Ok(()) => tracing::info!("got ACCESSORY=START uevent"),
        Err(_) => anyhow::bail!("timed out after 60s waiting for ACCESSORY=START"),
    }

    state.disable("default")?;
    tokio::time::sleep(Duration::from_millis(500)).await; // let the host re-enumerate
    state.enable("accessory")?;
    tracing::info!("switched to accessory gadget; /dev/usb_accessory should be live");

    Ok(())
}

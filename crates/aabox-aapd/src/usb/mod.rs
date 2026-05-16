//! USB transport layer for the AAP source daemon.
//!
//! Two distinct platforms are supported, with very different ceremonies:
//!
//! ### Android (the AABox target)
//!
//! Android's init.rc has already brought up a composite USB gadget (`g1`)
//! containing `accessory.gs2` along with adb / mtp / ptp / rndis / midi. The
//! UDC is permanently bound to that gadget. When a host (car) plugs into the
//! slave USB-C and issues AOAv2 control transfers, the kernel's `f_accessory`
//! driver responds and (on `ACCESSORY_START`) makes `/dev/usb_accessory`
//! readable.
//!
//! All our daemon does on Android is:
//!   1. Open `/dev/usb_accessory` (succeeds immediately even without a car).
//!   2. Read — blocks until the car triggers `ACCESSORY_START`, then bytes
//!      flow.
//!
//! No UDC management, no gadget creation. `wait_for_accessory()` is the entry
//! point.
//!
//! ### Plain Linux (e.g. a generic SBC where we control init)
//!
//! No pre-staged gadget exists. We have to:
//!   1. Run `tools/usb-gadget-setup.sh` once at boot to create the gadgets.
//!   2. Bind `default` to the UDC.
//!   3. Listen for the `ACCESSORY=START` uevent.
//!   4. Switch UDC from `default` to `accessory`.
//!   5. Open `/dev/usb_accessory`.
//!
//! That dance lives in [`gadget`] + [`uevent`] and the obsolete
//! [`bring_up_with_gadget_swap`] function — kept for the plain-Linux path.

#![cfg(any(target_os = "linux", target_os = "android"))]

pub mod gadget;
pub mod stream;
pub mod uevent;

use anyhow::Result;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Android path: open the kernel-managed `/dev/usb_accessory` and return its
/// fd. The open succeeds before accessory mode is active; reads on the fd
/// block until the car drives the AOAv2 handshake to completion.
///
/// Also spawns a uevent listener that logs `ACCESSORY=START` when it fires —
/// useful for diagnostics, not required for correctness.
pub async fn wait_for_accessory() -> Result<OwnedFd> {
    let notify = Arc::new(Notify::new());
    let nclone = notify.clone();
    std::thread::Builder::new()
        .name("aabox-uevent".into())
        .spawn(move || {
            if let Err(e) = uevent::run(nclone) {
                tracing::warn!("uevent listener exited: {e:#}");
            }
        })?;

    let fd = stream::open()?;
    tracing::info!(?fd, "/dev/usb_accessory opened — waiting for host AOAv2 handshake");

    // Cosmetic: log if we see ACCESSORY=START within the next minute. Reads
    // on the fd will start returning bytes when the kernel finishes the
    // handshake regardless of whether we caught the uevent.
    tokio::select! {
        _ = notify.notified() => tracing::info!("ACCESSORY=START uevent observed"),
        _ = tokio::time::sleep(Duration::from_secs(60)) => {
            tracing::info!("60s elapsed without ACCESSORY=START — that's fine; reads will block until a host plugs in");
        }
    }

    Ok(fd)
}

/// Plain-Linux path (no pre-staged Android composite gadget): bring up
/// `default`, wait for `ACCESSORY=START`, switch to `accessory`. Kept around
/// for the day someone wants to run AABox on a vanilla SBC.
pub async fn bring_up_with_gadget_swap(state: &mut gadget::UsbGadgetState) -> Result<()> {
    let accessory_started = Arc::new(Notify::new());
    let nclone = accessory_started.clone();
    std::thread::Builder::new()
        .name("aabox-uevent".into())
        .spawn(move || {
            if let Err(e) = uevent::run(nclone) {
                tracing::error!("uevent listener exited: {e:#}");
            }
        })?;

    state.disable_all()?;
    state.enable("default")?;
    tracing::info!("default gadget enabled; waiting for ACCESSORY=START");

    tokio::time::timeout(
        Duration::from_secs(60),
        accessory_started.notified(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timed out after 60s waiting for ACCESSORY=START"))?;

    state.disable("default")?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    state.enable("accessory")?;
    tracing::info!("switched to accessory gadget");
    Ok(())
}

//! Bulk endpoint stream — once accessory mode is live, `/dev/usb_accessory` is
//! a regular char device backed by the kernel f_accessory driver. Reads pull
//! bytes from the host (car), writes push to it. AAP framing (channel ID + len
//! + protobuf body) is layered on top by the codec module (Phase 3).

#![cfg(any(target_os = "linux", target_os = "android"))]

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub const ACCESSORY_DEV: &str = "/dev/usb_accessory";

/// Open the accessory device read+write. O_CLOEXEC is set so child processes
/// (if any) won't inherit it. The kernel returns EAGAIN before accessory mode
/// is live, so callers should retry on EAGAIN/ENODEV with backoff.
pub fn open() -> Result<OwnedFd> {
    open_at(Path::new(ACCESSORY_DEV))
}

pub fn open_at(path: &Path) -> Result<OwnedFd> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    Ok(OwnedFd::from(f))
}

// Pull libc through nix's transitive dependency. Adding it directly to
// aabox-aapd's deps wasn't necessary — nix re-exports the constant we need.
mod libc {
    pub use ::nix::libc::O_CLOEXEC;
}

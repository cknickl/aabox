//! ConfigFS gadget UDC management.
//!
//! Assumes the gadget descriptors themselves (`default/`, `accessory/`) have
//! already been created under `/sys/kernel/config/usb_gadget/` by the boot-time
//! setup script (`tools/usb-gadget-setup.sh`).
//!
//! This module only manages the **UDC binding** — writing the UDC name to a
//! gadget's `UDC` file enables it (kernel attaches the gadget to a USB device
//! controller); writing an empty line detaches it.

#![cfg(any(target_os = "linux", target_os = "android"))]

use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Candidate configfs roots, ordered by preference. Android mounts configfs at
/// `/config` directly; generic Linux distros mount it at `/sys/kernel/config`.
pub const CONFIGFS_CANDIDATES: &[&str] = &[
    "/config/usb_gadget",
    "/sys/kernel/config/usb_gadget",
];
pub const UDC_DIR: &str = "/sys/class/udc";

fn discover_configfs() -> Result<PathBuf> {
    for c in CONFIGFS_CANDIDATES {
        let p = Path::new(c);
        if p.exists() {
            return Ok(p.to_path_buf());
        }
    }
    Err(anyhow!(
        "no configfs usb_gadget directory found (tried: {:?})",
        CONFIGFS_CANDIDATES
    ))
}

pub struct UsbGadgetState {
    configfs: PathBuf,
    udc_name: String,
}

impl UsbGadgetState {
    /// Auto-detect both the configfs root and the UDC.
    pub fn autodetect() -> Result<Self> {
        let configfs = discover_configfs()?;
        tracing::info!(path = %configfs.display(), "configfs root");
        let entries = fs::read_dir(UDC_DIR)
            .with_context(|| format!("read_dir({UDC_DIR})"))?;
        let first = entries
            .filter_map(|e| e.ok())
            .next()
            .ok_or_else(|| anyhow!("no UDC available under {UDC_DIR} — kernel/dtoverlay issue?"))?;
        let udc_name = first
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("UDC name is not valid UTF-8"))?;
        tracing::info!(udc = %udc_name, "autodetected UDC");
        Ok(Self {
            configfs,
            udc_name,
        })
    }

    /// Explicit UDC name (use for testing or when multiple controllers exist).
    /// configfs root still auto-detected.
    pub fn with_udc(udc_name: impl Into<String>) -> Result<Self> {
        Ok(Self {
            configfs: discover_configfs()?,
            udc_name: udc_name.into(),
        })
    }

    /// Enable a gadget by name (binds it to the UDC).
    pub fn enable(&self, gadget_name: &str) -> Result<()> {
        let udc_path = self.gadget_udc_path(gadget_name);
        if read_trimmed(&udc_path)?.is_some() {
            tracing::debug!(gadget = gadget_name, "already enabled");
            return Ok(());
        }
        fs::write(&udc_path, self.udc_name.as_bytes())
            .with_context(|| format!("write {} ← {}", udc_path.display(), self.udc_name))?;
        tracing::info!(gadget = gadget_name, "enabled");
        Ok(())
    }

    /// Disable a gadget by name (writes empty string to its UDC file).
    pub fn disable(&self, gadget_name: &str) -> Result<()> {
        let udc_path = self.gadget_udc_path(gadget_name);
        if read_trimmed(&udc_path)?.is_none() {
            tracing::debug!(gadget = gadget_name, "already disabled");
            return Ok(());
        }
        fs::write(&udc_path, "\n")
            .with_context(|| format!("write empty → {}", udc_path.display()))?;
        tracing::info!(gadget = gadget_name, "disabled");
        Ok(())
    }

    /// Disable both default and accessory (best-effort, ignores missing gadgets).
    pub fn disable_all(&self) -> Result<()> {
        for g in ["default", "accessory"] {
            if self.gadget_udc_path(g).exists() {
                let _ = self.disable(g);
            }
        }
        Ok(())
    }

    fn gadget_udc_path(&self, gadget_name: &str) -> PathBuf {
        self.configfs.join(gadget_name).join("UDC")
    }
}

fn read_trimmed(path: &Path) -> Result<Option<String>> {
    let s = fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let t = s.trim_end_matches(|c: char| c == '\n' || c == '\r' || c == ' ');
    Ok(if t.is_empty() { None } else { Some(t.to_string()) })
}

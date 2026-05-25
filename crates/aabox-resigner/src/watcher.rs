//! Daemon-mode file watcher.
//!
//! Watches `/sdcard/Download` (configurable) for `*.apk` close-write events,
//! filters by name pattern (`carcar*.apk`), processes the APK through the
//! signer, then `pm install`s the result over the existing package.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{process_apk, CARCAR_LAUNCHER_PKG};

/// Entry point for `aabox-resigner --watch`.
///
/// Blocks forever, processing any `carcar*.apk` that appears in `dir`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn watch(dir: &Path, key: &Path, cert: &Path) -> Result<()> {
    use inotify::{Inotify, WatchMask};

    if !dir.exists() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating watch dir {}", dir.display()))?;
    }
    if !dir.is_dir() {
        bail!("watch dir is not a directory: {}", dir.display());
    }

    let mut inotify = Inotify::init().context("inotify::init")?;
    inotify
        .watches()
        .add(
            dir,
            WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::CREATE,
        )
        .with_context(|| format!("inotify add_watch {}", dir.display()))?;
    tracing::info!(dir = %dir.display(), "watching for CarCar APK drops");

    let mut buffer = [0u8; 4096];
    loop {
        let events = inotify
            .read_events_blocking(&mut buffer)
            .context("read_events_blocking")?;
        for ev in events {
            let Some(name) = ev.name else { continue };
            let name_str = name.to_string_lossy();
            if !is_carcar_apk_filename(&name_str) {
                continue;
            }
            let path = dir.join(name);
            tracing::info!(file = %path.display(), "candidate APK detected");
            if let Err(e) = handle_one(&path, key, cert) {
                tracing::error!(error = %e, file = %path.display(), "failed to handle APK");
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn watch(_dir: &Path, _key: &Path, _cert: &Path) -> Result<()> {
    bail!("watch mode only supported on Linux/Android (inotify)");
}

/// Single-shot pipeline: process the APK to a `.patched.apk` in the same dir,
/// then `pm install` it.
pub fn handle_one(input: &Path, key: &Path, cert: &Path) -> Result<()> {
    // Some downloaders write atomically (rename from `.partial` → `.apk`)
    // which already fires close_write only once, and some chunked downloaders
    // close-write multiple times. Be conservative: only process if the file
    // has a sane minimum size and is a valid zip (locate_zip_layout succeeds).
    let metadata = std::fs::metadata(input)
        .with_context(|| format!("stat {}", input.display()))?;
    if metadata.len() < 1024 * 1024 {
        bail!(
            "ignoring {} — too small ({} bytes), likely an in-progress download",
            input.display(),
            metadata.len()
        );
    }

    let parent = input.parent().unwrap_or_else(|| Path::new("."));
    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("apk");
    let patched: PathBuf = parent.join(format!("{}.aabox-patched.apk", stem));

    let report = process_apk(input, &patched, key, cert)
        .with_context(|| format!("process_apk on {}", input.display()))?;
    tracing::info!(?report, output = %patched.display(), "patched");

    if report.sdk_int_patches == 0 {
        tracing::warn!(
            "no SDK_INT patches applied — this APK might not need patching. Installing anyway."
        );
    }

    pm_install(&patched, CARCAR_LAUNCHER_PKG)?;
    // Best-effort: remove the patched intermediate after successful install.
    let _ = std::fs::remove_file(&patched);
    Ok(())
}

fn pm_install(apk: &Path, package: &str) -> Result<()> {
    tracing::info!(apk = %apk.display(), package, "invoking pm install");
    let output = Command::new("pm")
        .arg("install")
        .arg("--pkg")
        .arg(package)
        .arg("-r")
        .arg(apk)
        .output()
        .context("spawning pm")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        bail!(
            "pm install failed (status={:?}): stdout={} stderr={}",
            output.status.code(),
            stdout,
            stderr
        );
    }
    tracing::info!(stdout = %stdout, "pm install succeeded");
    Ok(())
}

fn is_carcar_apk_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("carcar")
        && lower.ends_with(".apk")
        && !lower.ends_with(".aabox-patched.apk") // skip our own outputs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_match() {
        assert!(is_carcar_apk_filename("carcar-3.3.10-default.apk"));
        assert!(is_carcar_apk_filename("CarCar_4.0.apk"));
        assert!(!is_carcar_apk_filename("carcar.aabox-patched.apk"));
        assert!(!is_carcar_apk_filename("other.apk"));
        assert!(!is_carcar_apk_filename("carcar.partial"));
    }
}

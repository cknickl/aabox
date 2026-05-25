//! On-device CarCar Launcher re-signer.
//!
//! Background
//! ----------
//! CarCar Launcher (the Android Auto launcher we use in the AABox) ships with
//! hard-coded checks that refuse to run on Android API ≥ 36. We work around
//! that by rewriting three `sget Landroid/os/Build$VERSION;->SDK_INT:I`
//! instructions in `classes.dex` to `const/16 vAA, 0x23` (decimal 35).
//!
//! Phase 1 we did that manually with baksmali / smali / apksigner on a laptop.
//! Phase 1.5 (this crate) automates it on-device so future CarCar updates land
//! without a developer touching anything.
//!
//! Operational model
//! -----------------
//! * `aabox-resigner --watch` runs as an init service. It watches
//!   `/sdcard/Download/` (where the CarCar in-app updater drops its `*.apk`
//!   blobs) with inotify and processes any new APK by:
//!   1. unzip → patch every `sget …SDK_INT` instruction in each `classes*.dex`
//!      to `const/16 vAA, 0x23`,
//!   2. recompute the DEX adler32 + sha-1 header,
//!   3. repack the APK (preserving STORED entries — alignment matters for
//!      Android's mmap of `lib/*.so`),
//!   4. strip any v2/v3 signing block + META-INF/CERT.* + META-INF/MANIFEST.MF
//!      and re-sign with the AABox platform key (v2 only — see note below),
//!   5. `pm install --pkg com.example.car_launcher <patched.apk>`.
//!
//! * `aabox-resigner --input X.apk --output Y.apk --key K --cert C` is the
//!   offline CLI form, useful for dev / CI and for the test fixture.
//!
//! Architecture notes
//! ------------------
//! Pure Rust. We deliberately do not bundle dalvikvm / smali / apksigner jars:
//!
//! * They are tens of megabytes
//! * The CM5 doesn't ship dalvikvm anyway (`/system/bin/dalvikvm` absent)
//! * The DEX rewrite is a fixed-width same-length opcode swap — no relocation,
//!   no instruction-stream parsing. We only walk the `field_id_table` to find
//!   the SDK_INT field index, then scan the data section for `sget` /
//!   `sget-wide` etc. referencing that index and overwrite in place.
//! * APK Signature Scheme v2 is well-specified; ring covers SHA-256 + RSA.
//!
//! v3 signing is not implemented — Android 14+ verifies APKs that have only v2
//! and our CM5 is on AOSP-16 userdebug which does accept v2. If a future
//! Android release stops accepting v2-only on system installs we will need v3
//! key-rotation support; we will revisit then.

pub mod apk;
pub mod dex;
pub mod sign;
pub mod watcher;

use std::path::Path;
use std::time::Instant;

use anyhow::Context;

/// Default location of the AABox platform private key on the CM5.
pub const DEFAULT_KEY_PATH: &str = "/vendor/aabox-keys/platform.pk8";
/// Default location of the AABox platform X.509 cert (PEM) on the CM5.
pub const DEFAULT_CERT_PATH: &str = "/vendor/aabox-keys/platform.x509.pem";

/// CarCar Launcher application package (the actual app, signed by AABox key).
pub const CARCAR_LAUNCHER_PKG: &str = "com.example.car_launcher";

/// CarCar in-app updater package — this is the Flutter app that downloads
/// new launcher APKs into `/sdcard/Download/`.
pub const CARCAR_INSTALLER_PKG: &str = "com.carcarlauncher.installer";

/// Default file-watcher directory — the public Downloads location the
/// `com.carcarlauncher.installer` Flutter app writes to. Confirmed on the CM5
/// (rev 2026-05-17) — `/sdcard/Download/carcar-3.3.10-default.apk`.
pub const DEFAULT_WATCH_DIR: &str = "/sdcard/Download";

/// Default log file location for daemon mode.
pub const DEFAULT_LOG_PATH: &str = "/data/local/tmp/aabox-resigner.log";

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// End-to-end: read `input`, patch + re-sign, write `output`.
///
/// This is the single function both the CLI and the daemon call.
pub fn process_apk(
    input: &Path,
    output: &Path,
    key_path: &Path,
    cert_path: &Path,
) -> anyhow::Result<ProcessReport> {
    let started = Instant::now();
    tracing::info!(input = %input.display(), output = %output.display(), "process_apk start");

    let signer = sign::Signer::load(key_path, cert_path)
        .with_context(|| format!("loading signer from {} + {}", key_path.display(), cert_path.display()))?;

    let apk_bytes = std::fs::read(input)
        .with_context(|| format!("reading {}", input.display()))?;

    // Step 1: extract → patch DEX entries → repack into a fresh ZIP without
    // any META-INF signing artefacts.
    let (repacked, patch_stats) = apk::repack_and_patch(&apk_bytes)
        .context("repacking + patching APK")?;

    // Step 2: re-sign with APK Signature Scheme v2.
    let signed = sign::sign_apk_v2(&repacked, &signer)
        .context("re-signing APK with v2 scheme")?;

    std::fs::write(output, &signed)
        .with_context(|| format!("writing {}", output.display()))?;

    let report = ProcessReport {
        input_bytes: apk_bytes.len(),
        output_bytes: signed.len(),
        sdk_int_patches: patch_stats.sdk_int_patches,
        dex_files_patched: patch_stats.dex_files_patched,
        elapsed_ms: started.elapsed().as_millis() as u64,
    };

    tracing::info!(?report, "process_apk done");
    Ok(report)
}

#[derive(Debug, Clone)]
pub struct ProcessReport {
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub sdk_int_patches: usize,
    pub dex_files_patched: usize,
    pub elapsed_ms: u64,
}

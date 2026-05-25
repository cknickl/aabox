//! APK (zip) manipulation. Two responsibilities:
//!
//! 1. **Unzip + DEX patch + repack**:
//!    - Read every entry from the input zip.
//!    - Drop everything in `META-INF/` that is signing-related
//!      (`MANIFEST.MF`, `CERT.SF`, `CERT.RSA`, `CERT.DSA`, `CERT.EC`),
//!      keeping per-library version files etc.
//!    - For every `classes*.dex` entry, patch the DEX (see `crate::dex`).
//!    - Write a fresh zip preserving the original compression method per
//!      entry and the alignment requirements of `lib/*.so` (Android mmap's
//!      them, so they must be page-aligned and STORED).
//!
//! 2. Locate the central directory + EOCD in the *signed* output. The signer
//!    uses those offsets to insert an APK Signing Block.
//!
//! We use the `zip` crate for read+write but bypass it for alignment by
//! emitting STORED entries via `zip::write::SimpleFileOptions` with
//! `with_alignment(4096)` so that `lib/*.so` stays page-aligned.

use anyhow::{Context, Result};
use std::io::{Cursor, Read, Seek, Write};
use zip::{
    write::{SimpleFileOptions, ZipWriter},
    CompressionMethod, ZipArchive,
};

use crate::dex;

#[derive(Debug, Default, Clone)]
pub struct PatchStats {
    pub sdk_int_patches: usize,
    pub dex_files_patched: usize,
}

/// Read `apk_bytes`, drop existing signing artefacts, patch SDK_INT in every
/// classes*.dex, and write a fresh (unsigned) APK. Returns the new bytes plus
/// patch statistics.
pub fn repack_and_patch(apk_bytes: &[u8]) -> Result<(Vec<u8>, PatchStats)> {
    let mut stats = PatchStats::default();

    let cursor = Cursor::new(apk_bytes);
    let mut archive = ZipArchive::new(cursor).context("opening input apk as zip")?;

    let mut out = Vec::with_capacity(apk_bytes.len());
    {
        let mut writer = ZipWriter::new(Cursor::new(&mut out));

        // Iterate in stored order to preserve any compression / alignment
        // optimisations the original build pipeline performed.
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).context("reading zip entry")?;
            let name = entry.name().to_string();

            if is_signing_artefact(&name) {
                tracing::debug!(name = %name, "dropping signing artefact");
                continue;
            }

            let original_method = entry.compression();
            let mut data = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut data).with_context(|| {
                format!("reading entry data {} ({} bytes uncompressed)", name, entry.size())
            })?;

            // Patch classes*.dex.
            if is_classes_dex(&name) {
                let r = dex::patch_dex(&mut data)
                    .with_context(|| format!("patching {}", name))?;
                if r.patches > 0 {
                    stats.dex_files_patched += 1;
                    stats.sdk_int_patches += r.patches;
                    tracing::info!(
                        name = %name,
                        patches = r.patches,
                        bytes = r.bytes_rewritten,
                        "patched DEX"
                    );
                } else {
                    tracing::debug!(name = %name, "no SDK_INT references found");
                }
            }

            write_entry(&mut writer, &name, &data, original_method)?;
        }

        writer.finish().context("finalising zip")?;
    }

    Ok((out, stats))
}

fn is_signing_artefact(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if !upper.starts_with("META-INF/") {
        return false;
    }
    upper.ends_with("/MANIFEST.MF")
        || upper.ends_with("/CERT.SF")
        || upper.ends_with("/CERT.RSA")
        || upper.ends_with("/CERT.DSA")
        || upper.ends_with("/CERT.EC")
        // Some signers use other base names (e.g., "BNDLTOOL.SF", "PLATFORM.RSA").
        // Strip any .SF/.RSA/.DSA/.EC at the top level of META-INF/.
        || (upper.split('/').count() == 2
            && (upper.ends_with(".SF")
                || upper.ends_with(".RSA")
                || upper.ends_with(".DSA")
                || upper.ends_with(".EC")
                || upper == "META-INF/MANIFEST.MF"))
}

fn is_classes_dex(name: &str) -> bool {
    if !name.starts_with("classes") || !name.ends_with(".dex") {
        return false;
    }
    // classes.dex, classes2.dex, classes3.dex, ...
    let middle = &name["classes".len()..name.len() - ".dex".len()];
    middle.is_empty() || middle.chars().all(|c| c.is_ascii_digit())
}

fn write_entry<W: Write + Seek>(
    writer: &mut ZipWriter<W>,
    name: &str,
    data: &[u8],
    original_method: CompressionMethod,
) -> Result<()> {
    // Decide compression policy:
    //   * If the original was STORED, keep STORED (this is the case for
    //     resources.arsc, *.png, and especially lib/*.so — Android requires
    //     these to be stored uncompressed for mmap).
    //   * If the original was DEFLATE, use DEFLATE.
    //   * Anything else (BZIP2, ZSTD, ...) → keep STORED to be safe; APK
    //     spec only allows STORED and DEFLATE anyway.
    let use_method = match original_method {
        CompressionMethod::Deflated => CompressionMethod::Deflated,
        _ => CompressionMethod::Stored,
    };

    // Alignment: STORED entries in lib/*.so must be 4096-aligned for Android
    // 14+ (zipalign -p 4 16k). Other STORED entries get 4-byte alignment.
    // The `zip` crate's `with_alignment` operates on the local file header
    // offset of the entry payload, which is what Android cares about.
    let alignment: u16 = if matches!(use_method, CompressionMethod::Stored) {
        if name.starts_with("lib/") && name.ends_with(".so") {
            4096
        } else {
            4
        }
    } else {
        // Deflate entries don't need alignment (Android doesn't mmap them).
        1
    };

    let options = SimpleFileOptions::default()
        .compression_method(use_method)
        .large_file(data.len() > u32::MAX as usize)
        .last_modified_time(zip::DateTime::default())
        .with_alignment(alignment);
    writer
        .start_file(name, options)
        .with_context(|| format!("start_file {}", name))?;
    writer
        .write_all(data)
        .with_context(|| format!("writing data for {}", name))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_artefact_detection() {
        assert!(is_signing_artefact("META-INF/MANIFEST.MF"));
        assert!(is_signing_artefact("META-INF/CERT.SF"));
        assert!(is_signing_artefact("META-INF/CERT.RSA"));
        assert!(is_signing_artefact("META-INF/PLATFORM.RSA"));
        assert!(is_signing_artefact("META-INF/BNDLTOOL.SF"));
        assert!(!is_signing_artefact("META-INF/androidx.core_core.version"));
        assert!(!is_signing_artefact(
            "META-INF/com/android/build/gradle/app-metadata.properties"
        ));
        assert!(!is_signing_artefact("classes.dex"));
    }

    #[test]
    fn classes_dex_detection() {
        assert!(is_classes_dex("classes.dex"));
        assert!(is_classes_dex("classes2.dex"));
        assert!(is_classes_dex("classes99.dex"));
        assert!(!is_classes_dex("classesfoo.dex"));
        assert!(!is_classes_dex("lib/classes.dex"));
        assert!(!is_classes_dex("classes.dex.bak"));
    }
}

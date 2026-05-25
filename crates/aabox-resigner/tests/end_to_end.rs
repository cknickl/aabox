//! Full pipeline test: synthetic APK → process_apk → verify output.

mod common;

use std::io::{Cursor, Read, Write};
use std::path::PathBuf;

use aabox_resigner::process_apk;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

#[test]
fn full_pipeline_patch_and_sign() {
    let dex = common::build_synthetic_dex();

    // Build a synthetic unsigned APK containing classes.dex + a deflated
    // sibling resource so the central directory has >1 entry.
    let mut apk_buf = Vec::new();
    {
        let mut w = ZipWriter::new(Cursor::new(&mut apk_buf));
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        w.start_file("classes.dex", opts).unwrap();
        w.write_all(&dex).unwrap();
        let opts2 = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        w.start_file("META-INF/version-control-info.textproto", opts2)
            .unwrap();
        w.write_all(b"# placeholder").unwrap();
        w.finish().unwrap();
    }

    let tmp = tempfile::tempdir().unwrap();
    let in_apk: PathBuf = tmp.path().join("in.apk");
    let out_apk: PathBuf = tmp.path().join("out.apk");
    let key_path: PathBuf = tmp.path().join("test.pk8");
    let cert_path: PathBuf = tmp.path().join("test.x509.pem");
    std::fs::write(&in_apk, &apk_buf).unwrap();

    // Throwaway 2048-bit RSA key + self-signed cert generated specifically
    // for this test. Not used for anything real.
    let key_b64 = include_str!("data/test_key.pk8.b64").trim();
    let cert_pem = include_str!("data/test_cert.x509.pem");
    std::fs::write(&key_path, base64_decode(key_b64)).unwrap();
    std::fs::write(&cert_path, cert_pem).unwrap();

    let report =
        process_apk(&in_apk, &out_apk, &key_path, &cert_path).expect("process_apk should succeed");
    assert_eq!(report.sdk_int_patches, 1, "expected 1 SDK_INT patch");
    assert_eq!(report.dex_files_patched, 1);

    let out_bytes = std::fs::read(&out_apk).unwrap();
    let mut archive = ZipArchive::new(Cursor::new(&out_bytes)).expect("output is a valid zip");
    let mut found_dex = false;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).unwrap();
        if entry.name() == "classes.dex" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).unwrap();
            let const16 = [0x13u8, 0x00, 0x23, 0x00];
            assert!(
                buf.windows(4).any(|w| w == const16),
                "patched classes.dex must contain const/16 v0, #0x23"
            );
            assert!(
                !buf.windows(4).any(|w| w == [0x60u8, 0x00, 0x00, 0x00]),
                "patched classes.dex must NOT contain the original sget"
            );
            found_dex = true;
        }
    }
    assert!(found_dex, "output APK must contain classes.dex");

    assert!(
        out_bytes
            .windows(16)
            .any(|w| w == b"APK Sig Block 42"),
        "output APK must contain an APK Signing Block"
    );
}

fn base64_decode(input: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in input.chars() {
        if c.is_whitespace() {
            continue;
        }
        let v: u32 = match c {
            'A'..='Z' => (c as u32) - ('A' as u32),
            'a'..='z' => (c as u32) - ('a' as u32) + 26,
            '0'..='9' => (c as u32) - ('0' as u32) + 52,
            '+' => 62,
            '/' => 63,
            '=' => break,
            _ => panic!("invalid base64 char: {:?}", c),
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    out
}

//! Unit test: run the DEX patcher over a hand-built minimal DEX.

mod common;

use aabox_resigner::dex::{patch_dex, CLAMPED_SDK_INT};

#[test]
fn dex_patcher_rewrites_sget_to_const16() {
    let mut dex = common::build_synthetic_dex();
    let pre = dex.clone();
    let result = patch_dex(&mut dex).expect("patch_dex");
    assert_eq!(result.patches, 1, "expected exactly one sget rewrite");
    assert_eq!(result.bytes_rewritten, 4);

    // The synthetic dex uses field_idx 0, so the sget bytes are
    // 0x60 0x00 0x00 0x00 — find that and verify rewrite.
    let pre_idx = pre
        .windows(4)
        .position(|w| w == [0x60, 0x00, 0x00, 0x00])
        .expect("sget pattern in synthetic dex");
    assert_eq!(
        &dex[pre_idx..pre_idx + 4],
        &[0x13, 0x00, CLAMPED_SDK_INT as u8, 0x00],
        "instruction at sget position must be rewritten to const/16 v0, #35"
    );
}

#[test]
fn dex_patcher_recomputes_header_checksums() {
    let mut dex = common::build_synthetic_dex();
    let _ = patch_dex(&mut dex).expect("patch_dex");

    // SHA-1 of dex[32..] should be at dex[12..32]
    use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};
    let mut ctx = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(&dex[32..]);
    let want = ctx.finish();
    assert_eq!(&dex[12..32], want.as_ref(), "sha-1 header signature");

    // Adler-32 of dex[12..] at dex[8..12]
    let adler = adler32_check(&dex[12..]);
    let got = u32::from_le_bytes([dex[8], dex[9], dex[10], dex[11]]);
    assert_eq!(adler, got, "adler-32 header checksum");
}

fn adler32_check(buf: &[u8]) -> u32 {
    const MOD_ADLER: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in buf {
        a = (a + byte as u32) % MOD_ADLER;
        b = (b + a) % MOD_ADLER;
    }
    (b << 16) | a
}

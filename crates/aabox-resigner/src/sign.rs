//! APK Signature Scheme v2 implementation.
//!
//! Reference: <https://source.android.com/docs/security/features/apksigning/v2>
//!
//! We sign with RSA-PKCS1-v1_5 + SHA-256 (signature algorithm ID `0x0103`)
//! over a CHUNKED_SHA256 content digest. The AABox platform key is a 2048-bit
//! RSA key in PKCS#8 form, paired with an X.509 v3 self-signed cert.
//!
//! The algorithm:
//!
//!   1. Locate EOCD in the input (which is the *unsigned* APK after
//!      `apk::repack_and_patch`).
//!   2. Split the input into three regions:
//!       a. ZIP entries (before central directory)
//!       b. Central directory
//!       c. EOCD (last 22 + comment bytes; in our case no comment)
//!   3. For the digest computation only, rewrite the EOCD's "central directory
//!      offset" to the offset where the APK Signing Block will begin (i.e.,
//!      the size of region (a)). The actual EOCD on disk also has to be
//!      patched the same way when we emit the final APK.
//!   4. Compute CHUNKED_SHA256 over (a) || (b) || (modified-c).
//!   5. Build the v2 signer block, sign `signed_data` with RSA-PKCS1-v1_5
//!      SHA-256.
//!   6. Wrap in the outer APK Signing Block.
//!   7. Emit: (a) || signing_block || (b) || (patched c).

use anyhow::{anyhow, bail, Context, Result};
use ring::{
    digest,
    rand::SystemRandom,
    signature::{RsaKeyPair, RSA_PKCS1_SHA256},
};
use std::path::Path;

// ---- v2 scheme constants --------------------------------------------------

pub const APK_SIG_BLOCK_MAGIC: &[u8; 16] = b"APK Sig Block 42";
pub const APK_SIGNATURE_SCHEME_V2_BLOCK_ID: u32 = 0x7109_871a;
pub const VERITY_PADDING_BLOCK_ID: u32 = 0x4272_6577;
pub const SIG_ALG_RSA_PKCS1_V1_5_SHA256: u32 = 0x0103;
pub const ANDROID_COMMON_PAGE_ALIGNMENT_BYTES: usize = 4096;

const CONTENT_DIGESTED_CHUNK_MAX_SIZE_BYTES: usize = 1024 * 1024;

// ---- Signer ---------------------------------------------------------------

/// A loaded signing identity: RSA private key + DER-encoded X.509 cert + the
/// cert's SubjectPublicKeyInfo bytes (for the v2 signer block).
pub struct Signer {
    key_pair: RsaKeyPair,
    /// X.509 v3 cert in DER form.
    cert_der: Vec<u8>,
    /// SubjectPublicKeyInfo from the cert (DER).
    public_key_spki_der: Vec<u8>,
}

impl Signer {
    /// Load a signer from disk.
    ///
    /// * `key_path` — DER-encoded PKCS#8 private key (the typical AOSP
    ///   `platform.pk8` format).
    /// * `cert_path` — PEM-encoded X.509 v3 cert (typical AOSP
    ///   `platform.x509.pem`).
    pub fn load(key_path: &Path, cert_path: &Path) -> Result<Self> {
        let key_bytes = std::fs::read(key_path)
            .with_context(|| format!("reading key {}", key_path.display()))?;
        let cert_bytes = std::fs::read(cert_path)
            .with_context(|| format!("reading cert {}", cert_path.display()))?;
        Self::from_bytes(&key_bytes, &cert_bytes)
    }

    /// Same as [`Signer::load`] but in-memory.
    ///
    /// `key_bytes` is PKCS#8 DER. `cert_bytes` is PEM or DER (auto-detected).
    pub fn from_bytes(key_bytes: &[u8], cert_bytes: &[u8]) -> Result<Self> {
        // Parse PKCS#8 private key. The AOSP `platform.pk8` is a raw PKCS#8
        // (DER, RSA) blob, no encryption.
        let key_pair = RsaKeyPair::from_pkcs8(key_bytes)
            .map_err(|e| anyhow!("parsing PKCS#8 private key: {}", e))?;

        // Parse the cert. Accept either PEM (typical AOSP) or DER.
        let cert_der = if looks_like_pem(cert_bytes) {
            pem_to_der(cert_bytes).context("converting cert PEM → DER")?
        } else {
            cert_bytes.to_vec()
        };

        // Extract SubjectPublicKeyInfo. We use `x509-cert` to parse and then
        // re-encode the SPKI back to DER.
        use der::{Decode, Encode};
        let cert = x509_cert::Certificate::from_der(&cert_der)
            .map_err(|e| anyhow!("parsing X.509 cert: {}", e))?;
        let spki_der = cert
            .tbs_certificate
            .subject_public_key_info
            .to_der()
            .map_err(|e| anyhow!("re-encoding SPKI: {}", e))?;

        Ok(Self {
            key_pair,
            cert_der,
            public_key_spki_der: spki_der,
        })
    }

    fn sign_pkcs1_sha256(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut sig = vec![0u8; self.key_pair.public().modulus_len()];
        let rng = SystemRandom::new();
        self.key_pair
            .sign(&RSA_PKCS1_SHA256, &rng, data, &mut sig)
            .map_err(|e| anyhow!("RSA-PKCS1 sign: {}", e))?;
        Ok(sig)
    }
}

fn looks_like_pem(bytes: &[u8]) -> bool {
    bytes
        .windows(11)
        .any(|w| w == b"-----BEGIN ")
}

fn pem_to_der(pem_bytes: &[u8]) -> Result<Vec<u8>> {
    // Manual PEM parse: strip BEGIN/END lines, base64-decode the body.
    let text = std::str::from_utf8(pem_bytes).context("PEM not UTF-8")?;
    let mut in_body = false;
    let mut b64 = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("-----BEGIN ") {
            in_body = true;
            continue;
        }
        if trimmed.starts_with("-----END ") {
            break;
        }
        if in_body {
            b64.push_str(trimmed);
        }
    }
    if b64.is_empty() {
        bail!("PEM has no body");
    }
    base64_decode(&b64).context("base64 decoding PEM body")
}

// Minimal base64 decoder — avoids an extra dependency. PEM bodies are
// well-formed base64 with `+/=` alphabet.
fn base64_decode(input: &str) -> Result<Vec<u8>> {
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
            '=' => {
                break;
            }
            _ => bail!("invalid base64 character: {:?}", c),
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

// ---- ZIP structure helpers ------------------------------------------------

#[derive(Debug, Copy, Clone)]
struct ZipLayout {
    /// Offset of the start of the central directory.
    cd_offset: u32,
    /// Length of the central directory.
    cd_size: u32,
    /// Offset of the EOCD record (the 0x06054b50 magic).
    eocd_offset: usize,
}

fn locate_zip_layout(apk: &[u8]) -> Result<ZipLayout> {
    // EOCD signature is 0x06054b50, then we have at minimum 22 bytes total.
    // The signature can appear up to 65535 bytes before EOF (ZIP comment).
    let max_back = std::cmp::min(apk.len(), 22 + 65535);
    let scan_start = apk.len() - max_back;
    let mut found: Option<usize> = None;
    for i in (scan_start..apk.len().saturating_sub(21)).rev() {
        if &apk[i..i + 4] == b"\x50\x4b\x05\x06" {
            // Confirm by checking comment-length consistency.
            let comment_len = u16::from_le_bytes([apk[i + 20], apk[i + 21]]) as usize;
            if i + 22 + comment_len == apk.len() {
                found = Some(i);
                break;
            }
        }
    }
    let eocd_offset = found.ok_or_else(|| anyhow!("EOCD not found"))?;
    let cd_size = u32::from_le_bytes([
        apk[eocd_offset + 12],
        apk[eocd_offset + 13],
        apk[eocd_offset + 14],
        apk[eocd_offset + 15],
    ]);
    let cd_offset = u32::from_le_bytes([
        apk[eocd_offset + 16],
        apk[eocd_offset + 17],
        apk[eocd_offset + 18],
        apk[eocd_offset + 19],
    ]);
    Ok(ZipLayout {
        cd_offset,
        cd_size,
        eocd_offset,
    })
}

// ---- chunked SHA-256 (APK v2 content digest) ------------------------------

/// Compute the v2 CHUNKED_SHA256 digest over the three apk regions, with the
/// EOCD modified so that its central-directory offset points to where the
/// signing block will start.
///
/// The chunking is **per-region**: each DataSource (in apksig's terms) is
/// independently sliced into 1 MiB chunks, with the final chunk of each
/// region potentially shorter than 1 MiB. We then concatenate all
/// per-chunk digests and SHA-256 the result with a `0x5a || u32 num_chunks ||
/// digests…` framing.
fn compute_chunked_sha256(
    before_cd: &[u8],
    central_dir: &[u8],
    eocd_modified: &[u8],
) -> Vec<u8> {
    fn chunk_count(len: usize) -> usize {
        if len == 0 {
            0
        } else {
            (len + CONTENT_DIGESTED_CHUNK_MAX_SIZE_BYTES - 1)
                / CONTENT_DIGESTED_CHUNK_MAX_SIZE_BYTES
        }
    }

    let num_chunks =
        chunk_count(before_cd.len()) + chunk_count(central_dir.len()) + chunk_count(eocd_modified.len());

    let mut chunk_digests = Vec::with_capacity(1 + 4 + num_chunks * 32);
    chunk_digests.push(0x5a);
    chunk_digests.extend_from_slice(&(num_chunks as u32).to_le_bytes());

    for region in [before_cd, central_dir, eocd_modified] {
        let mut off = 0;
        while off < region.len() {
            let n = std::cmp::min(CONTENT_DIGESTED_CHUNK_MAX_SIZE_BYTES, region.len() - off);
            let mut ctx = digest::Context::new(&digest::SHA256);
            ctx.update(&[0xa5]);
            ctx.update(&(n as u32).to_le_bytes());
            ctx.update(&region[off..off + n]);
            let d = ctx.finish();
            chunk_digests.extend_from_slice(d.as_ref());
            off += n;
        }
    }

    let final_digest = digest::digest(&digest::SHA256, &chunk_digests);
    final_digest.as_ref().to_vec()
}

// ---- Length-prefixed builders --------------------------------------------
//
// These mirror the helpers in
// `com.android.apksig.internal.apk.ApkSigningBlockUtils` / `ApkSigningBlockUtilsLite`.

/// `encodeAsSequenceOfLengthPrefixedElements`:
/// `[u32:elem0.len][elem0][u32:elem1.len][elem1] ...`
/// No outer length prefix — the caller wraps as needed.
fn encode_lp_elements(elements: &[&[u8]]) -> Vec<u8> {
    let total: usize = elements.iter().map(|e| 4 + e.len()).sum();
    let mut out = Vec::with_capacity(total);
    for e in elements {
        out.extend_from_slice(&(e.len() as u32).to_le_bytes());
        out.extend_from_slice(e);
    }
    out
}

/// `encodeAsSequenceOfLengthPrefixedPairsOfIntAndLengthPrefixedBytes`:
/// for each (int_id, bytes):
///   `[u32: 8 + bytes.len][u32: int_id][u32: bytes.len][bytes...]`
fn encode_lp_int_lp_bytes_pairs(pairs: &[(u32, &[u8])]) -> Vec<u8> {
    let total: usize = pairs.iter().map(|(_id, b)| 12 + b.len()).sum();
    let mut out = Vec::with_capacity(total);
    for (id, b) in pairs {
        let pair_size_excl_self = 8 + b.len() as u32;
        out.extend_from_slice(&pair_size_excl_self.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(b);
    }
    out
}

// ---- v2 signer block ------------------------------------------------------

/// Build the body of the APK Signature Scheme v2 block (i.e., the value after
/// the (length, ID) pair header in the outer signing block).
///
/// Format (from V2SchemeSigner.java):
///   length-prefixed sequence of signers (we have one)
///     signer:
///       length-prefixed signed_data
///         length-prefixed sequence of digests
///           pair: u32 sig_algo_id, length-prefixed digest bytes
///         length-prefixed sequence of certificates
///           cert_der (length-prefixed)
///         length-prefixed additional_attributes (empty when v3 disabled)
///         length-prefixed sequence of public-key params  *** see note ***
///       length-prefixed sequence of signatures
///         pair: u32 sig_algo_id, length-prefixed signature bytes
///       length-prefixed public key (X.509 SubjectPublicKeyInfo DER)
///
/// Note on the trailing zero-length sequence: AOSP's apksig emits a fourth
/// element after additionalAttributes — `new byte[0]` — which is reserved for
/// future use. We mirror that.
fn build_v2_block_body(signer: &Signer, content_digest: &[u8]) -> Result<Vec<u8>> {
    // ---- signedData ----
    // digests = sequence_of_(int_id, lp_bytes) pairs
    let digests_seq = encode_lp_int_lp_bytes_pairs(&[(
        SIG_ALG_RSA_PKCS1_V1_5_SHA256,
        content_digest,
    )]);

    // certificates = sequence_of_lp_elements over cert DER blobs
    let certs_seq = encode_lp_elements(&[signer.cert_der.as_slice()]);

    // additionalAttributes = empty bytes (v3 disabled)
    let additional_attrs: &[u8] = &[];

    // signedData = lp_elements([digests_seq, certs_seq, additional_attrs, empty])
    //   (the trailing empty element mirrors apksig's reserved `new byte[0]`)
    let signed_data = encode_lp_elements(&[
        digests_seq.as_slice(),
        certs_seq.as_slice(),
        additional_attrs,
        &[],
    ]);

    // ---- signature over signedData ----
    let signature = signer.sign_pkcs1_sha256(&signed_data)?;
    let signatures_seq = encode_lp_int_lp_bytes_pairs(&[(
        SIG_ALG_RSA_PKCS1_V1_5_SHA256,
        signature.as_slice(),
    )]);

    // ---- signer block ----
    // signer = lp_elements([signedData, signatures_seq, publicKey])
    let signer_block = encode_lp_elements(&[
        signed_data.as_slice(),
        signatures_seq.as_slice(),
        signer.public_key_spki_der.as_slice(),
    ]);

    // ---- outer v2 body ----
    // body = lp_elements([ lp_elements([signer_block]) ])
    let signers_seq = encode_lp_elements(&[signer_block.as_slice()]);
    let body = encode_lp_elements(&[signers_seq.as_slice()]);
    Ok(body)
}

/// Wrap a list of (id, value) pairs into an APK Signing Block.
///
/// Pads with a verity padding block so that the total block size is a
/// multiple of `ANDROID_COMMON_PAGE_ALIGNMENT_BYTES` (4096), which keeps the
/// central directory page-aligned for Android's mmap.
fn build_signing_block(pairs: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut blocks_size = 0;
    for (_id, v) in pairs {
        blocks_size += 8 + 4 + v.len(); // size(u64) + id(u32) + value
    }
    let header_footer = 8 + 8 + 16; // outer size + outer size again + magic
    let mut total = header_footer + blocks_size;
    let mut padding_pair: Option<Vec<u8>> = None;
    if total % ANDROID_COMMON_PAGE_ALIGNMENT_BYTES != 0 {
        let mut padding =
            ANDROID_COMMON_PAGE_ALIGNMENT_BYTES - (total % ANDROID_COMMON_PAGE_ALIGNMENT_BYTES);
        if padding < 12 {
            padding += ANDROID_COMMON_PAGE_ALIGNMENT_BYTES;
        }
        let mut p = Vec::with_capacity(padding);
        // pair = u64 size (excluding self), u32 id, value
        let pair_size = padding - 8; // size excludes the u64 size field itself
        p.extend_from_slice(&(pair_size as u64).to_le_bytes());
        p.extend_from_slice(&VERITY_PADDING_BLOCK_ID.to_le_bytes());
        let zeros_len = padding - 8 - 4;
        p.extend(std::iter::repeat(0u8).take(zeros_len));
        padding_pair = Some(p);
        total += padding;
    }

    let mut out = Vec::with_capacity(total);
    let outer_size_excl = (total - 8) as u64;
    out.extend_from_slice(&outer_size_excl.to_le_bytes());
    for (id, v) in pairs {
        let pair_size = 4 + v.len() as u64;
        out.extend_from_slice(&pair_size.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(v);
    }
    if let Some(p) = padding_pair {
        out.extend_from_slice(&p);
    }
    out.extend_from_slice(&outer_size_excl.to_le_bytes());
    out.extend_from_slice(APK_SIG_BLOCK_MAGIC);
    debug_assert_eq!(out.len(), total);
    out
}

// ---- Top-level entrypoint -------------------------------------------------

pub fn sign_apk_v2(apk_bytes: &[u8], signer: &Signer) -> Result<Vec<u8>> {
    let layout = locate_zip_layout(apk_bytes)?;

    // Split the apk: [before_cd .. cd .. eocd_with_trailer]
    let cd_start = layout.cd_offset as usize;
    let cd_end = cd_start + layout.cd_size as usize;
    if cd_end > layout.eocd_offset {
        bail!(
            "central directory extends past EOCD: cd_end={} eocd_offset={}",
            cd_end,
            layout.eocd_offset
        );
    }
    let before_cd_raw = &apk_bytes[..cd_start];
    let central_dir = &apk_bytes[cd_start..cd_end];
    let eocd = &apk_bytes[layout.eocd_offset..]; // 22 + comment bytes

    // Pad `before_cd` with zeros so the APK Signing Block starts at a
    // 4096-byte boundary. This is what apksig does via
    // `generateApkSigningBlockPadding`; both signer and verifier must agree
    // on these padding bytes (they're part of the digest input). Android's
    // verifier strips this padding by reading the signing block size and
    // walking backwards from CD start, so we don't need to record the
    // padding length anywhere — only that it's zero bytes.
    let pad_size = if before_cd_raw.len() % ANDROID_COMMON_PAGE_ALIGNMENT_BYTES != 0 {
        ANDROID_COMMON_PAGE_ALIGNMENT_BYTES
            - (before_cd_raw.len() % ANDROID_COMMON_PAGE_ALIGNMENT_BYTES)
    } else {
        0
    };
    let mut before_cd_padded = Vec::with_capacity(before_cd_raw.len() + pad_size);
    before_cd_padded.extend_from_slice(before_cd_raw);
    before_cd_padded.resize(before_cd_padded.len() + pad_size, 0);

    // The EOCD used for the digest has its CD offset rewritten to point at
    // the start of the (future) APK Signing Block, which is the size of the
    // *padded* before_cd region.
    let mut eocd_for_digest = eocd.to_vec();
    let new_cd_offset = before_cd_padded.len() as u32;
    eocd_for_digest[16..20].copy_from_slice(&new_cd_offset.to_le_bytes());

    let content_digest =
        compute_chunked_sha256(&before_cd_padded, central_dir, &eocd_for_digest);

    let v2_block_body = build_v2_block_body(signer, &content_digest)?;
    let signing_block = build_signing_block(&[(APK_SIGNATURE_SCHEME_V2_BLOCK_ID, v2_block_body)]);

    // Now assemble the final APK:
    //   before_cd_padded || signing_block || central_dir || patched_eocd
    let total_size =
        before_cd_padded.len() + signing_block.len() + central_dir.len() + eocd.len();
    let mut out = Vec::with_capacity(total_size);
    out.extend_from_slice(&before_cd_padded);
    out.extend_from_slice(&signing_block);
    out.extend_from_slice(central_dir);

    // The on-disk EOCD's CD offset must now reflect the actual location of
    // the central directory in the final file: it shifted right by
    // (pad_size + signing_block.len()) bytes from the unsigned input.
    let final_cd_offset = (before_cd_padded.len() + signing_block.len()) as u32;
    let mut patched_eocd = eocd.to_vec();
    patched_eocd[16..20].copy_from_slice(&final_cd_offset.to_le_bytes());
    out.extend_from_slice(&patched_eocd);

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decode_ascii() {
        // "Man" → "TWFu"
        assert_eq!(base64_decode("TWFu").unwrap(), b"Man");
        // "any carnal pleas" with newlines
        let s = "YW55IGNhcm5hbCBwbGVhcw==";
        assert_eq!(base64_decode(s).unwrap(), b"any carnal pleas");
    }

    #[test]
    fn locate_layout_empty_archive() {
        // Minimal EOCD: signature + 18 zero bytes
        let mut apk = Vec::new();
        apk.extend_from_slice(b"\x50\x4b\x05\x06");
        apk.extend_from_slice(&[0u8; 18]);
        let layout = locate_zip_layout(&apk).unwrap();
        assert_eq!(layout.eocd_offset, 0);
        assert_eq!(layout.cd_offset, 0);
        assert_eq!(layout.cd_size, 0);
    }

    #[test]
    fn lp_elements_format() {
        // Single element "abc" → 4-byte LE length + bytes.
        let out = encode_lp_elements(&[b"abc"]);
        assert_eq!(out, vec![3, 0, 0, 0, b'a', b'b', b'c']);
        // Two elements.
        let out = encode_lp_elements(&[b"hi", b""]);
        assert_eq!(out, vec![2, 0, 0, 0, b'h', b'i', 0, 0, 0, 0]);
    }

    #[test]
    fn lp_int_lp_bytes_pairs_format() {
        // (0x01020304, b"xyz") → [u32:11] [u32:0x01020304] [u32:3] "xyz"
        let out = encode_lp_int_lp_bytes_pairs(&[(0x01020304, b"xyz")]);
        assert_eq!(
            out,
            vec![
                11, 0, 0, 0,
                0x04, 0x03, 0x02, 0x01,
                3, 0, 0, 0,
                b'x', b'y', b'z',
            ]
        );
    }
}
